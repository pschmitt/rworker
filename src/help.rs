//! Usage, per-action help and the machine-readable command spec (read by
//! the shell completions). The texts live in `src/text/`, with `__ME__`
//! standing for the program name.

use crate::ctx::Ctx;

pub const SPEC: &str = include_str!("text/spec.txt");
pub fn flags(action: &str) -> impl Iterator<Item = (&str, bool)> {
    SPEC.lines().filter_map(move |line| {
        let (owner, rest) = line.strip_prefix("flag ")?.split_once(' ')?;
        let flag = rest.split_whitespace().next()?;
        (owner == action).then(|| {
            flag.split_once('=')
                .map_or((flag, false), |(name, _)| (name, true))
        })
    })
}

const USAGE: &str = include_str!("text/usage.txt");

pub const HELP: &[(&str, &str)] = &[
    ("start", include_str!("text/help-start.txt")),
    ("jobs", include_str!("text/help-jobs.txt")),
    ("ps", include_str!("text/help-ps.txt")),
    ("logs", include_str!("text/help-logs.txt")),
    ("events", include_str!("text/help-events.txt")),
    ("register", include_str!("text/help-register.txt")),
    ("profiles", include_str!("text/help-profiles.txt")),
    ("server", include_str!("text/help-server.txt")),
    ("up", include_str!("text/help-up.txt")),
    ("image", include_str!("text/help-image.txt")),
    ("machines", include_str!("text/help-ls.txt")),
    ("exec", include_str!("text/help-exec.txt")),
    ("down", include_str!("text/help-down.txt")),
    ("prune", include_str!("text/help-prune.txt")),
    ("extend", include_str!("text/help-extend.txt")),
    ("topic", include_str!("text/help-topic.txt")),
    ("claim", include_str!("text/help-claim.txt")),
    ("niks3-token", include_str!("text/help-niks3-token.txt")),
    ("ssh", include_str!("text/help-ssh.txt")),
    ("inspect", include_str!("text/help-inspect.txt")),
    ("sync", include_str!("text/help-sync.txt")),
    ("cp", include_str!("text/help-cp.txt")),
    ("builder", include_str!("text/help-builder.txt")),
    ("nix-cache", include_str!("text/help-nix-cache.txt")),
    ("completions", include_str!("text/help-completions.txt")),
    ("tui", include_str!("text/help-tui.txt")),
];

pub fn usage(ctx: &Ctx) -> String {
    USAGE.replace("__ME__", &ctx.me)
}

/// The help of one action; the usage for anything else.
pub fn action_help(ctx: &Ctx, action: &str) -> String {
    let action = crate::resolve_action(action).unwrap_or(action);
    match HELP.iter().find(|(a, _)| *a == action) {
        Some((_, text)) => text.replace("__ME__", &ctx.me),
        None => usage(ctx),
    }
}

/// `action_help ACTION | paint_help`.
pub fn painted(ctx: &Ctx, action: &str) -> String {
    crate::paint::help(ctx, &action_help(ctx, action), false)
}

/// `usage | paint_help overview`.
pub fn painted_usage(ctx: &Ctx) -> String {
    crate::paint::help(ctx, &usage(ctx), true)
}

/// `show_help ACTION >&2`: the help of the command that rejected its
/// arguments, on stderr, colored unless NO_COLOR is set.
pub fn show(ctx: &Ctx, action: &str) {
    if action.is_empty() {
        ctx.err_raw(&painted_usage(ctx));
    } else {
        ctx.err_raw(&painted(ctx, action));
    }
}

/// `action_help ACTION >&2`, uncolored.
pub fn plain(ctx: &Ctx, action: &str) {
    ctx.err_raw(&action_help(ctx, action));
}
