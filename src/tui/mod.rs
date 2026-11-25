//! `tui`: an interactive terminal UI over the other actions. It lists the
//! workers (refreshed in the background), follows logs and events, shows
//! jobs, processes and details, and runs the actions on the selected
//! worker. Everything goes through this program again (`rworker ACTION`
//! as a child process), so the TUI does exactly what the CLI does and
//! never blocks on the network while drawing.

mod ansi;
mod app;
mod create;
mod proc;
mod ui;

use crate::ctx::{Ctx, Fail, R};
use std::io::IsTerminal;
use std::time::Duration;

#[derive(Clone, Copy)]
enum View {
    Workers,
    Jobs,
    Profiles,
    Events,
    Logs,
}

/// What the TUI shows first.
enum Start {
    View(View),
    /// `rworker logs` in a terminal: its log, without chrome, until the
    /// user leaves it for the rest of the TUI.
    Logs(Vec<String>),
}

impl View {
    fn from_prefix(prefix: &str) -> Option<Self> {
        if prefix.is_empty() {
            return None;
        }
        [
            ("workers", Self::Workers),
            ("jobs", Self::Jobs),
            ("profiles", Self::Profiles),
            ("events", Self::Events),
            ("logs", Self::Logs),
        ]
        .into_iter()
        .find_map(|(name, view)| name.starts_with(prefix).then_some(view))
    }
}

pub fn cmd_tui(ctx: &Ctx, args: &[String]) -> R {
    let mut interval = 5u64;
    let mut view = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-n" | "--interval" => {
                i += 1;
                match args.get(i).and_then(|v| v.parse::<u64>().ok()) {
                    Some(n) if n >= 1 => interval = n,
                    _ => {
                        ctx.err("Interval must be a whole number of seconds");
                        crate::help::show(ctx, "tui");
                        return Err(Fail(2));
                    }
                }
            }
            a if view.is_none() && View::from_prefix(a).is_some() => {
                view = View::from_prefix(a);
            }
            a => {
                if a.starts_with('-') {
                    ctx.err(&format!("Unknown option: {a}"));
                } else if view.is_some() {
                    ctx.err("Specify only one TUI view");
                } else {
                    ctx.err(&format!("Unknown TUI view: {a}"));
                }
                crate::help::show(ctx, "tui");
                return Err(Fail(2));
            }
        }
        i += 1;
    }
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        ctx.err("tui needs a terminal");
        return Err(Fail(2));
    }
    app::run(
        ctx,
        Duration::from_secs(interval),
        Start::View(view.unwrap_or(View::Workers)),
    )
    .map_err(|e| {
        ctx.err(&format!("{}: tui: {e}", ctx.me));
        Fail(1)
    })
}

/// Whether `logs` should open in the TUI: both ends are a terminal.
pub fn logs_interactive() -> bool {
    std::io::stdout().is_terminal() && std::io::stdin().is_terminal()
}

/// `rworker logs ARGS` in a terminal: the TUI with the plain log (from
/// `rworker logs ARGS --plain`) as its only view, chrome off.
pub fn cmd_logs(ctx: &Ctx, args: &[String]) -> R {
    app::run(ctx, Duration::from_secs(5), Start::Logs(args.to_vec())).map_err(|e| {
        ctx.err(&format!("{}: tui: {e}", ctx.me));
        Fail(1)
    })
}
