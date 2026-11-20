//! `tui`: an interactive terminal UI over the other actions. It lists the
//! workers (refreshed in the background), follows logs and events, shows
//! jobs, processes and details, and runs the actions on the selected
//! worker. Everything goes through this program again (`rworker ACTION`
//! as a child process), so the TUI does exactly what the CLI does and
//! never blocks on the network while drawing.

mod ansi;
mod app;
mod proc;
mod ui;

use crate::ctx::{Ctx, Fail, R};
use std::io::IsTerminal;
use std::time::Duration;

pub fn cmd_tui(ctx: &Ctx, args: &[String]) -> R {
    let mut interval = 5u64;
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
            a => {
                ctx.err(&format!("Unknown option: {a}"));
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
    app::run(ctx, Duration::from_secs(interval)).map_err(|e| {
        ctx.err(&format!("{}: tui: {e}", ctx.me));
        Fail(1)
    })
}
