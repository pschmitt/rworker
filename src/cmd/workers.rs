//! `__workers [ACTION]`: name, description and display icon as TSV for the shell
//! completions, from the ls cache. Never waits on the network: a cache
//! older than 30 s is refreshed in the background for the next TAB.

use crate::ctx::{Ctx, R, outln};
use crate::jq::{self, get};
use crate::{github_profiles, pool_icon, table};
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

/// The cached `ls` answer, refreshed in the background when it is older
/// than 30 s (for the next TAB). None without a cache.
fn cached_ls(ctx: &Ctx) -> Option<String> {
    let cache = ctx.ls_cache();
    let age = std::fs::metadata(&cache)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .map_or(999, |d| d.as_secs());
    if age > 30 {
        refresh_in_background(ctx);
    }
    std::fs::read_to_string(&cache).ok()
}

pub fn cmd_workers(ctx: &Ctx, action: &str) -> R {
    let Some(text) = cached_ls(ctx) else {
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
            let worker = serde_json::json!({"name": n});
            candidate(ctx, &n, "", pool_icon::glyph(&worker));
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
        let expiry = if crate::table::is_pool(w) {
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
        candidate(
            ctx,
            &jq::raw(get(w, "name")),
            &desc.join(" · "),
            pool_icon::glyph(w),
        );
    }
    Ok(())
}

fn candidate(ctx: &Ctx, value: &str, description: &str, icon: &str) {
    let description = description.replace(char::is_control, " ");
    let icon = if ctx.icons.get() { icon } else { "" };
    outln(&format!("{value}\t{description}\t{icon}"));
}

/// A topic or agent as a regex that matches exactly that text.
fn regex_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if "\\.+*?()|[]{}^$".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `__values SET`: TSV value, description and icon from static profiles or
/// the cached worker list plus static filter values. The *-re sets escape
/// values for literal matches.
pub fn cmd_values(ctx: &Ctx, set: &str) -> R {
    let prefix = format!("values {set} ");
    let static_values = crate::help::SPEC
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_default()
        .split_whitespace();
    if set == "profile" {
        for value in static_values {
            let forge = github_profiles::from_name(value).map(|profile| profile.forge);
            candidate(ctx, value, "", pool_icon::provider_glyph(forge));
        }
        return Ok(());
    }
    let (field, regex) = match set {
        "topic" => ("topic", false),
        "topic-re" => ("topic", true),
        "agent" => ("agent", false),
        "agent-re" => ("agent", true),
        "id" => ("agent_id", false),
        "id-re" => ("agent_id", true),
        _ => return Ok(()),
    };
    let mut values: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for value in static_values {
        values.entry(value.to_owned()).or_default();
    }
    let list = cached_ls(ctx)
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .unwrap_or(Value::Null);
    for w in jq::items(&list).unwrap_or_default() {
        let v = jq::or_empty(get(w, field));
        if !v.is_empty() {
            values.entry(v).or_default().push(jq::raw(get(w, "name")));
        }
    }
    for (value, workers) in values {
        let desc = match workers.len() {
            0 => String::new(),
            1..=3 => workers.join(", "),
            n => format!("{}, ... ({n} workers)", workers[..2].join(", ")),
        };
        let icon = match field {
            "agent" => table::glyph(&value),
            "agent_id" => value
                .split_once(':')
                .map_or("", |(agent, _)| table::glyph(agent)),
            _ => "",
        };
        let value = if regex { regex_escape(&value) } else { value };
        // One value per line: a tab or newline in it would break the lines.
        let value = value.replace(['\t', '\n'], " ");
        candidate(ctx, &value, &desc, icon);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn escape() {
        assert_eq!(
            super::regex_escape("rbw (release) 1.2"),
            "rbw \\(release\\) 1\\.2"
        );
        assert_eq!(super::regex_escape("plain words"), "plain words");
    }
}
