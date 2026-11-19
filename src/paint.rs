//! Colors of the human-readable output (none when stdout is not a terminal
//! or NO_COLOR is set) and of the help (unless NO_COLOR is set).

use crate::ctx::Ctx;
use regex::Regex;
use std::sync::LazyLock;

pub fn paint(ctx: &Ctx, code: &str, text: &str) -> String {
    if ctx.color {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

pub fn vm(ctx: &Ctx, text: &str) -> String {
    paint(ctx, "36", text)
}

pub fn flavor(ctx: &Ctx, text: &str) -> String {
    paint(ctx, "34", text)
}

/// A server state, upper-cased and colored by meaning.
pub fn state(ctx: &Ctx, state: &str) -> String {
    let state = state.to_uppercase();
    let (color, bold) = match state.as_str() {
        "ACTIVE" | "READY" | "SUCCEEDED" | "SUCCESS" | "COMPLETED" => ("32", true),
        "ERROR" | "FAILED" | "FAILURE" => ("31", true),
        "PENDING" | "QUEUED" | "BUILD" | "BUILDING" | "REBUILD" | "RESIZE" | "DELETING"
        | "BOOTING" => ("33", false),
        "SHUTOFF" | "DELETED" => ("31", false),
        _ => return state,
    };
    if ctx.color && bold {
        format!("\x1b[1;{color}m{state}\x1b[0m")
    } else if ctx.color {
        format!("\x1b[{color}m{state}\x1b[0m")
    } else {
        state
    }
}

/// A state-like value colored by meaning, its text kept as is.
pub fn value(ctx: &Ctx, v: &str) -> String {
    let lower = v.to_lowercase();
    match lower.as_str() {
        "exit 0" => v.to_string(),
        l if l.starts_with("exit ") => paint(ctx, "1;31", v),
        "running" | "active" | "ready" | "succeeded" | "success" | "completed" => {
            paint(ctx, "32", v)
        }
        "error" | "failed" | "failure" | "shutoff" | "deleted" => paint(ctx, "31", v),
        _ => paint(ctx, "33", v),
    }
}

/// Current UTC time in gray italics, the prefix of every `logs -f` line.
pub fn timestamp(ctx: &Ctx) -> String {
    paint(ctx, "3;90", &crate::time::utc_iso())
}

/// Italic text that survives the resets of the paint calls inside it.
pub fn italic(ctx: &Ctx, text: &str) -> String {
    if ctx.color {
        format!(
            "\x1b[3m{}\x1b[0m",
            text.replace("\x1b[0m", "\x1b[0m\x1b[3m")
        )
    } else {
        text.to_string()
    }
}

/// A status message on stderr: timestamp, then the italic text.
pub fn status_msg(ctx: &Ctx, text: &str) {
    if ctx.quiet.get() {
        return;
    }
    ctx.err(&format!("{} {}", timestamp(ctx), italic(ctx, text)));
}

pub fn event_target(ctx: &Ctx, target: &str) -> String {
    match target.split_once('/') {
        Some((w, rest)) => format!("{}/{}", vm(ctx, w), paint(ctx, "36", rest)),
        None => vm(ctx, target),
    }
}

pub fn ok_line(ctx: &Ctx, text: &str) -> String {
    format!("{} {}", paint(ctx, "32", "✔"), text)
}

pub fn ok(ctx: &Ctx, text: &str) {
    crate::ctx::outln(&ok_line(ctx, text));
}

pub fn warn_mark(ctx: &Ctx) -> String {
    paint(ctx, "33", "!")
}

/// Brand colors for the agent glyphs (truecolor): Claude orange, OpenAI
/// light gray, NixOS blue for nixbot.
pub fn agents(text: &str) -> String {
    let reset = "\x1b[0m";
    text.replace('\u{ec82}', &format!("\x1b[38;2;217;119;87m\u{ec82}{reset}"))
        .replace(
            '\u{ec81}',
            &format!("\x1b[38;2;200;200;200m\u{ec81}{reset}"),
        )
        .replace(
            '\u{f313}',
            &format!("\x1b[38;2;126;186;228m\u{f313}{reset}"),
        )
}

struct Rule {
    re: Regex,
    rep: &'static str,
    all: bool,
    /// Only on Usage lines and their continuations.
    usage_only: bool,
}

fn rule(re: &str, rep: &'static str, all: bool, usage_only: bool) -> Rule {
    Rule {
        re: Regex::new(re).expect("valid help regex"),
        rep,
        all,
        usage_only,
    }
}

// Replacement templates use these placeholders for the escape codes.
static HELP_RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    vec![
        rule(
            r"(^|[ \[|])(--?[a-z][-a-z0-9]*)",
            "${1}<c>${2}<r>",
            true,
            true,
        ),
        rule(
            r"(^|[ \[|=])([A-Z][A-Z_]+([.][.][.])?)([\] |:]|$)",
            "${1}<y>${2}<r>${4}",
            true,
            true,
        ),
        rule(r"^(Usage:)", "<b>${1}<r>", false, false),
        rule(r"^([A-Z][A-Za-z ,()-]*:)$", "<b>${1}<r>", false, false),
        rule(
            r"^(  +)(-[-a-zA-Z0-9]+(, -[-a-zA-Z0-9]+)*) ([A-Z][A-Z_.:]*)",
            "${1}<c>${2}<r> <y>${4}<r>",
            false,
            false,
        ),
        rule(
            r"^(  +)(-[-a-zA-Z0-9]+(, -[-a-zA-Z0-9]+)*)( |$)",
            "${1}<c>${2}<r>${4}",
            false,
            false,
        ),
    ]
});

static OVERVIEW_RULE: LazyLock<Rule> = LazyLock::new(|| {
    rule(
        r"^(  )([a-z][a-z-]+)( |$)",
        "${1}<g>${2}<r>${3}",
        false,
        false,
    )
});

static USAGE_LINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(Usage:|       )").unwrap());

fn apply(rule: &Rule, line: &str) -> String {
    if rule.usage_only && !USAGE_LINE.is_match(line) {
        return line.to_string();
    }
    let rep = rule
        .rep
        .replace("<b>", "\x1b[1m")
        .replace("<c>", "\x1b[36m")
        .replace("<y>", "\x1b[33m")
        .replace("<g>", "\x1b[32m")
        .replace("<r>", "\x1b[0m");
    if rule.all {
        rule.re.replace_all(line, rep.as_str()).into_owned()
    } else {
        rule.re.replacen(line, 1, rep.as_str()).into_owned()
    }
}

/// Colors for help and usage: bold Usage and section headers, cyan flags
/// with yellow arguments, green action names in the overview.
pub fn help(ctx: &Ctx, text: &str, overview: bool) -> String {
    if ctx.no_color {
        return text.to_string();
    }
    text.split('\n')
        .map(|line| {
            let mut l = line.to_string();
            for r in HELP_RULES.iter() {
                l = apply(r, &l);
            }
            if overview {
                l = apply(&OVERVIEW_RULE, &l);
            }
            l
        })
        .collect::<Vec<_>>()
        .join("\n")
}
