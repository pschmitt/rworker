//! rworker: client for ad-hoc build workers, disposable encrypted
//! OpenStack VMs that the control host creates on request (`rworker`
//! there). Runs it over SSH, pins each worker's SSH host key locally and
//! wraps ssh, rsync and Nix builder use.

mod cmd;
mod creator;
mod ctx;
mod event_history;
mod github_profiles;
mod help;
mod inventory_feed;
mod jq;
mod paint;
mod pool_icon;
mod proc;
mod server;
mod signals;
mod state;
mod table;
mod time;
mod tui;

use ctx::{Ctx, Fail, R};

/// Every action the dispatch handles (`help` is answered before it).
pub const ACTIONS: &[&str] = &[
    "start",
    "jobs",
    "ps",
    "logs",
    "events",
    "up",
    "image",
    "profiles",
    "server",
    "machines",
    "down",
    "prune",
    "exec",
    "extend",
    "topic",
    "claim",
    "ssh",
    "sync",
    "builder",
    "nix-cache",
    "setup",
    "inspect",
    "cp",
    "completions",
    "tui",
];

const ALIASES: &[(&str, &str)] = &[
    ("ls", "machines"),
    ("nodes", "machines"),
    ("instances", "machines"),
    ("info", "inspect"),
    ("flavors", "profiles"),
];

fn resolve_action(input: &str) -> Result<&str, Vec<&'static str>> {
    let actions = ACTIONS
        .iter()
        .chain(&["help"])
        .map(|&name| (name, name))
        .chain(ALIASES.iter().copied());
    if let Some((_, command)) = actions.clone().find(|(name, _)| *name == input) {
        return Ok(command);
    }
    if input.is_empty() || input.starts_with('-') || input.starts_with("__") {
        return Ok(input);
    }
    let mut matches: Vec<_> = actions
        .filter(|(name, _)| name.starts_with(input))
        .map(|(_, command)| command)
        .collect();
    matches.sort_unstable();
    matches.dedup();
    match matches.as_slice() {
        [command] => Ok(command),
        [] => Ok(input),
        _ => Err(matches),
    }
}

pub const ZSH_COMPLETION: &str = include_str!("completions/_rworker");
pub const BASH_COMPLETION: &str = include_str!("completions/rworker.bash");

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
        .and_then(|out| {
            let workers = serde_json::from_str::<serde_json::Value>(&out).ok()?;
            *ctx.resolution_snapshot.borrow_mut() = Some(out);
            Some(workers)
        })
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
        "setup",
        "inspect",
        "prune",
        "ssh",
        "cp",
    ];
    if !WORKER_ACTIONS.contains(&action) {
        return Ok(out);
    }
    // Retired workers have no inventory entry. Cached reads use exact names.
    if action == "logs" && out.iter().any(|arg| arg == "--cached") {
        return Ok(out);
    }
    let value_flags: Vec<_> = help::flags(action)
        .filter_map(|(flag, takes_value)| takes_value.then_some(flag))
        .collect();
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
        if arg == "--owner" || value_flags.contains(&arg.as_str()) {
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
            // `setup TOPIC NAME`: the topic comes first, the worker second.
            "setup" => {
                if pos == 1 {
                    out[i] = resolve_worker(ctx, &arg)?;
                }
                pos += 1;
            }
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
        "jobs" => cmd::jobs::cmd_jobs(ctx, args),
        "start" | "register" => {
            let mut all = vec![action.to_string()];
            all.extend(args.iter().cloned());
            cmd::jobs::jobs_remote(ctx, &all)
        }
        "ps" => cmd::jobs::cmd_ps(ctx, args),
        "logs" => cmd::jobs::cmd_logs(ctx, args),
        "events" => cmd::events::run(ctx, args),
        "up" => cmd::up::up(ctx, args, false).map(|_| ()),
        "image" => cmd::image::run(ctx, args),
        "profiles" => cmd::simple::cmd_profiles(ctx, args),
        "machines" => cmd::ls::cmd_ls(ctx, args),
        "down" => cmd::simple::cmd_down(ctx, args),
        "prune" => cmd::inspect::cmd_prune(ctx, args),
        "exec" => cmd::exec::cmd_exec(ctx, args),
        "extend" => cmd::simple::cmd_extend(ctx, args),
        "topic" => cmd::simple::cmd_topic(ctx, args),
        "claim" => cmd::simple::cmd_claim(ctx, args),
        "niks3-token" => cmd::simple::cmd_niks3_token(ctx, args),
        "ssh" => cmd::files::cmd_ssh(ctx, args),
        "sync" => cmd::files::cmd_sync(ctx, args),
        "builder" => cmd::simple::cmd_builder(ctx, args),
        "nix-cache" => cmd::nix_cache::cmd_nix_cache(ctx, args),
        "setup" => cmd::setup::cmd_setup(ctx, args),
        "inspect" => cmd::inspect::cmd_inspect(ctx, args),
        "cp" => cmd::files::cmd_cp(ctx, args),
        "tui" => tui::cmd_tui(ctx, args),
        "completions" => match args {
            [shell] if shell == "zsh" => {
                ctx::out(ZSH_COMPLETION);
                Ok(())
            }
            [shell] if shell == "bash" => {
                ctx::out(BASH_COMPLETION);
                Ok(())
            }
            _ => {
                help::show(ctx, "completions");
                Err(Fail(2))
            }
        },
        _ => {
            ctx.err(&format!("Unknown action: {action}"));
            help::show(ctx, "");
            Err(Fail(2))
        }
    }
}

