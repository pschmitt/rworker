//! `__workers [ACTION]`: "NAME<TAB>DESCRIPTION" per worker for the shell
//! completions, from the ls cache. Never waits on the network: a cache
//! older than 30 s is refreshed in the background for the next TAB.

use crate::ctx::{Ctx, R, outln};
use crate::jq::{self, get};
use crate::table;
use serde_json::Value;
use std::process::{Command, Stdio};
use std::time::SystemTime;

fn refresh_in_background(ctx: &Ctx) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let _ = Command::new(exe)
        .arg("__ls-refresh")
        .env("RWORKER_HOST", &ctx.control_host)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

pub fn cmd_workers(ctx: &Ctx, action: &str) -> R {
    let cache = ctx.ls_cache();
    let age = std::fs::metadata(&cache)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .map_or(999, |d| d.as_secs());
    if age > 30 {
        refresh_in_background(ctx);
    }
    let Ok(text) = std::fs::read_to_string(&cache) else {
        let mut names: Vec<String> = std::fs::read_dir(&ctx.state_dir)
            .map(|d| {
                d.filter_map(Result::ok)
                    .filter_map(|e| e.file_name().to_str().map(String::from))
                    .filter(|f| !f.starts_with('.') && f.ends_with(".json"))
                    .map(|f| f.trim_end_matches(".json").to_string())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        for n in names {
            outln(&format!("{n}\t"));
        }
        return Ok(());
    };
    let Ok(list) = serde_json::from_str::<Value>(&text) else {
        return Ok(());
    };
    let now = crate::time::now() as f64;
    let me = ctx.owner();
    let adhoc_only = matches!(action, "extend" | "prune" | "topic");
    for w in jq::items(&list).unwrap_or_default() {
        if adhoc_only && get(w, "kind").as_str() != Some("adhoc") {
            continue;
        }
        let expiry = if get(w, "kind").as_str() == Some("pool") {
            Some(jq::raw(get(w, "pool_state")))
        } else if jq::gt_num(get(w, "grace_until"), now) {
            Some(format!(
                "grace {}",
                table::dur_pos(jq::as_f64(get(w, "grace_until")) - now)
            ))
        } else if jq::gt_num(get(w, "expires_at"), now) {
            Some(format!(
                "expires {}",
                table::dur_pos(jq::as_f64(get(w, "expires_at")) - now)
            ))
        } else if jq::gt_num(get(w, "expires_at"), 0.0) {
            Some("expired".to_string())
        } else {
            None
        };
        let owner = jq::or_empty(get(w, "owner"));
        let parts = [
            Some(get(w, "kind")).filter(|v| !v.is_null()).map(jq::raw),
            jq::truthy(get(w, "agent")).then(|| table::agent_label(ctx, get(w, "agent"))),
            Some(get(w, "topic")).filter(|v| !v.is_null()).map(jq::raw),
            expiry.filter(|e| e != "null"),
            (get(w, "status").as_str() != Some("ACTIVE"))
                .then(|| get(w, "status"))
                .filter(|v| !v.is_null())
                .map(jq::raw),
            (!owner.is_empty() && owner != me).then_some(owner),
        ];
        let desc: Vec<String> = parts
            .into_iter()
            .flatten()
            .filter(|p| !p.is_empty())
            .collect();
        outln(&format!(
            "{}\t{}",
            jq::raw(get(w, "name")),
            desc.join(" · ")
        ));
    }
    Ok(())
}
