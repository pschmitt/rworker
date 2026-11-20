//! ssh, sync and cp: talking to a worker with its pinned host key.

use crate::ctx::{Ctx, Fail, R};
use crate::jq;
use crate::paint;
use crate::proc;
use crate::state;
use serde_json::json;
use std::process::Command;

/// "[user@]name": the worker and the user named in it (empty: per worker).
pub struct Target {
    pub name: String,
    pub user: String,
}

pub fn split_target(ctx: &Ctx, t: &str) -> R<Target> {
    let after_at = t.split_once('@').map(|(_, r)| r);
    if t.is_empty() || (t.ends_with('@') && after_at == Some("")) {
        ctx.err("Missing worker name");
        return Err(Fail(2));
    }
    Ok(match t.split_once('@') {
        Some((u, n)) => Target {
            name: n.to_string(),
            user: u.to_string(),
        },
        None => Target {
            name: t.to_string(),
            user: String::new(),
        },
    })
}

/// The login user: the one named, RWORKER_USER, or the worker's own.
pub fn target_user(ctx: &Ctx, t: &Target) -> R<String> {
    if !t.user.is_empty() {
        return Ok(t.user.clone());
    }
    let env = crate::ctx::env("RWORKER_USER");
    if !env.is_empty() {
        return Ok(env);
    }
    state::worker_field(ctx, &t.name, "user")
}

/// The ssh command for `cmd_ssh TARGET ARGS...`, ready to run.
pub fn ssh_command(ctx: &Ctx, args: &[String]) -> R<Command> {
    let mut args = args;
    let mut login_user = String::new();
    // -l USER (like ssh's), before the name.
    if args.first().map(String::as_str) == Some("-l") {
        login_user = args.get(1).cloned().unwrap_or_default();
        if login_user.is_empty() {
            ctx.err("-l needs a user");
            crate::help::show(ctx, "ssh");
            return Err(Fail(2));
        }
        args = &args[2..];
    }
    let Some(first) = args.first().filter(|a| !a.is_empty()) else {
        crate::help::show(ctx, "ssh");
        return Err(Fail(2));
    };
    let mut t = split_target(ctx, first).map_err(|_| Fail(2))?;
    if !login_user.is_empty() {
        t.user = login_user;
    }
    let ip = state::worker_ip(ctx, &t.name)?;
    let user = target_user(ctx, &t).map_err(|_| Fail(1))?;
    let opts = state::ssh_opts(ctx, &t.name);
    let mut cmd = state::worker_ssh(&opts, &format!("{user}@{ip}"));
    cmd.args(&args[1..]);
    Ok(cmd)
}

pub fn cmd_ssh(ctx: &Ctx, args: &[String]) -> R {
    let mut cmd = ssh_command(ctx, args)?;
    crate::ctx::status(proc::run(ctx, &mut cmd)?)
}

pub fn cmd_sync(ctx: &Ctx, args: &[String]) -> R {
    let target = args.first().cloned().unwrap_or_default();
    let src = args.get(1).cloned().unwrap_or_default();
    let mut dest = args.get(2).cloned().unwrap_or_default();
    if target.is_empty() || src.is_empty() {
        crate::help::show(ctx, "sync");
        return Err(Fail(2));
    }
    let t = split_target(ctx, &target).map_err(|_| Fail(2))?;
    let ip = state::worker_ip(ctx, &t.name)?;
    let user = target_user(ctx, &t).map_err(|_| Fail(1))?;
    let opts = state::ssh_opts(ctx, &t.name);
    if dest.is_empty() {
        dest = match realpath_basename(&src) {
            Some(b) => b,
            None => {
                ctx.err(&format!("realpath: {src}: No such file or directory"));
                String::new()
            }
        };
    }
    let src_dir = format!("{}/", src.strip_suffix('/').unwrap_or(&src));
    let code = proc::run(
        ctx,
        Command::new("rsync")
            .args([
                "-a",
                "--delete",
                "--exclude",
                "target/",
                "--exclude",
                ".direnv/",
            ])
            .args(["--exclude", "/result", "-e"])
            .arg(state::rsync_e(&opts))
            .arg(&src_dir)
            .arg(format!("{user}@{ip}:{dest}/")),
    )?;
    if code != 0 {
        return Err(Fail(1));
    }
    if ctx.json.get() {
        crate::ctx::json_out(
            ctx,
            &jq::compact(&json!({"name": t.name, "src": src, "dest": dest, "user": user})),
        );
    } else {
        paint::ok(
            ctx,
            &format!("synced {src} to {}:{dest}", paint::vm(ctx, &t.name)),
        );
    }
    Ok(())
}

/// `basename "$(realpath SRC)"`: GNU realpath accepts a missing last
/// component (only the directories above it must exist).
fn realpath_basename(src: &str) -> Option<String> {
    let base = |p: &std::path::Path| {
        p.file_name()
            .map_or_else(|| "/".to_string(), |f| f.to_string_lossy().into_owned())
    };
    if let Ok(p) = std::fs::canonicalize(src) {
        return Some(base(&p));
    }
    let path = std::path::Path::new(src);
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => std::path::Path::new("."),
    };
    std::fs::canonicalize(parent).ok()?;
    path.file_name().map(|f| f.to_string_lossy().into_owned())
}

/// NAME:PATH (a local path with ":" needs a ./ or / prefix).
pub fn is_remote_spec(s: &str) -> bool {
    s.contains(':') && !s.starts_with("./") && !s.starts_with('/')
}

pub fn cmd_cp(ctx: &Ctx, args: &[String]) -> R {
    let src = args.first().cloned().unwrap_or_default();
    let dst = args.get(1).cloned().unwrap_or_default();
    if src.is_empty() || dst.is_empty() || args.get(2).is_some_and(|a| !a.is_empty()) {
        crate::help::show(ctx, "cp");
        return Err(Fail(2));
    }
    let (down, remote, mut local) = if is_remote_spec(&src) {
        (true, src.clone(), dst.clone())
    } else if is_remote_spec(&dst) {
        (false, dst.clone(), src.clone())
    } else {
        ctx.err("One side must be [USER@]NAME:PATH");
        crate::help::show(ctx, "cp");
        return Err(Fail(2));
    };
    // Make relative local paths unambiguous to rsync, including names with
    // colons.
    if !local.starts_with('/') && !local.starts_with("./") && !local.starts_with("../") {
        local = format!("./{local}");
    }
    let (target, path) = remote.split_once(':').unwrap_or((&remote, ""));
    let t = split_target(ctx, target).map_err(|_| Fail(2))?;
    let ip = state::worker_ip(ctx, &t.name)?;
    let user = target_user(ctx, &t).map_err(|_| Fail(1))?;
    let opts = state::ssh_opts(ctx, &t.name);
    let spec = format!("{user}@{ip}:{path}");
    let mut cmd = Command::new("rsync");
    cmd.args(["-a", "--partial", "-e"])
        .arg(state::rsync_e(&opts));
    if down {
        cmd.arg(&spec).arg(&local);
    } else {
        cmd.arg(&local).arg(&spec);
    }
    if proc::run(ctx, &mut cmd)? != 0 {
        return Err(Fail(1));
    }
    if ctx.json.get() {
        let direction = if down { "down" } else { "up" };
        crate::ctx::json_out(
            ctx,
            &jq::compact(&json!({"name": t.name, "src": src, "dest": dst, "direction": direction})),
        );
    } else {
        paint::ok(ctx, &format!("copied {src} to {dst}"));
    }
    Ok(())
}
