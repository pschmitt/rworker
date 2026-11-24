//! `setup TOPIC NAME [OPTIONS]`: get a worker ready for a kind of work with
//! this host's settings or issue a worker-specific credential.

use crate::ctx::{Ctx, Fail, R};
use crate::github_profiles::Forge;
use serde_json::{Value, json};
use std::fs::File;
use std::path::Path;
use std::time::{Duration, Instant};

#[cfg(test)]
pub const NIX_TOPICS: &[&str] = &["cache", "auth", "builder", "upload-blob"];

/// The topics `setup` knows, for the help and the completion.
pub const TOPICS: &[&str] = &[
    "nix",
    "nix-cache",
    "nix-builder",
    "niks3-token",
    "register-log-file",
    "netbird",
    "tailscale",
];

pub fn cmd_setup(ctx: &Ctx, args: &[String]) -> R {
    let Some(topic) = args
        .first()
        .filter(|t| !t.is_empty() && !t.starts_with('-'))
    else {
        crate::help::show(ctx, "setup");
        return Err(Fail(2));
    };
    match topic.as_str() {
        "nix" => setup_nix(ctx, &args[1..]),
        "nix-cache" => super::nix_cache::cmd_nix_cache(ctx, &args[1..]),
        "nix-builder" => super::simple::cmd_builder(ctx, &args[1..]),
        "niks3-token" => super::simple::cmd_niks3_token(ctx, &args[1..]),
        "netbird" | "tailscale" => setup_mesh(ctx, topic, &args[1..]),
        "register-log-file" => {
            let mut remote = vec!["register".to_string()];
            remote.extend_from_slice(&args[1..]);
            super::jobs::jobs_remote(ctx, &remote)
        }
        other => {
            ctx.err(&format!(
                "Unknown setup topic: {other} (known: {})",
                TOPICS.join(", ")
            ));
            crate::help::show(ctx, "setup");
            Err(Fail(2))
        }
    }
}

fn setup_nix(ctx: &Ctx, args: &[String]) -> R {
    use super::nix_cache::{Opts, cmd_nix_cache, cmd_setup_nix};
    match args.first().map(String::as_str) {
        Some("cache") => cmd_nix_cache(ctx, &args[1..]),
        Some("auth") => cmd_setup_nix(
            ctx,
            &args[1..],
            "setup",
            Opts {
                cache: false,
                ..Opts::default()
            },
        ),
        Some("builder") => super::simple::cmd_builder(ctx, &args[1..]),
        Some("upload-blob") => upload_blobs(ctx, &args[1..]),
        Some("--") => cmd_setup_nix(ctx, &args[1..], "setup", Opts::default()),
        _ => cmd_setup_nix(ctx, args, "setup", Opts::default()),
    }
}