struct Options {
    args: Vec<String>,
    json: bool,
    help: bool,
    remove_after_ssh: bool,
}

fn normalize_options(action: &str, args: &[String]) -> Options {
    let create = action == "ssh" && args.first().is_some_and(|arg| arg == "--create");
    let flags: std::collections::HashMap<_, _> = help::flags(action)
        .chain(help::flags("up").filter(|_| create))
        .chain([("-h", false), ("--help", false)])
        .chain(
            [("-j", false), ("--json", false)]
                .into_iter()
                .filter(|_| action != "ssh"),
        )
        .collect();
    let mut options = Options {
        args: Vec::with_capacity(args.len()),
        json: false,
        help: false,
        remove_after_ssh: false,
    };
    let mut remaining = args.iter();
    while let Some(arg) = remaining.next() {
        if arg == "--" || (matches!(action, "exec" | "ssh") && !create && !arg.starts_with('-')) {
            options.args.push(arg.clone());
            options.args.extend(remaining.cloned());
            break;
        }

        let mut inline = None;
        let mut names = Vec::new();
        if flags.contains_key(arg.as_str()) {
            names.push(arg.clone());
        } else if let Some((name, value)) = arg.split_once('=')
            && name.starts_with("--")
            && flags.get(name) == Some(&true)
        {
            names.push(name.to_string());
            inline = Some(value);
        } else if let Some(cluster) = arg.strip_prefix('-').filter(|rest| !rest.starts_with('-')) {
            for (offset, letter) in cluster.char_indices() {
                let name = format!("-{letter}");
                let Some(&takes_value) = flags.get(name.as_str()) else {
                    names.clear();
                    break;
                };
                names.push(name);
                if takes_value {
                    let suffix = &cluster[offset + letter.len_utf8()..];
                    inline = (!suffix.is_empty()).then_some(suffix);
                    break;
                }
            }
        }

        if names.is_empty() {
            options.args.push(arg.clone());
            // Retain compatibility for the controller's older client options.
            if arg == "--owner" {
                options.args.extend(remaining.next().cloned());
            }
            continue;
        }
        for name in names {
            let takes_value = flags[name.as_str()];
            match name.as_str() {
                "-h" | "--help" => options.help = true,
                "-j" | "--json" => options.json = true,
                "--rm" if action == "ssh" => options.remove_after_ssh = true,
                _ => options.args.push(name),
            }
            if takes_value {
                options.args.extend(
                    inline
                        .map(str::to_owned)
                        .or_else(|| remaining.next().cloned()),
                );
            }
        }
    }
    options
}

