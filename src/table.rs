//! The `ls` table (kubectl style), its colors, and the labels shared with
//! inspect and the completions.

use crate::ctx::{Ctx, Fail, R};
use crate::jq::{self, get};
use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

pub(crate) fn glyph(agent: &str) -> &'static str {
    match agent {
        "claude" => "\u{ec82}",
        "codex" => "\u{ec81}",
        "shell" => "\u{ea85}",
        "nixbot" | "nix build" => "\u{f313}",
        "copilot" => "\u{ec1e}",
        "gemini" | "agy" | "antigravity" => "\u{ec10}",
        "browser-daemon" => "\u{f02a0}",
        _ => "",
    }
}

/// "<glyph> agent" ("agent" without icons, "-" without an agent).
pub fn agent_label(ctx: &Ctx, a: &Value) -> String {
    let s = jq::or_empty(a);
    if s.is_empty() {
        "-".to_string()
    } else if ctx.icons {
        format!("{} {s}", glyph(&s))
    } else {
        s
    }
}

/// The worker's overlay addresses (NetBird, Tailscale), or its `ip`.
pub fn ips_label(w: &Value) -> Value {
    let parts: Vec<String> = [
        jq::path(w, &["ips", "netbird"]),
        jq::path(w, &["ips", "tailscale"]),
    ]
    .into_iter()
    .filter(|v| jq::truthy(v))
    .map(jq::raw)
    .collect();
    let joined = parts.join(" ");
    if joined.is_empty() {
        get(w, "ip").clone()
    } else {
        Value::String(joined)
    }
}

/// The table's durations: "42m", "1h9m", "1d2h", "expired" when negative.
pub fn dur(s: f64) -> String {
    if s < 0.0 {
        "expired".to_string()
    } else {
        dur_pos(s)
    }
}

/// The completions' durations (never negative there).
pub fn dur_pos(s: f64) -> String {
    if s < 3600.0 {
        format!("{}m", (s / 60.0).floor())
    } else if s < 86400.0 {
        format!(
            "{}h{}m",
            (s / 3600.0).floor(),
            (jq::modulo(s, 3600.0) / 60.0).floor()
        )
    } else {
        format!(
            "{}d{}h",
            (s / 86400.0).floor(),
            (jq::modulo(s, 86400.0) / 3600.0).floor()
        )
    }
}

fn opt(v: &Value) -> Value {
    if jq::or_empty(v).is_empty() && !matches!(v, Value::Number(_) | Value::Bool(true)) {
        Value::String("-".to_string())
    } else {
        v.clone()
    }
}

fn expiry(w: &Value, now: f64) -> String {
    let pool_state = get(w, "pool_state");
    if get(w, "kind").as_str() == Some("pool") && jq::truthy(pool_state) {
        let last_busy = get(w, "last_busy");
        if pool_state.as_str() == Some("idle") && jq::truthy(last_busy) {
            format!("idle {}", dur(now - jq::as_f64(last_busy)))
        } else {
            jq::raw(pool_state)
        }
    } else if jq::gt_num(get(w, "grace_until"), now) {
        format!("grace {}", dur(jq::as_f64(get(w, "grace_until")) - now))
    } else if jq::gt_num(get(w, "expires_at"), 0.0) {
        dur(jq::as_f64(get(w, "expires_at")) - now)
    } else {
        "-".to_string()
    }
}

pub(crate) const COLUMNS: [&str; 6] = ["NAME", "STATUS", "AGENT", "TOPIC", "EXPIRES", "AGE"];
pub(crate) const WIDE_COLUMNS: [&str; 3] = ["OWNER", "PROFILE", "IP"];

