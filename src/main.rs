//! rworker: client for ad-hoc build workers, disposable encrypted
//! OpenStack VMs that the control host creates on request (`rworker`
//! there). Runs it over SSH, pins each worker's SSH host key locally and
//! wraps ssh, rsync and Nix builder use.

mod cmd;
mod creator;
mod ctx;
mod help;
mod jq;
mod paint;
mod proc;
mod signals;
mod state;
mod table;
mod time;

use ctx::{Ctx, Fail, R};

/// Every action the dispatch handles (`help` is answered before it).
pub const ACTIONS: &[&str] = &[
    "start",
    "jobs",
    "ps",
    "logs",
    "events",
    "register",
    "up",
    "profiles",
    "ls",
    "down",
    "prune",
    "exec",
    "extend",
    "topic",
    "claim",
    "niks3-token",
    "ssh",
    "sync",
    "builder",
    "nix-cache",
    "inspect",
    "cp",
];

/// A worker name may be abbreviated: an exact name wins, else a unique
/// prefix, else a unique substring. A name this host already knows (state
/// file) skips the lookup; an ambiguous fragment is an error.
fn resolve_worker(ctx: &Ctx, frag: &str) -> R<String> {
    if frag.is_empty() || ctx.state_file(frag).is_file() {
        return Ok(frag.to_string());
    }
    let names: Vec<String> = ctx
        .muted(|| state::remote_ls(ctx))
        .ok()
        .and_then(|out| serde_json::from_str::<serde_json::Value>(&out).ok())
        .and_then(|v| {
            jq::items(&v).map(|items| {
                items
                    .into_iter()
                    .map(|w| jq::raw(jq::get(w, "name")))
                    .collect()
            })
        })
        .unwrap_or_default();
    if names.is_empty() {
        return Ok(frag.to_string());
    }
    if let Some(exact) = names.iter().find(|n| *n == frag) {
        return Ok(exact.clone());
    }
    // grep "^frag" with every character but [A-Za-z0-9_-] as a wildcard.
    let prefix = |n: &str| {
        let mut nc = n.chars();
        frag.chars().all(|f| match nc.next() {
            Some(c) if f.is_ascii_alphanumeric() || f == '_' || f == '-' => c == f,
            Some(_) => true,
            None => false,
        })
    };
    let mut hits: Vec<&String> = names
        .iter()
        .filter(|n| n.contains(frag) && prefix(n))
        .collect();
    if hits.is_empty() {
        hits = names.iter().filter(|n| n.contains(frag)).collect();
    }
    match hits.len() {
        0 => Ok(frag.to_string()),
        1 => Ok(hits[0].clone()),
        _ => {
            ctx.err(&format!("Ambiguous worker name {frag}, matches:"));
            for h in hits {
                ctx.err(&format!("  {h}"));
            }
            Err(Fail(1))
        }
    }
}