fn run(ctx: &Ctx, argv: &[String]) -> R {
    let raw = argv.first().map_or("", String::as_str);
    let action = resolve_action(raw).map_err(|matches| {
        ctx.err(&format!("Ambiguous action {raw}: {}", matches.join(", ")));
        Fail(2)
    })?;
    let second = argv.get(1).cloned().unwrap_or_default();
    match action {
        "__serve-control-socket" => {
            return match argv {
                [_, config] => server::socket::serve(config),
                _ => Err(Fail(2)),
            };
        }
        "__control-socket" => {
            if let Some(path) = argv.get(1) {
                return server::socket::request(path, &argv[2..]);
            }
            return Err(Fail(2));
        }
        "server" => return server::run(ctx, &argv[1..]),
        "__tui-provision-ssh" => {
            let args = &argv[1..];
            let remove = args.first().is_some_and(|arg| arg == "--rm");
            let options = normalize_options("up", &args[usize::from(remove)..]);
            if options.help {
                ctx.err("Use rworker up --help for worker creation options");
                return Err(Fail(2));
            }
            return cmd::up::provision_for_ssh(ctx, &options.args, remove);
        }
        "__connect-created" => {
            return match argv {
                [_, name, user] => cmd::up::connect_created(ctx, name, user, false),
                [_, name, user, flag] if flag == "--rm" => {
                    cmd::up::connect_created(ctx, name, user, true)
                }
                _ => Err(Fail(2)),
            };
        }
        "__builder-gateway" => return server::builder_gateway::run(ctx, &argv[1..]),
        "__action" => {
            let action = resolve_action(&second).map_err(|_| Fail(2))?;
            ctx::outln(action);
            return Ok(());
        }
        "__spec" => {
            ctx::out(help::SPEC);
            return Ok(());
        }
        "__workers" => {
            let _ = cmd::workers::cmd_workers(ctx, &second);
            return Ok(());
        }
        "__values" => {
            let _ = cmd::workers::cmd_values(ctx, &second);
            return Ok(());
        }
        "__jobs" => {
            if second.is_empty() {
                return Ok(());
            }
            return cmd::jobs::jobs_names(ctx, &second);
        }
        // Internal: the background refresh of the completions' cache.
        "__inventory-websocket" => return inventory_feed::serve(ctx),
        "__ls-refresh" => return state::remote_ls(ctx).map(|_| ()),
        // Internal: one follower of `logs -f`.
        "__worker-history" => return cmd::history::client(ctx),
        "__cached-worker-history" => return cmd::history::show(ctx),
        "__cached-archive-exec" => return cmd::log_cache::receive_exec(ctx, &argv[1..]),
        "__collect-job-events" => return cmd::events::collect_jobs(ctx),
        "__collect-logs" => return cmd::log_cache::collect(ctx, &argv[1..]),
        "__cached-logs" => return cmd::log_cache::read(ctx, &argv[1..], false),
        "__cached-preview" => return cmd::log_cache::preview(ctx, &argv[1..]),
        "__cached-preview-local" => return cmd::log_cache::read(ctx, &argv[1..], true),
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
            use std::io::IsTerminal;
            if argv.is_empty() && std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
            {
                return tui::cmd_tui(ctx, &[]);
            }
            // The overview, colored like --help.
            ctx.err_raw(&help::painted_usage(ctx));
            return Err(Fail(2));
        }
        _ => {}
    }
    let Options {
        mut args,
        json,
        help: show_help,
        remove_after_ssh,
    } = normalize_options(action, &argv[1..]);
    if json {
        ctx.json.set(true);
    }
    if show_help {
        ctx::out(&help::painted(ctx, action));
        return Ok(());
    }
    if action == "ssh" && args.first().is_some_and(|arg| arg == "--create") {
        return cmd::up::ssh_create(ctx, &args[1..], remove_after_ssh);
    }
    if remove_after_ssh {
        ctx.err("--rm requires ssh --create");
        return Err(Fail(2));
    }
    args = resolve_args(ctx, action, &args)?;
    dispatch(ctx, action, &args)
}

