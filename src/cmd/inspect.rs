//! `inspect` and `prune`.

use crate::ctx::{Ctx, Fail, R, outln};
use crate::jq::{self, get};
use crate::paint::{self, paint};
use crate::state;
use crate::table;
use serde_json::{Value, json};
use std::io::{BufRead, IsTerminal, Write};

/// "2026-11-04 14:13 (in 1h 59m)" for an epoch timestamp, "-" for 0.
fn fmt_time(at: &str) -> String {
    if at.is_empty() || at == "0" || at == "null" {
        return "-".to_string();
    }
    let at: i64 = at.split('.').next().unwrap_or("0").parse().unwrap_or(0);
    let delta = at - crate::time::now();
    if delta >= 0 {
        format!(
            "{} (in {}h {}m)",
            crate::time::date_hm(at),
            delta / 3600,
            delta % 3600 / 60
        )
    } else {
        let d = -delta;
        format!(
            "{} ({}h {}m ago)",
            crate::time::date_hm(at),
            d / 3600,
            d % 3600 / 60
        )
    }
}

fn label(ctx: &Ctx, name: &str, value: &str) {
    outln(&format!(
        "{} {value}",
        paint(ctx, "1", &format!("{:<14}", format!("{name}:")))
    ));
}

pub fn cmd_inspect(ctx: &Ctx, args: &[String]) -> R {
    let mut name = String::new();
    let mut boot = false;
    for a in args.iter().take_while(|a| !a.is_empty()) {
        match a.as_str() {
            "--boot" => boot = true,
            s if s.starts_with('-') => {
                ctx.err(&format!("Unknown option: {s}"));
                crate::help::show(ctx, "inspect");
                return Err(Fail(2));
            }
            s => name = s.to_string(),
        }
    }
    if name.is_empty() {
        ctx.err("Missing worker name");
        crate::help::show(ctx, "inspect");
        return Err(Fail(2));
    }
    let out = state::remote_ls(ctx)?;
    let list = state::parse(ctx, &out)?;
    let Some(info) = state::find(ctx, &list, &name)?.cloned() else {
        ctx.err(&format!("No worker named {}", paint::vm(ctx, &name)));
        return Err(Fail(1));
    };
    let mut boot_meta = Value::Null;
    if boot {
        let ip = state::worker_ip(ctx, &name)?;
        let opts = state::ssh_opts(ctx, &name);
        let user = state::worker_field(ctx, &name, "user").unwrap_or_default();
        let (code, text) = crate::proc::capture(
            ctx,
            state::worker_ssh(
                &[opts, vec!["-o".into(), "ConnectTimeout=10".into()]].concat(),
                &format!("{user}@{ip}"),
            )
            .args(["cat", "/run/rworker/metadata.json"]),
        )?;
        if code != 0 {
            return Err(Fail(1));
        }
        boot_meta = state::parse(ctx, &text).map_err(|_| Fail(1))?;
    }
    let info = if jq::truthy(&boot_meta) {
        jq::merge(&info, &[("boot_metadata", boot_meta.clone())])
    } else {
        info
    };
    if ctx.json.get() {
        crate::ctx::json_out(ctx, &jq::pretty(&info));
        return Ok(());
    }
    let f = |k: &str| jq::raw(get(&info, k));
    let or = |k: &str, d: &str| jq::or(get(&info, k), d);
    label(ctx, "Name", &paint::vm(ctx, &f("name")));
    label(ctx, "Kind", &f("kind"));
    label(ctx, "Status", &paint::state(ctx, &f("status")));
    label(ctx, "Owner", &or("owner", "-"));
    outln(&paint(ctx, "1", "Creator:"));
    let mut agent = table::agent_label(ctx, get(&info, "agent"));
    if ctx.color {
        agent = paint::agents(&agent);
    }
    let pid = or("agent_pid", "-");
    if pid != "-" {
        agent += &format!(" (PID {pid})");
    }
    label(ctx, "  Agent", &agent);
    label(ctx, "  Agent ID", &or("agent_id", "-"));
    label(ctx, "  Session", &or("agent_session", "-"));
    label(ctx, "  Topic", &or("topic", "-"));
    label(ctx, "  Cwd", &or("cwd", "-"));
    label(ctx, "Flavor", &or("flavor", "-"));
    for (key, title) in [("image_name", "Image"), ("image_id", "Image ID")] {
        if let Some(value) = info[key]
            .as_str()
            .or_else(|| info["metadata"][key].as_str())
            .filter(|value| !value.is_empty())
        {
            label(ctx, title, value);
        }
    }
    for (id, url, title) in [
        ("run_id", "run_url", "CI run"),
        ("job_id", "job_url", "CI job"),
    ] {
        if let Some(id) = info[id].as_u64() {
            let value = info[url]
                .as_str()
                .map_or_else(|| id.to_string(), |url| format!("{id} · {url}"));
            label(ctx, title, &value);
        }
    }
    label(ctx, "Zone", &or("availability_zone", "-"));
    label(
        ctx,
        "IP",
        &format!(
            "{} (NetBird connected: {})",
            jq::or(&table::ips_label(&info), "-"),
            f("connected")
        ),
    );
    label(ctx, "Host key", &or("host_key", "-"));
    label(ctx, "Created", &or("created_at", "-"));
    let expires_at = or("expires_at", "0");
    let mut expires = fmt_time(&expires_at);
    let left = expires_at
        .split('.')
        .next()
        .unwrap_or("0")
        .parse::<i64>()
        .unwrap_or(0)
        - crate::time::now();
    if expires_at == "0" {
    } else if left < 1800 {
        // Expired or under 30 minutes: bold red; under an hour: orange.
        expires = paint(ctx, "1;31", &expires);
    } else if left < 3600 {
        expires = paint(ctx, "38;5;208", &expires);
    }
    label(ctx, "Expires", &expires);
    let grace = or("grace_until", "0");
    if grace != "0" {
        label(
            ctx,
            "Grace",
            &format!(
                "until {}; last in use {}",
                fmt_time(&grace),
                fmt_time(&or("last_active", "0"))
            ),
        );
    }
    let pool_state = or("pool_state", "");
    if !pool_state.is_empty() {
        // Pool workers have no TTL: the autoscaler drains them when idle.
        label(
            ctx,
            "Pool state",
            &format!(
                "{pool_state} (last busy {})",
                fmt_time(&or("last_busy", "0"))
            ),
        );
    } else {
        label(ctx, "Max lifetime", &fmt_time(&or("max_lifetime_end", "0")));
    }
    outln(&paint(ctx, "1", "Metadata:"));
    if let Value::Object(m) = jq::alt(get(&info, "metadata"), &json!({})) {
        for (k, v) in m {
            if k != "host_key" {
                outln(&format!("  {k}={}", jq::raw(v)));
            }
        }
    }
    if boot {
        outln(&paint(
            ctx,
            "1",
            "Boot metadata (/run/rworker/metadata.json):",
        ));
        for line in jq::pretty(&boot_meta).lines() {
            outln(&format!("  {line}"));
        }
    }
    Ok(())
}