fn upload_blobs(ctx: &Ctx, args: &[String]) -> R {
    let Some((worker, args)) = args
        .split_first()
        .filter(|(worker, _)| !worker.starts_with('-'))
    else {
        crate::help::show(ctx, "setup");
        return Err(Fail(2));
    };
    let mut args = args.iter();
    let mut paths = Vec::new();
    let mut name = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--name" if name.is_none() => {
                name = Some(args.next().ok_or_else(|| {
                    ctx.err("--name needs a store name");
                    Fail(2)
                })?);
            }
            "--" => {
                paths.extend(args);
                break;
            }
            s if s.starts_with('-') => {
                ctx.err(&format!("Unexpected upload-blob option: {s}"));
                return Err(Fail(2));
            }
            _ => paths.push(arg),
        }
    }
    if paths.is_empty() || (name.is_some() && paths.len() != 1) {
        ctx.err("Provide at least one local file, or exactly one with --name");
        return Err(Fail(2));
    }

    let mut uploads = Vec::new();
    for path in paths {
        let store_name = name.map(String::as_str).unwrap_or_else(|| {
            Path::new(path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
        });
        if store_name.is_empty()
            || store_name.len() > 211
            || store_name.starts_with('.')
            || !store_name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"+-._?=".contains(&b))
        {
            ctx.err(&format!(
                "Invalid Nix store name: {store_name:?}; use --name"
            ));
            return Err(Fail(2));
        }
        // Check before opening so a FIFO cannot block the client.
        if !Path::new(path).is_file() {
            ctx.err(&format!("{path}: expected a local regular file"));
            return Err(Fail(2));
        }
        let (file, size) = File::open(path)
            .and_then(|file| {
                let size = file.metadata()?.len();
                Ok((file, size))
            })
            .map_err(|error| {
                ctx.err(&format!("{path}: {error}"));
                Fail(1)
            })?;
        uploads.push((store_name, file, size));
    }

    let mut results = Vec::new();
    for (name, file, size) in uploads {
        let script = format!(
            r#"set -eu
umask 077
stage=$(mktemp -d -- "$HOME/.rworker-blob.XXXXXXXXXX")
trap 'rm -rf -- "$stage"' EXIT
trap 'exit 1' HUP INT TERM
file="$stage"/{}
cat > "$file"
if [ "$(wc -c < "$file")" -ne {size} ]
then
  printf '%s\n' 'Blob upload was incomplete or the local file changed size' >&2
  exit 1
fi
nix-store --add-fixed sha256 "$file""#,
            crate::proc::shell_quote(name),
        );
        let mut cmd = super::files::ssh_command(ctx, &[worker.clone(), "-T".into(), script])?;
        let store_path = crate::proc::capture_ok(ctx, cmd.stdin(file))?;
        if ctx.json.get() {
            results.push(store_path);
        } else {
            crate::ctx::outln(&store_path);
        }
    }
    if ctx.json.get() {
        crate::ctx::json_out(ctx, &json!({ "paths": results }).to_string());
    }
    Ok(())
}

fn setup_mesh(ctx: &Ctx, provider: &str, args: &[String]) -> R {
    let [worker] = args else {
        crate::help::show(ctx, "setup");
        return Err(Fail(2));
    };
    let Some((forge, suffix)) = Forge::ALL.into_iter().find_map(|forge| {
        worker
            .strip_prefix(forge.prefix())
            .map(|suffix| (forge, suffix))
    }) else {
        ctx.err("On-demand mesh setup requires a GitHub Actions or GitLab CI worker");
        return Err(Fail(2));
    };
    let owner = ctx.owner();
    let setup = format!("{}-mesh-setup", forge.key());
    let out = crate::state::remote(ctx, &[setup.as_str(), suffix, owner.as_str(), provider])?;
    let response: Value = serde_json::from_str(&out).map_err(|_| Fail(1))?;
    let request = response["id"].as_str().ok_or(Fail(1))?;
    let status = format!("{}-mesh-status", forge.key());
    let deadline = Instant::now() + Duration::from_secs(300);
    while Instant::now() < deadline {
        let out = crate::state::remote(ctx, &[status.as_str(), suffix, owner.as_str(), request])?;
        let response: Value = serde_json::from_str(&out).map_err(|_| Fail(1))?;
        if !response["result"].is_null() {
            let Some(port) = response["result"]["port"]
                .as_u64()
                .filter(|port| (1024..=65535).contains(port))
            else {
                ctx.err(&format!("{provider} setup failed on {worker}"));
                return Err(Fail(1));
            };
            let proxy = format!("socks5h://127.0.0.1:{port}");
            if ctx.json.get() {
                crate::ctx::json_out(
                    ctx,
                    &json!({"name": worker, "provider": provider, "proxy": proxy}).to_string(),
                );
            } else {
                crate::ctx::outln(&proxy);
            }
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    ctx.err(&format!(
        "Timed out waiting for {provider} setup on {worker}"
    ));
    Err(Fail(1))
}