fn main() {
    signals::default_sigpipe();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let ctx = Ctx::from_env();
    #[cfg(feature = "web")]
    if let [action, worker] = argv.as_slice()
        && action == "__web-ssh"
    {
        std::process::exit(match cmd::files::web_ssh(&ctx, worker) {
            Ok(()) => 0,
            Err(Fail(code)) => code,
        });
    }
    let remote_command = std::env::var_os("RWORKER_CONTROL_SSH_COMPAT").is_some()
        && std::env::var_os("SSH_CONNECTION").is_some()
        && std::env::var_os("SSH_TTY").is_none()
        && match argv.as_slice() {
            [action, rest @ ..] if action == "ls" => rest.is_empty() || rest == ["--fresh"],
            [action, ..] => matches!(
                action.as_str(),
                "profiles"
                    | "up"
                    | "down"
                    | "extend"
                    | "topic"
                    | "claim"
                    | "niks3-token"
                    | "lifecycle-events"
                    | "github-up"
                    | "github-status"
                    | "github-down"
                    | "github-mesh-setup"
                    | "github-mesh-status"
                    | "gitlab-up"
                    | "gitlab-status"
                    | "gitlab-down"
                    | "gitlab-mesh-setup"
                    | "gitlab-mesh-status"
            ),
            [] => false,
        };
    let result = if remote_command {
        match std::env::var("RWORKER_CONTROL_SOCKET") {
            Ok(path) => server::socket::request(&path, &argv),
            Err(_) => {
                ctx.err("Missing RWORKER_CONTROL_SOCKET for SSH control request");
                Err(Fail(1))
            }
        }
    } else {
        run(&ctx, &argv)
    };
    let code = match result {
        Ok(()) => 0,
        Err(Fail(c)) => c,
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_prefixes_and_aliases() {
        for name in [
            "machines",
            "ma",
            "ls",
            "no",
            "nod",
            "node",
            "nodes",
            "inst",
            "instanc",
            "instances",
        ] {
            assert_eq!(resolve_action(name), Ok("machines"));
        }
        assert_eq!(resolve_action("info"), Ok("inspect"));
        assert_eq!(resolve_action("fl"), Ok("profiles"));
        assert_eq!(resolve_action("lo"), Ok("logs"));
        assert!(resolve_action("in").is_err());
        assert_eq!(resolve_action("__jobs"), Ok("__jobs"));
    }

    #[test]
    fn normalizes_options_using_each_actions_flag_arity() {
        let options = |action, args: &[&str]| {
            normalize_options(
                action,
                &args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>(),
            )
        };
        assert_eq!(options("machines", &["-Aw"]).args, ["-A", "-w"]);
        assert_eq!(
            options("machines", &["-wAon"]).args,
            ["-w", "-A", "-o", "n"]
        );
        assert_eq!(options("jobs", &["-wn5"]).args, ["-w", "-n", "5"]);
        assert_eq!(options("logs", &["-fq"]).args, ["-f", "-q"]);
        assert_eq!(
            options("prune", &["-yn", "worker"]).args,
            ["-y", "-n", "worker"]
        );
        assert_eq!(options("machines", &["-Aq"]).args, ["-Aq"]);
        assert_eq!(options("machines", &["--topic=-j"]).args, ["--topic", "-j"]);
        assert!(!options("machines", &["--topic", "-j"]).json);
        assert!(options("machines", &["-Ajh"]).help);
        assert!(options("machines", &["-Ajh"]).json);
    }

    #[test]
    fn option_normalization_preserves_remote_command_arguments() {
        let options = |action, args: &[&str]| {
            normalize_options(
                action,
                &args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>(),
            )
        };
        assert_eq!(
            options("ssh", &["-lagent", "worker", "-jqh"]).args,
            ["-l", "agent", "worker", "-jqh"],
        );
        assert_eq!(
            options("ssh", &["--create", "--profile=standard", "-lagent"]).args,
            ["--create", "--profile", "standard", "-l", "agent"],
        );
        let create = options(
            "ssh",
            &["--create", "--rm", "--topic=--rm", "--profile=gh-aarch64"],
        );
        assert!(create.remove_after_ssh);
        assert_eq!(
            create.args,
            ["--create", "--topic", "--rm", "--profile", "gh-aarch64"]
        );
        assert!(!options("ssh", &["--create", "--topic=--rm"]).remove_after_ssh);
        assert!(!options("ssh", &["worker", "echo", "--rm"]).remove_after_ssh);
        assert_eq!(
            options("exec", &["command", "--json", "-h"]).args,
            ["command", "--json", "-h"],
        );
        assert_eq!(
            options("start", &["worker", "--", "-jqh"]).args,
            ["worker", "--", "-jqh"],
        );
    }

    fn spec_lines(kind: &str) -> Vec<String> {
        help::SPEC
            .lines()
            .filter_map(|l| l.strip_prefix(&format!("{kind} ")))
            .map(String::from)
            .collect()
    }

    /// Every flag the completion scripts name exists in the spec, and both
    /// skip exactly the value-taking flags of `logs` when they look for its
    /// worker: the scripts cannot drift from the commands they complete.
    #[test]
    fn completions_follow_the_spec() {
        let flags: Vec<String> = spec_lines("flag")
            .iter()
            .map(|l| {
                l.split(' ')
                    .nth(1)
                    .unwrap()
                    .split('=')
                    .next()
                    .unwrap()
                    .to_string()
            })
            .chain(["--help".to_string()])
            .collect();
        let re = regex::Regex::new(r"(^|[^-\w])(--[a-z][a-z-]*)").unwrap();
        for (name, script) in [("zsh", ZSH_COMPLETION), ("bash", BASH_COMPLETION)] {
            let code: String = script
                .lines()
                .filter(|l| !l.trim_start().starts_with('#'))
                .collect::<Vec<_>>()
                .join("\n");
            for c in re.captures_iter(&code) {
                assert!(
                    flags.contains(&c[2].to_string()),
                    "{name} completion names unknown flag {}",
                    &c[2]
                );
            }
        }
        let mut logs_value_flags: Vec<String> = spec_lines("flag")
            .iter()
            .filter(|l| l.starts_with("logs ") && l.split(' ').nth(1).unwrap().contains('='))
            .map(|l| {
                l.split(' ')
                    .nth(1)
                    .unwrap()
                    .split('=')
                    .next()
                    .unwrap()
                    .to_string()
            })
            .collect();
        logs_value_flags.sort();
        assert_eq!(
            logs_value_flags,
            [
                "--agent",
                "--agent-id",
                "--lines",
                "--redact-regex",
                "--topic"
            ],
            "logs gained or lost a value flag: update the worker detection of both completion scripts"
        );
        assert!(
            ZSH_COMPLETION.contains("--lines|--topic|--agent|--agent-id|--redact-regex) skipval=1")
        );
        assert!(BASH_COMPLETION.contains(
            r#"[[ "$w" == --lines || "$w" == --topic || "$w" == --agent || "$w" == --agent-id || "$w" == --redact-regex ]]"#
        ));
    }

    /// The command spec (read by the completions), the dispatch and the
    /// per-action help list the same actions and flags.
    #[test]
    fn spec_dispatch_and_help_agree() {
        let aliases: Vec<String> = ALIASES
            .iter()
            .map(|(name, target)| format!("{name} {target}"))
            .collect();
        let spec_aliases: Vec<String> = spec_lines("alias")
            .iter()
            .map(|line| line.splitn(3, ' ').take(2).collect::<Vec<_>>().join(" "))
            .collect();
        assert_eq!(spec_aliases, aliases);
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
        assert_eq!(
            spec_lines("values")
                .iter()
                .find_map(|line| line.strip_prefix("setup-topic "))
                .unwrap()
                .split_whitespace()
                .collect::<Vec<_>>(),
            cmd::setup::TOPICS,
        );

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