/// kubectl-style table from `ls` JSON (aligned, uncolored); empty when
/// there are no workers.
pub fn plain(ctx: &Ctx, list: &Value) -> R<String> {
    let now = crate::time::now() as f64;
    let wide = ctx.wide.get();
    let mut header = COLUMNS.to_vec();
    if wide {
        header.extend(WIDE_COLUMNS);
    }
    let mut rows = vec![header.join("\t")];
    let Some(mut workers) = jq::items(list) else {
        ctx.err(&format!(
            "jq: error (at <stdin>:0): Cannot index {} with \"kind\"",
            jq::raw(list)
        ));
        return Err(Fail(5));
    };
    workers.sort_by(|a, b| {
        jq::truthy(get(a, "retired"))
            .cmp(&jq::truthy(get(b, "retired")))
            .then_with(|| jq::cmp(get(a, "kind"), get(b, "kind")))
            .then_with(|| jq::cmp(get(a, "name"), get(b, "name")))
    });
    for w in workers {
        let Some(status) = get(w, "status").as_str() else {
            ctx.err(&format!(
                "jq: error (at <stdin>:0): {} cannot be ascii_upcased",
                jq::raw(get(w, "status"))
            ));
            return Err(Fail(5));
        };
        let age = match get(w, "created_at") {
            v if jq::truthy(v) => match v.as_str().and_then(crate::time::created_epoch) {
                Some(c) => dur(now - c as f64),
                None => "-".to_string(),
            },
            _ => "-".to_string(),
        };
        let mut cells = vec![
            jq::tsv(get(w, "name")),
            jq::tsv(&Value::String(status.to_ascii_uppercase())),
            jq::tsv(&Value::String(agent_label(ctx, get(w, "agent")))),
            jq::tsv(&opt(get(w, "topic"))),
            jq::tsv(&Value::String(expiry(w, now))),
            age,
        ];
        if wide {
            cells.extend([
                jq::tsv(&opt(get(w, "owner"))),
                jq::tsv(jq::alt(
                    get(w, "profile"),
                    &Value::String("standard".into()),
                )),
                jq::tsv(jq::alt(&ips_label(w), &Value::String("<none>".into()))),
            ]);
        }
        rows.push(cells.join("\t"));
    }
    if rows.len() <= 1 {
        return Ok(String::new());
    }
    Ok(jq::column(&rows.join("\n")))
}

struct Sub {
    re: Regex,
    rep: String,
}

static COLOR_RULES: LazyLock<Vec<Sub>> = LazyLock::new(|| {
    let (bold_green, green, yellow) = ("\x1b[1;32m", "\x1b[32m", "\x1b[33m");
    let (red, bold_red, reset) = ("\x1b[31m", "\x1b[1;31m", "\x1b[0m");
    let (cyan, warn, urgent) = ("\x1b[36m", "\x1b[38;5;208m", "\x1b[1;31m");
    let s = |re: &str, rep: String| Sub {
        re: Regex::new(re).expect("valid table regex"),
        rep,
    };
    vec![
        s(r" (ACTIVE|READY) ", format!(" {bold_green}${{1}}{reset} ")),
        s(
            r" (PENDING|QUEUED|BUILD|BUILDING|REBUILD|RESIZE|DELETING|BOOTING) ",
            format!(" {yellow}${{1}}{reset} "),
        ),
        s(
            r" (ERROR|FAILED|FAILURE) ",
            format!(" {bold_red}${{1}}{reset} "),
        ),
        s(
            r" (SHUTOFF|DELETED|expired) ",
            format!(" {red}${{1}}{reset} "),
        ),
        s(
            r" (grace [0-9]+[mhd]|draining) ",
            format!(" {yellow}${{1}}{reset} "),
        ),
        s(r" (busy) ", format!(" {green}${{1}}{reset} ")),
        s(
            r" (([0-9]|[12][0-9])m)( +[0-9]+[mhd])",
            format!(" {urgent}${{1}}{reset}${{3}}"),
        ),
        s(
            r" ([3-5][0-9]m)( +[0-9]+[mhd])",
            format!(" {warn}${{1}}{reset}${{2}}"),
        ),
        s(r"^([^ ]+)", format!("{cyan}${{1}}{reset}")),
    ]
});

