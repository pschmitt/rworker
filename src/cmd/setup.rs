//! `setup TOPIC NAME [OPTIONS]`: get a worker ready for a kind of work with
//! this host's settings. Topics so far: `nix`. More (gradle, ssh, ...) plug
//! in here.

use crate::ctx::{Ctx, Fail, R};

/// The topics `setup` knows, for the help and the completion.
pub const TOPICS: &[&str] = &["nix"];

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