pub fn cmd_prune(ctx: &Ctx, args: &[String]) -> R {
    let (mut all, mut yes, mut dry) = (false, false, false);
    let mut names: Vec<String> = Vec::new();
    for a in args.iter().take_while(|a| !a.is_empty()) {
        match a.as_str() {
            "--all" => all = true,
            "-y" | "--yes" => yes = true,
            "-n" | "--dry-run" => dry = true,
            s if s.starts_with('-') => {
                ctx.err(&format!("Unknown option: {s}"));
                crate::help::show(ctx, "prune");
                return Err(Fail(2));
            }
            s => names.push(s.to_string()),
        }
    }
    let me = ctx.owner();
    let out = state::remote_ls(ctx)?;
    let list = state::parse(ctx, &out)?;
    let adhoc: Vec<&Value> = state::items(ctx, &list)?
        .into_iter()
        .filter(|w| get(w, "kind").as_str() == Some("adhoc"))
        .collect();
    let targets: Vec<(String, String)> = adhoc
        .into_iter()
        .filter(|w| {
            if !names.is_empty() {
                get(w, "name")
                    .as_str()
                    .is_some_and(|n| names.iter().any(|x| x == n))
            } else if all {
                true
            } else {
                get(w, "owner").as_str() == Some(me.as_str())
            }
        })
        .map(|w| (jq::raw(get(w, "name")), jq::or(get(w, "owner"), "-")))
        .collect();
    let json = ctx.json.get();
    if targets.is_empty() {
        outln(if json { "[]" } else { "Nothing to prune." });
        return Ok(());
    }
    let entry = |n: &str, o: &str, d: bool| json!({"name": n, "owner": o, "deleted": d});
    if json && dry {
        let v: Vec<Value> = targets.iter().map(|(n, o)| entry(n, o, false)).collect();
        crate::ctx::json_out(ctx, &jq::compact(&Value::Array(v)));
        return Ok(());
    }
    if !json {
        outln("Ad-hoc workers to delete:");
        let rows: Vec<String> = targets.iter().map(|(n, o)| format!("{n}\t{o}")).collect();
        for line in jq::column(&rows.join("\n")).lines() {
            outln(&format!("  {line}"));
        }
    }
    if dry {
        return Ok(());
    }
    if !yes {
        if !std::io::stdin().is_terminal() {
            ctx.err("Not a terminal: pass --yes to delete");
            return Err(Fail(2));
        }
        let mut e = std::io::stderr();
        let _ = write!(e, "Delete them? [y/N] ");
        let _ = e.flush();
        let mut answer = String::new();
        // EOF ended the bash client under set -e.
        if std::io::stdin().lock().read_line(&mut answer).unwrap_or(0) == 0 {
            return Err(Fail(1));
        }
        if !answer.starts_with(['y', 'Y']) {
            outln("Aborted.");
            return Err(Fail(1));
        }
    }
    let mut failed = false;
    let mut results = Vec::new();
    for (name, owner) in &targets {
        let remote = ["down", name, "--owner", &me, "--force"];
        if state::remote_quiet(ctx, &remote).is_ok_and(|c| c == 0) {
            state::forget(ctx, name);
            results.push(entry(name, owner, true));
            if !json {
                paint::ok(ctx, &format!("deleted {}", paint::vm(ctx, name)));
            }
        } else {
            failed = true;
            results.push(entry(name, owner, false));
        }
    }
    if json {
        crate::ctx::json_out(ctx, &jq::compact(&Value::Array(results)));
    }
    if failed { Err(Fail(1)) } else { Ok(()) }
}