/// Rewrite abbreviated worker names in the arguments of ACTION: only the
/// argument positions that are worker names, so topic and command texts
/// are left alone. Options that take a value are skipped with it.
fn resolve_args(ctx: &Ctx, action: &str, args: &[String]) -> R<Vec<String>> {
    let mut out = args.to_vec();
    const WORKER_ACTIONS: &[&str] = &[
        "start",
        "jobs",
        "register",
        "logs",
        "ps",
        "down",
        "extend",
        "topic",
        "claim",
        "niks3-token",
        "sync",
        "builder",
        "nix-cache",
        "inspect",
        "prune",
        "ssh",
        "cp",
    ];
    if !WORKER_ACTIONS.contains(&action) {
        return Ok(out);
    }
    const VALUE_FLAGS: &[&str] = &[
        "--lines",
        "--topic",
        "--agent",
        "--agent-id",
        "--ttl",
        "--owner",
        "--name",
        "--workdir",
        "--interval",
        "-n",
        "-l",
        "-o",
        "--log",
        "--flavor",
        "--disk",
        "--profile",
    ];
    let mut skip_next = false;
    let mut pos = 0;
    #[allow(clippy::needless_range_loop)] // out[i] is rewritten in place
    for i in 0..out.len() {
        let arg = out[i].clone();
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "--" {
            break;
        }
        if VALUE_FLAGS.contains(&arg.as_str()) {
            skip_next = true;
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        match action {
            "cp" => {
                if cmd::files::is_remote_spec(&arg) {
                    let (target, path) = arg.split_once(':').unwrap_or((&arg, ""));
                    let (user, name) = match target.split_once('@') {
                        Some((u, n)) => (format!("{u}@"), n.to_string()),
                        None => (String::new(), target.to_string()),
                    };
                    let full = resolve_worker(ctx, &name)?;
                    out[i] = format!("{user}{full}:{path}");
                }
            }
            "prune" => out[i] = resolve_worker(ctx, &arg)?,
            _ => {
                if pos == 0 {
                    let (user, name) = match arg.split_once('@') {
                        Some((u, n)) if action == "ssh" => (format!("{u}@"), n.to_string()),
                        _ => (String::new(), arg.clone()),
                    };
                    out[i] = format!("{user}{}", resolve_worker(ctx, &name)?);
                }
                pos += 1;
                if matches!(action, "ssh" | "topic" | "claim") {
                    break;
                }
            }
        }
    }
    Ok(out)
}

fn dispatch(ctx: &Ctx, action: &str, args: &[String]) -> R {
    match action {
        "start" | "jobs" | "register" => {
            let mut all = vec![action.to_string()];
            all.extend(args.iter().cloned());
            cmd::jobs::jobs_remote(ctx, &all)
        }
        "ps" => cmd::jobs::cmd_ps(ctx, args),
        "logs" => cmd::jobs::cmd_logs(ctx, args),
        "events" => cmd::jobs::cmd_events(ctx, args),
        "up" => cmd::up::up(ctx, args, false).map(|_| ()),
        "profiles" => cmd::simple::cmd_profiles(ctx, args),
        "ls" => cmd::ls::cmd_ls(ctx, args),
        "down" => cmd::simple::cmd_down(ctx, args),
        "prune" => cmd::inspect::cmd_prune(ctx, args),
        "exec" => cmd::exec::cmd_exec(ctx, args),
        "extend" => cmd::simple::cmd_extend(ctx, args),
        "topic" => cmd::simple::cmd_topic(ctx, args),
        "claim" => cmd::simple::cmd_claim(ctx, args),
        "niks3-token" => cmd::simple::cmd_niks3_token(ctx, args),
        "ssh" => {
            if args.first().map(String::as_str) == Some("--create") {
                let mut a = args[1..].to_vec();
                a.push("--ssh".into());
                cmd::up::up(ctx, &a, false).map(|_| ())
            } else {
                cmd::files::cmd_ssh(ctx, args)
            }
        }
        "sync" => cmd::files::cmd_sync(ctx, args),
        "builder" => cmd::simple::cmd_builder(ctx, args),
        "nix-cache" => cmd::nix_cache::cmd_nix_cache(ctx, args),
        "inspect" => cmd::inspect::cmd_inspect(ctx, args),
        "cp" => cmd::files::cmd_cp(ctx, args),
        _ => {
            ctx.err(&format!("Unknown action: {action}"));
            help::show(ctx, "");
            Err(Fail(2))
        }
    }
}

fn run(ctx: &Ctx, argv: &[String]) -> R {
    let action = argv.first().cloned().unwrap_or_default();
    let second = argv.get(1).cloned().unwrap_or_default();
    match action.as_str() {
        "__spec" => {
            ctx::out(help::SPEC);
            return Ok(());
        }
        "__workers" => {
            let _ = cmd::workers::cmd_workers(ctx, &second);
            return Ok(());
        }
        "__jobs" => {
            if second.is_empty() {
                return Ok(());
            }
            return cmd::jobs::jobs_names(ctx, &second);
        }
        // Internal: the background refresh of the completions' cache.
        "__ls-refresh" => return state::remote_ls(ctx).map(|_| ()),
        // Internal: one follower of `logs -f`.
        "__jobs-remote" => return cmd::jobs::jobs_remote(ctx, &argv[1..]),
        "-h" | "--help" | "help" => {
            if second.is_empty() {
                ctx::out(&help::painted_usage(ctx));
            } else {
                ctx::out(&help::painted(ctx, &second));
            }
            return Ok(());
        }
        "" => {
            // The overview, colored like --help.
            ctx.err_raw(&help::painted_usage(ctx));
            return Err(Fail(2));
        }
        _ => {}
    }
    let mut args: Vec<String> = argv[1..].to_vec();
    // --json anywhere (but not for ssh, whose arguments are ssh's).
    if action != "ssh" {
        let mut rest = Vec::new();
        let mut in_cmd = false;
        for a in args {
            if in_cmd {
                rest.push(a);
            } else if a == "--json" || a == "-j" {
                ctx.json.set(true);
            } else {
                // exec: everything from -- on belongs to the command.
                in_cmd = a == "--";
                rest.push(a);
            }
        }
        args = rest;
    }
    // `ACTION --help`, except where the arguments belong to ssh or the
    // command once a worker or command is given.
    for a in &args {
        if a == "--" {
            break;
        }
        if (action == "ssh" || action == "exec") && !a.starts_with('-') {
            break;
        }
        if a == "-h" || a == "--help" {
            ctx::out(&help::painted(ctx, &action));
            return Ok(());
        }
    }
    if action != "ssh" || args.first().map(String::as_str) != Some("--create") {
        args = resolve_args(ctx, &action, &args)?;
    }
    dispatch(ctx, &action, &args)
}

fn main() {
    signals::default_sigpipe();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let ctx = Ctx::from_env();
    let code = match run(&ctx, &argv) {
        Ok(()) => 0,
        Err(Fail(c)) => c,
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec_lines(kind: &str) -> Vec<String> {
        help::SPEC
            .lines()
            .filter_map(|l| l.strip_prefix(&format!("{kind} ")))
            .map(String::from)
            .collect()
    }

    /// The command spec (read by the completions), the dispatch and the
    /// per-action help list the same actions and flags.
    #[test]
    fn spec_dispatch_and_help_agree() {
        let mut spec: Vec<String> = spec_lines("action")
            .iter()
            .map(|l| l.split(' ').next().unwrap().to_string())
            .filter(|a| a != "help")
            .collect();
        spec.sort();
        let mut dispatched: Vec<String> = ACTIONS.iter().map(|a| a.to_string()).collect();
        dispatched.sort();
        assert_eq!(spec, dispatched, "spec actions and dispatch differ");
        let mut helped: Vec<String> = help::HELP.iter().map(|(a, _)| a.to_string()).collect();
        helped.sort();
        assert_eq!(spec, helped, "spec actions and action_help differ");

        let ctx = Ctx::from_env();
        for line in spec_lines("flag") {
            let mut parts = line.split(' ');
            let action = parts.next().unwrap();
            let flag = parts.next().unwrap().split('=').next().unwrap();
            assert!(
                help::action_help(&ctx, action).contains(flag),
                "flag {flag} of {action} is missing from its help"
            );
        }
        for line in spec_lines("global") {
            let flag = line.split(' ').next().unwrap();
            assert!(
                help::usage(&ctx).contains(flag),
                "global flag {flag} is missing from the usage"
            );
        }
    }
}
