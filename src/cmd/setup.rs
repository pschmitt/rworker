//! `setup TOPIC NAME [OPTIONS]`: get a worker ready for a kind of work with
//! this host's settings or issue a worker-specific credential.

use crate::ctx::{Ctx, Fail, R};

/// The topics `setup` knows, for the help and the completion.
pub const TOPICS: &[&str] = &["nix", "niks3-token", "register-log-file"];

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
        "niks3-token" => super::simple::cmd_niks3_token(ctx, &args[1..]),
        "register-log-file" => {
            let mut remote = vec!["register".to_string()];
            remote.extend_from_slice(&args[1..]);
            super::jobs::jobs_remote(ctx, &remote)
        }
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