/// kubecolor-like colors for a table (header on line 1): states, EXPIRES
/// under an hour orange and under 30 minutes bold red, cyan names.
pub fn colorize(table: &str) -> String {
    let lines: Vec<String> = table
        .split('\n')
        .enumerate()
        .map(|(i, line)| {
            if i == 0 {
                return format!("\x1b[1m{line}\x1b[0m");
            }
            let mut l = line.to_string();
            for r in COLOR_RULES.iter() {
                l = r.re.replacen(&l, 1, r.rep.as_str()).into_owned();
            }
            l
        })
        .collect();
    crate::paint::agents(&lines.join("\n"))
}

/// Align plain cells before styling them so ANSI escapes never shift columns.
pub(crate) fn styled_columns(
    ctx: &Ctx,
    rows: &[Vec<&str>],
    style: impl Fn(usize, &str) -> String,
) -> String {
    use std::fmt::Write;
    use unicode_width::UnicodeWidthStr;
    let mut widths = vec![0; rows.first().map_or(0, Vec::len)];
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.width());
        }
    }
    let mut table = String::new();
    for (index, row) in rows.iter().enumerate() {
        if index > 0 {
            table.push('\n');
        }
        for (col, cell) in row.iter().enumerate() {
            if index == 0 {
                table.push_str(&crate::paint::paint(ctx, "1", cell));
            } else {
                table.push_str(&style(col, cell));
            }
            if col + 1 < row.len() {
                let padding = widths[col] - cell.width() + 2;
                let _ = write!(table, "{:padding$}", "");
            }
        }
    }
    table
}

/// The table on stdout: colored on a terminal, "No workers found." when
/// empty.
pub fn render(ctx: &Ctx, list: &Value) -> R {
    let rows = plain(ctx, list)?;
    if rows.is_empty() {
        crate::ctx::outln("No workers found.");
    } else if ctx.color {
        crate::ctx::outln(&colorize(&rows));
    } else {
        crate::ctx::outln(&rows);
    }
    Ok(())
}

/// What counts as a change in --watch (not the ticking EXPIRES/AGE).
pub fn signatures(list: &Value) -> Vec<(String, String)> {
    let Some(items) = jq::items(list) else {
        return Vec::new();
    };
    items
        .into_iter()
        .map(|w| {
            let f = |k: &str| jq::raw(get(w, k));
            (
                f("name"),
                [
                    f("kind"),
                    f("status"),
                    f("ip"),
                    f("owner"),
                    f("expires_at"),
                    f("grace_until"),
                    f("flavor"),
                    f("topic"),
                    f("pool_state"),
                ]
                .join("|"),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Colors must not move anything: the painted table, stripped of its
    /// escape codes, is the plain one (a dropped space once shifted AGE).
    #[test]
    fn colorize_keeps_text() {
        let plain = "NAME  KIND  STATUS  EXPIRES  AGE\n\
                     a  adhoc  ACTIVE  5m       1h9m\n\
                     b  adhoc  ACTIVE  25m      2h\n\
                     c  adhoc  ACTIVE  50m      1h9m\n\
                     d  adhoc  BUILD   1h7m     52m\n\
                     e  adhoc  ERROR   expired  3h\n\
                     f  pool   ACTIVE  busy     1d2h\n\
                     g  adhoc  ACTIVE  grace 4m  1h";
        let painted = colorize(plain);
        let strip = Regex::new("\x1b\\[[0-9;]*m").unwrap();
        assert_eq!(strip.replace_all(&painted, ""), plain);
        assert!(painted.contains("\x1b[1;31m5m\x1b[0m"));
        assert!(painted.contains("\x1b[38;5;208m50m\x1b[0m"));
    }

    #[test]
    fn durations() {
        assert_eq!(dur(-1.0), "expired");
        assert_eq!(dur(59.0), "0m");
        assert_eq!(dur(4140.0), "1h9m");
        assert_eq!(dur(93600.0), "1d2h");
    }
}
