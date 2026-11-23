//! `setup TOPIC NAME [OPTIONS]`: get a worker ready for a kind of work with
//! this host's settings or issue a worker-specific credential.

use crate::ctx::{Ctx, Fail, R};
use crate::github_profiles::Forge;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

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
        "nix" => super::nix_cache::cmd_setup_nix(ctx, &args[1..], "setup", Default::default()),
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
