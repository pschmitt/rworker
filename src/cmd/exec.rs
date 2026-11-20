//! `exec`: one-shot build. Create a worker, copy directories to it, run
//! the command, copy results back and delete the worker, also on errors
//! and Ctrl-C.

use crate::ctx::{Ctx, Fail, R};
use crate::jq;
use crate::paint::{self, paint};
use crate::proc;
use crate::signals;
use crate::state;
use serde_json::{Value, json};
use std::process::{Command, Stdio};

/// Deletes the worker when dropped, unless it is to be kept.
struct Cleanup<'a> {
    ctx: &'a Ctx,
    name: String,
    keep: bool,
}

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        if self.keep {
            return;
        }
        let ctx = self.ctx;
        let remote = ["down", &self.name, "--owner", &ctx.owner()];
        let _ = ctx.muted(|| state::remote_quiet(ctx, &remote));
        state::forget(ctx, &self.name);
        ctx.err(&paint::ok_line(ctx, &format!("deleted {}", self.name)));
    }
}

fn interrupted() -> R {
    match signals::pending() {
        Some(_) => Err(Fail(130)),
        None => Ok(()),
    }
}

pub fn cmd_exec(ctx: &Ctx, args: &[String]) -> R {
    let (mut keep, mut keep_failed) = (false, false);
    let mut up_args: Vec<String> = vec!["--quiet-topic-warning".into()];
    let (mut syncs, mut fetches): (Vec<String>, Vec<String>) = (Vec::new(), Vec::new());
    let mut cmd: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() && !args[i].is_empty() {
        let a = args[i].as_str();
        match a {
            "--ttl" | "--flavor" | "--disk" | "--profile" => {
                let v = args.get(i + 1).cloned().unwrap_or_default();
                if v.is_empty() {
                    ctx.err(&format!("{a} needs a value"));
                    return Err(Fail(2));
                }
                up_args.extend([a.to_string(), v]);
                i += 1;
            }
            "--scratch" | "--nix-cache" | "--no-nix-cache" | "--no-niks3-token" => {
                up_args.push(a.to_string());
            }
            "--topic" | "--agent-id" => {
                // `shift` past the end stopped the bash client (set -e).
                let Some(v) = args.get(i + 1) else {
                    return Err(Fail(1));
                };
                up_args.extend([a.to_string(), v.clone()]);
                i += 1;
            }
            "--sync" | "--fetch" => {
                let v = args.get(i + 1).cloned().unwrap_or_default();
                if v.is_empty() {
                    ctx.err(&format!("{a} needs a path"));
                    return Err(Fail(2));
                }
                if a == "--sync" {
                    syncs.push(v);
                } else {
                    fetches.push(v);
                }
                i += 1;
            }
            "--keep" => keep = true,
            "--keep-on-failure" => keep_failed = true,
            "--" => {
                cmd = args[i + 1..].to_vec();
                break;
            }
            s if s.starts_with('-') => {
                ctx.err(&format!("Unknown option: {s}"));
                crate::help::show(ctx, "exec");
                return Err(Fail(2));
            }
            _ => {
                cmd = args[i..].to_vec();
                break;
            }
        }
        i += 1;
    }
    if cmd.is_empty() {
        ctx.err("Missing command");
        crate::help::show(ctx, "exec");
        return Err(Fail(2));
    }

    let json = ctx.json.get();
    let info = super::up::up(ctx, &up_args, true)?.unwrap_or_default();
    let info: Value = serde_json::from_str(&info).map_err(|_| Fail(1))?;
    let need = |k: &str| {
        let v = jq::get(&info, k);
        if jq::truthy(v) {
            Ok(jq::raw(v))
        } else {
            Err(Fail(1))
        }
    };
    let name = need("name")?;
    let ip = need("ip")?;
    ctx.err(&format!(
        "{} {} is {}",
        paint(ctx, "32", "✔"),
        paint::vm(ctx, &name),
        paint(ctx, "1;32", "READY")
    ));
    let opts = state::ssh_opts(ctx, &name);
    // Delete the worker however we leave (also Ctrl-C), unless asked not to.
    signals::trap();
    let mut cleanup = Cleanup {
        ctx,
        name: name.clone(),
        keep,
    };

    for item in &syncs {
        let (src, dest) = match item.split_once(':') {
            Some((s, d)) => (s.to_string(), d.to_string()),
            None => (item.clone(), String::new()),
        };
        let mut a = vec![name.clone(), src];
        if !dest.is_empty() {
            a.push(dest);
        }
        ctx.json.set(true);
        let r = {
            // `> /dev/null`: the sync's own JSON line is not ours.
            let saved = redirect_stdout_null();
            let r = super::files::cmd_sync(ctx, &a);
            restore_stdout(saved);
            r
        };
        ctx.json.set(json);
        interrupted()?;
        r.map_err(|_| Fail(1))?;
        ctx.err(&format!("{} synced {item}", paint(ctx, "32", "✔")));
    }

    let login = format!("agent@{ip}");
    let mut ssh = Command::new("ssh");
    // OpenSSH joins command words with spaces before the remote shell parses
    // them. Preserve that command string inside the job wrapper.
    ssh.args(&opts).arg(&login).arg(format!(
        "bash -c {} -- {}",
        proc::shell_quote(include_str!("../exec-job.sh")),
        proc::shell_quote(&cmd.join(" "))
    ));
    let archive = info["archive_logs"] == true;
    let (rc, stdout, stderr) = if json {
        let o = ssh.output().map_err(|e| {
            ctx.err(&format!("{}: ssh: {e}", ctx.me));
            Fail(1)
        })?;
        (proc::code_of(o.status), o.stdout, o.stderr)
    } else if archive {
        let mut child = ssh
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                ctx.err(&format!("{}: ssh: {e}", ctx.me));
                Fail(1)
            })?;
        let stdout = child.stdout.take().ok_or(Fail(1))?;
        let stderr = child.stderr.take().ok_or(Fail(1))?;
        std::thread::scope(|scope| {
            let out = scope.spawn(move || {
                super::log_cache::stream_tail(stdout, Some(std::io::stdout().lock()))
            });
            let err = scope.spawn(move || {
                super::log_cache::stream_tail(stderr, Some(std::io::stderr().lock()))
            });
            let status = child.wait().map_err(|_| Fail(1))?;
            let stdout = out.join().map_err(|_| Fail(1))?.map_err(|_| Fail(1))?;
            let stderr = err.join().map_err(|_| Fail(1))?.map_err(|_| Fail(1))?;
            Ok((proc::code_of(status), stdout, stderr))
        })?
    } else {
        (proc::run(ctx, &mut ssh)?, Vec::new(), Vec::new())
    };
    if archive {
        let info = json!({
            "topic": info["topic"], "agent": info["agent"], "created_at": info["created_at"],
            "exit_code": rc,
        });
        if super::log_cache::send_record(
            ctx,
            &name,
            &info,
            Some((
                super::log_cache::keep_tail(&stdout),
                super::log_cache::keep_tail(&stderr),
            )),
        )
        .is_err()
        {
            ctx.err(&format!("Could not archive exec logs for {name}"));
        }
    }
    interrupted()?;

    for item in &fetches {
        let (remote_path, local) = match item.split_once(':') {
            Some((r, l)) => (r.to_string(), l.to_string()),
            None => {
                let base = item
                    .trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .filter(|b| !b.is_empty())
                    .unwrap_or("/");
                (item.clone(), base.to_string())
            }
        };
        let code = proc::run(
            ctx,
            Command::new("rsync")
                .args(["-a", "-e"])
                .arg(state::rsync_e(&opts))
                .arg(format!("{login}:{remote_path}"))
                .arg(&local),
        )?;
        interrupted()?;
        if code == 0 {
            ctx.err(&format!(
                "{} fetched {remote_path} to {local}",
                paint(ctx, "32", "✔")
            ));
        } else {
            ctx.err(&format!("Could not fetch {remote_path}"));
        }
    }

    if rc != 0 && keep_failed {
        cleanup.keep = true;
        ctx.err(&format!(
            "Command failed ({rc}); keeping {}: {} ssh {name}",
            paint::vm(ctx, &name),
            ctx.me
        ));
    }
    if json {
        crate::ctx::json_out(
            ctx,
            &jq::pretty(&json!({
                "name": name, "exit_code": rc,
                "stdout": String::from_utf8_lossy(&stdout), "stderr": String::from_utf8_lossy(&stderr),
                "kept": cleanup.keep,
            })),
        );
    }
    drop(cleanup);
    crate::ctx::status(rc)
}

fn redirect_stdout_null() -> i32 {
    use std::io::Write;
    let _ = std::io::stdout().flush();
    // SAFETY: plain descriptor juggling on stdout.
    unsafe {
        let saved = libc::dup(1);
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY);
        if null >= 0 {
            libc::dup2(null, 1);
            libc::close(null);
        }
        saved
    }
}

fn restore_stdout(saved: i32) {
    use std::io::Write;
    let _ = std::io::stdout().flush();
    if saved >= 0 {
        // SAFETY: saved is the descriptor dup'ed from stdout above.
        unsafe {
            libc::dup2(saved, 1);
            libc::close(saved);
        }
    }
}
