//! `nix-cache`: let a worker substitute from this host's S3 cache. Only
//! the s3:// substituters, the trusted keys and the AWS profiles they name
//! leave the host, over SSH (pinned host key).

use crate::ctx::{Ctx, Fail, R, outln};
use crate::jq::{self, get};
use crate::paint;
use crate::proc;
use crate::state;
use regex::Regex;
use serde_json::{Value, json};
use std::process::Command;

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| a.iter().map(jq::raw).collect())
        .unwrap_or_default()
}

fn unique(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v.dedup();
    v
}

/// The s3:// substituters, the AWS profiles they name and the trusted
/// public keys of this host's Nix. nix.conf can hold access-tokens and
/// netrc paths, and non-S3 caches are not reachable from OpenStack anyway.
pub fn settings(ctx: &Ctx) -> R<Value> {
    let out = Command::new("nix")
        .args(["config", "show", "--json"])
        .stderr(std::process::Stdio::null())
        .output();
    let cfg = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => {
            ctx.err("Could not read the Nix configuration (nix config show)");
            return Err(Fail(1));
        }
    };
    let cfg = state::parse(ctx, &cfg)?;
    let vals = |k: &str| strings(jq::alt(jq::path(&cfg, &[k, "value"]), &json!([])));
    let subs = unique(
        [vals("substituters"), vals("extra-substituters")]
            .concat()
            .into_iter()
            .filter(|s| s.starts_with("s3://"))
            .collect(),
    );
    let re = Regex::new(r"[?&]profile=(?<p>[^&]+)").expect("valid regex");
    let profiles = unique(
        subs.iter()
            .map(|s| {
                re.captures(s)
                    .map(|c| c["p"].to_string())
                    .unwrap_or_else(|| "default".to_string())
            })
            .collect(),
    );
    let keys = unique(
        [
            vals("trusted-public-keys"),
            vals("extra-trusted-public-keys"),
        ]
        .concat(),
    );
    Ok(json!({"substituters": subs, "profiles": profiles, "keys": keys}))
}

/// One profile section ("[name]" and its lines) of an AWS credentials file.
fn aws_profile_section(file: &str, profile: &str) -> String {
    let Ok(text) = std::fs::read_to_string(file) else {
        return String::new();
    };
    let want = format!("[{profile}]");
    let mut found = false;
    let mut out = Vec::new();
    for line in text.lines() {
        if line == want {
            found = true;
            out.push(line);
            continue;
        }
        if line.starts_with('[') {
            found = false;
        }
        if found {
            out.push(line);
        }
    }
    proc::trim_nl(&out.join("\n"))
}

fn ssh_capture(opts: &[String], login: &str, script: &str) -> String {
    Command::new("ssh")
        .args(opts)
        .arg(login)
        .arg(script)
        .stderr(std::process::Stdio::null())
        .output()
        .map(|o| proc::trim_nl(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default()
}

/// Push the settings to a worker. The JSON result, progress on stderr.
pub fn push(ctx: &Ctx, name: &str, dry: bool) -> R<String> {
    let settings = settings(ctx)?;
    let subs = strings(get(&settings, "substituters"));
    if subs.is_empty() {
        ctx.err("This host's Nix has no s3:// substituter; nothing to push");
        return Err(Fail(1));
    }
    let creds_file = crate::ctx::env_or(
        "AWS_SHARED_CREDENTIALS_FILE",
        &format!("{}/.aws/credentials", ctx.home),
    );
    let mut creds = String::new();
    for profile in strings(get(&settings, "profiles")) {
        let section = aws_profile_section(&creds_file, &profile);
        if section.is_empty() {
            ctx.err(&format!(
                "No [{profile}] profile in {creds_file} (named by an s3:// substituter)"
            ));
            return Err(Fail(1));
        }
        creds.push_str(&section);
        creds.push('\n');
    }
    let conf = format!(
        "# Pushed by rworker nix-cache from {} at {}.\nextra-substituters = {}\nextra-trusted-public-keys = {}",
        ctx.hostname,
        crate::time::utc_iso(),
        subs.join(" "),
        strings(get(&settings, "keys")).join(" ")
    );
    if dry {
        let v = jq::merge(
            &settings,
            &[("name", json!(name)), ("dry_run", json!(true))],
        );
        return Ok(jq::compact(&v));
    }

    let ip = state::worker_ip(ctx, name)?;
    let opts = state::ssh_opts(ctx, name);
    let login = format!("agent@{ip}");
    // NetBird may still be setting up the route to a fresh worker.
    for _ in 1..=10 {
        if super::up::probe(&opts, &login) {
            break;
        }
        proc::sleep_secs(3.0);
    }
    let profiles = get(&settings, "profiles").clone();
    let subs_v = get(&settings, "substituters").clone();
    // An image without this feature has no /run/rworker-nix (agent cannot
    // create it); say so instead of failing on a shell error.
    let has_dir = Command::new("ssh")
        .args(&opts)
        .arg(&login)
        .arg("test -d /run/rworker-nix")
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !has_dir {
        return Ok(jq::compact(&json!({
            "ok": false, "name": name, "daemon": "image-too-old", "reachable": false,
            "substituters": subs_v, "profiles": profiles,
        })));
    }
    let unit = "rworker-nix-config.service";
    let before = ssh_capture(
        &opts,
        &login,
        &format!("systemctl show -p ExecMainStartTimestampMonotonic --value {unit}"),
    );
    // Credentials first, nix.conf last: replacing nix.conf is what restarts
    // the daemon (path unit on the worker). The secret travels on stdin only.
    let code = proc::run_with_input(
        ctx,
        state::worker_ssh(&opts, &login).arg(
            "umask 077; cat > /run/rworker-nix/aws-credentials.new && mv /run/rworker-nix/aws-credentials.new /run/rworker-nix/aws-credentials",
        ),
        creds.as_bytes(),
    )?;
    crate::ctx::status(code).map_err(|_| Fail(1))?;
    let code = proc::run_with_input(
        ctx,
        state::worker_ssh(&opts, &login).arg(
            "umask 077; cat > /run/rworker-nix/nix.conf.new && mv /run/rworker-nix/nix.conf.new /run/rworker-nix/nix.conf",
        ),
        format!("{conf}\n").as_bytes(),
    )?;
    crate::ctx::status(code).map_err(|_| Fail(1))?;

    // Does the worker reach the bucket with what it was given?
    let url = &subs[0];
    let check = Command::new("ssh")
        .args(&opts)
        .arg(&login)
        .arg(format!(
            "AWS_SHARED_CREDENTIALS_FILE=/run/rworker-nix/aws-credentials timeout 60 nix store info --store {}",
            proc::shell_quote(url)
        ))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output();
    let verified = match check {
        Ok(o) if o.status.success() => true,
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            let lines: Vec<&str> = err.lines().collect();
            let tail = lines[lines.len().saturating_sub(3)..].join("\n");
            ctx.err(&format!("The worker cannot reach the S3 cache: {tail}"));
            false
        }
        Err(_) => false,
    };

    // Did its nix-daemon pick the settings up (restarted by the path unit)?
    let mut daemon = "timeout";
    if ssh_capture(
        &opts,
        &login,
        &format!("systemctl show -p LoadState --value {unit}"),
    ) != "loaded"
    {
        daemon = "image-too-old";
    } else {
        for _ in 1..=20 {
            let after = ssh_capture(
                &opts,
                &login,
                &format!(
                    "systemctl show -p ExecMainStartTimestampMonotonic --value {unit}; systemctl is-active nix-daemon.socket"
                ),
            )
            .split('\n')
            .collect::<Vec<_>>()
            .join(" ");
            let first = after.split(' ').next().unwrap_or("");
            if first != before && after.ends_with(" active") {
                daemon = "restarted";
                break;
            }
            proc::sleep_secs(1.0);
        }
    }
    Ok(jq::compact(&json!({
        "ok": verified && daemon == "restarted",
        "name": name, "daemon": daemon, "reachable": verified,
        "substituters": subs_v, "profiles": profiles,
    })))
}

pub fn cmd_nix_cache(ctx: &Ctx, args: &[String]) -> R {
    let mut name = String::new();
    let mut dry = false;
    for a in args.iter().take_while(|a| !a.is_empty()) {
        match a.as_str() {
            "--dry-run" | "-n" => dry = true,
            s if s.starts_with('-') => {
                ctx.err(&format!("Unknown option: {s}"));
                crate::help::show(ctx, "nix-cache");
                return Err(Fail(2));
            }
            s => name = s.to_string(),
        }
    }
    if name.is_empty() {
        crate::help::show(ctx, "nix-cache");
        return Err(Fail(2));
    }
    let out = push(ctx, &name, dry).map_err(|_| Fail(1))?;
    if ctx.json.get() {
        outln(&out);
        return Ok(());
    }
    let v: Value = serde_json::from_str(&out).unwrap_or(Value::Null);
    if dry {
        outln(&format!("Would push to {}:", paint::vm(ctx, &name)));
        for s in strings(get(&v, "substituters")) {
            outln(&format!("  substituter {s}"));
        }
        for p in strings(get(&v, "profiles")) {
            outln(&format!("  profile     {p}"));
        }
        let n = get(&v, "keys").as_array().map_or(0, Vec::len);
        outln(&format!("  trusted keys {n}"));
        return Ok(());
    }
    match get(&v, "daemon").as_str() {
        Some("restarted") => paint::ok(
            ctx,
            &format!("{} substitutes from the S3 cache", paint::vm(ctx, &name)),
        ),
        Some("image-too-old") => {
            ctx.err(&format!(
                "{} {} runs an image from before --nix-cache existed: nothing was pushed (create a new worker)",
                paint::warn_mark(ctx),
                paint::vm(ctx, &name)
            ));
            return Err(Fail(1));
        }
        _ => ctx.err(&format!(
            "{} pushed to {}, but its nix-daemon did not restart in time",
            paint::warn_mark(ctx),
            paint::vm(ctx, &name)
        )),
    }
    Ok(())
}
