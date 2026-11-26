//! `up`: create an ad-hoc worker and wait until it is ready.

use crate::ctx::{Ctx, Fail, R, outln};
use crate::jq::{self, get};
use crate::paint::{self, paint};
use crate::proc;
use crate::state;
use regex::Regex;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, IsTerminal, Read, Write};
use std::process::Stdio;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// "in 1h 59m (14:13)" for an epoch timestamp.
pub fn fmt_expiry(at: i64) -> String {
    let left = at - crate::time::now();
    if left <= 0 {
        format!("expired ({})", crate::time::hm(at))
    } else {
        format!(
            "in {}h {}m ({})",
            left / 3600,
            left % 3600 / 60,
            crate::time::hm(at)
        )
    }
}

/// An epoch value as bash arithmetic saw it.
pub fn epoch(v: &Value) -> i64 {
    match v {
        Value::Number(n) => n
            .as_i64()
            .unwrap_or_else(|| n.as_f64().unwrap_or(0.0) as i64),
        Value::String(s) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

/// The control host's stderr while `up` waits: "rworker-progress STAGE"
/// lines become a spinner with the stage and elapsed time, redrawn in place
/// on a terminal and gone once the worker is ready or failed; anything
/// else is printed above it. Without a terminal only the other lines pass.
fn render_progress(ctx: &Ctx, stderr: impl Read + Send + 'static, emit_stages: bool) {
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                // A last line without a newline is dropped, as `read` did.
                Ok(0) | Err(_) => break,
                Ok(_) if buf.last() != Some(&b'\n') => break,
                Ok(_) => {
                    buf.pop();
                    if tx.send(String::from_utf8_lossy(&buf).into_owned()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    render_progress_lines(rx, ctx.no_color, emit_stages);
}

fn render_progress_lines(rx: mpsc::Receiver<String>, no_color: bool, emit_stages: bool) {
    if emit_stages {
        for line in rx {
            eprintln!("{line}");
        }
        return;
    }
    let frames: Vec<char> = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏".chars().collect();
    let tty = std::io::stderr().is_terminal();
    let esc = tty && !no_color;
    let creating = Regex::new(
        r"^creating[[:space:]]+([^[:space:]]+)[[:space:]]+\(([^()]*)\)[[:space:]]+from[[:space:]]+image[[:space:]]+(.+)$",
    )
    .expect("valid regex");
    let mut stage = "REQUESTING".to_string();
    let start = Instant::now();
    let mut i = 0usize;
    let mut e = std::io::stderr();
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                if let Some(s) = line.strip_prefix("rworker-progress ") {
                    stage = s.to_string();
                } else {
                    if tty {
                        let _ = write!(e, "\r\x1b[K");
                    }
                    match creating.captures(&line) {
                        Some(c) if esc => {
                            let _ = writeln!(
                                e,
                                "creating \x1b[36m{}\x1b[0m (\x1b[34m{}\x1b[0m) from image \x1b[35m{}\x1b[0m",
                                &c[1], &c[2], &c[3]
                            );
                        }
                        _ => {
                            let _ = writeln!(e, "{line}");
                        }
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if tty {
            let (state, detail) = match stage.split_once(" · ") {
                Some((s, d)) => (s.to_string(), d.to_string()),
                None => (stage.clone(), String::new()),
            };
            let state = state.to_uppercase();
            let starts = |ps: &[&str]| ps.iter().any(|p| state.starts_with(p));
            let (color, bold) = if starts(&["ACTIVE", "READY", "SUCCEEDED", "SUCCESS", "COMPLETED"])
            {
                (32, true)
            } else if starts(&["ERROR", "FAILED", "FAILURE"]) {
                (31, true)
            } else if starts(&["SHUTOFF", "DELETED"]) {
                (31, false)
            } else {
                (33, false)
            };
            let frame = frames[i % frames.len()];
            i += 1;
            let secs = start.elapsed().as_secs();
            let mut s = String::new();
            if esc {
                s += &format!("\r\x1b[K\x1b[36m{frame}\x1b[0m ");
                if bold {
                    s += &format!("\x1b[1;{color}m{state}\x1b[0m");
                } else {
                    s += &format!("\x1b[{color}m{state}\x1b[0m");
                }
                if !detail.is_empty() {
                    s += &format!(" · \x1b[33m{detail}\x1b[0m");
                }
                s += &format!(" \x1b[2m({secs}s)\x1b[0m");
            } else if !detail.is_empty() {
                s += &format!("\r\x1b[K{frame} {state} · {detail} ({secs}s)");
            } else {
                s += &format!("\r\x1b[K{frame} {state} ({secs}s)");
            }
            let _ = e.write_all(s.as_bytes());
        }
    }
    if tty {
        let _ = write!(e, "\r\x1b[K");
    }
}

/// Warn when the niks3 token the control host reported could not be
/// installed. The worker itself is fine.
pub fn warn_niks3_token(ctx: &Ctx, name: &str, info: &Value) {
    if jq::path(info, &["niks3_token", "ok"]) != &Value::Bool(false) {
        return;
    }
    let err = jq::or(jq::path(info, &["niks3_token", "error"]), "unknown error");
    if err == "not-configured" {
        ctx.err(&format!(
            "{} the controller has no niks3 token issuer: none given to {}",
            paint::warn_mark(ctx),
            paint::vm(ctx, name)
        ));
    } else if err == "image-too-old" {
        ctx.err(&format!(
            "{} image predates niks3 tokens: none given to {}",
            paint::warn_mark(ctx),
            paint::vm(ctx, name)
        ));
    } else {
        ctx.err(&format!(
            "{} could not give {} a niks3 token (the worker is fine): {err}; retry: {} setup niks3-token {name}",
            paint::warn_mark(ctx),
            paint::vm(ctx, name),
            ctx.me
        ));
    }
}

/// Whether to push the S3 cache and the access-tokens: as asked, else
/// whatever this host's Nix has to give (an s3:// substituter, tokens).
fn nix_wants(ctx: &Ctx, nix_cache: Option<bool>, nix_auth: Option<bool>) -> (bool, bool) {
    if nix_cache.is_some() && nix_auth.is_some() {
        return (nix_cache == Some(true), nix_auth == Some(true));
    }
    let have = ctx.muted(|| super::nix_cache::settings(ctx)).ok();
    let has = |key: &str| {
        have.as_ref()
            .and_then(|s| jq::get(s, key).as_array().map(|a| !a.is_empty()))
            .unwrap_or(false)
    };
    (
        nix_cache.unwrap_or_else(|| has("substituters")),
        nix_auth.unwrap_or_else(|| has("auth_hosts")),
    )
}

/// What `up` gives a CI lease, as it does an OpenStack worker: the Nix cache
/// and access-tokens this host has, and a niks3 push token. Each is a
/// convenience: a failure warns and leaves a usable lease. The results go
/// into `info` as "nix_cache" and "niks3_token" (never the token itself).
fn setup_lease_credentials(
    ctx: &Ctx,
    name: &str,
    nix_cache: Option<bool>,
    nix_auth: Option<bool>,
    niks3_token: bool,
    info: &mut Value,
) {
    let (want_cache, want_auth) = nix_wants(ctx, nix_cache, nix_auth);
    if want_cache || want_auth {
        let opts = super::nix_cache::Opts {
            cache: want_cache,
            auth: want_auth,
            auth_hosts: Vec::new(),
        };
        let result = super::nix_cache::push(ctx, name, false, &opts)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({"ok": false}));
        if get(&result, "ok") != &Value::Bool(true) {
            ctx.err(&format!(
                "{} could not set up Nix on {} (the worker is fine); retry: {} setup nix {name}",
                paint::warn_mark(ctx),
                paint::vm(ctx, name),
                ctx.me
            ));
        }
        info["nix_cache"] = result;
    }
    if niks3_token {
        let result = super::nix_cache::issue_lease_niks3_token(ctx, name, false);
        // A controller without niks3 has nothing to give: not a failure.
        if get(&result, "error").as_str() != Some("not-configured") {
            info["niks3_token"] = result;
            warn_niks3_token(ctx, name, info);
        }
    }
}

/// `jq -er .FIELD`: the value as text, failing on null.
fn need(info: &Value, field: &str) -> R<String> {
    let v = get(info, field);
    if jq::truthy(v) {
        Ok(jq::raw(v))
    } else {
        Err(Fail(1))
    }
}

/// Create a worker. With `capture` (exec), the JSON is returned instead of
/// printed, as `JSON=1 cmd_up` in a command substitution.
fn hosted_up(
    ctx: &Ctx,
    profile: &crate::github_profiles::Profile,
    ttl: &str,
    key_file: &str,
    topic: &str,
    queue_timeout: Option<Duration>,
    progress: &mpsc::Sender<String>,
) -> R<Value> {
    let key = std::fs::read_to_string(key_file).map_err(|error| {
        ctx.err(&format!("Read SSH public key {key_file}: {error}"));
        Fail(2)
    })?;
    let owner = ctx.owner();
    let forge = profile.forge;
    let action = |command| format!("{}-{command}", forge.key());
    let (code, raw) = proc::capture(
        ctx,
        &mut state::control_cmd::<&str>(
            ctx,
            &[
                &action("up"),
                profile.runner,
                ttl,
                key.trim(),
                &owner,
                topic,
            ],
        ),
    )?;
    if code != 0 {
        return Err(Fail(if code == 255 { 255 } else { 1 }));
    }
    let initial: Value = serde_json::from_str(&raw).map_err(|_| Fail(255))?;
    let id = need(&initial, "id").map_err(|_| Fail(255))?;
    let suffix = initial["worker_suffix"].as_str().unwrap_or(&id);
    let name = format!("{}{suffix}", forge.prefix());
    let run_url = initial["run_url"].as_str().map(str::to_owned).or_else(|| {
        initial["repository"]
            .as_str()
            .zip(initial["run_id"].as_u64())
            .filter(|_| forge == crate::github_profiles::Forge::GitHub)
            .map(|(repository, run)| format!("https://github.com/{repository}/actions/runs/{run}"))
    });
    let link = run_url.as_deref().map_or_else(String::new, |url| {
        format!(" · {}", workflow_link(url, std::io::stderr().is_terminal()))
    });
    let _ = progress.send(format!(
        "creating {name} ({}) from image {}{link}",
        profile.name, profile.label
    ));
    let _ = progress.send(format!("rworker-progress QUEUED · {}", forge.label()));
    let expires = epoch(&initial["expires_at"]);
    let queue_started = std::time::Instant::now();
    let mut runner_started = false;
    let mut disconnected = false;

    let ready = (|| {
        while crate::time::now() < expires {
            let reply = ctx.muted(|| state::remote(ctx, &[&action("status"), &id]));
            let Ok(lease) =
                reply.and_then(|raw| serde_json::from_str::<Value>(&raw).map_err(|_| Fail(1)))
            else {
                if !disconnected {
                    let _ = progress.send("rworker-progress RECONNECTING · controller".into());
                    disconnected = true;
                }
                std::thread::sleep(Duration::from_secs(3));
                continue;
            };
            let started = runner_started
                || lease["workflow"]["status"] == "in_progress"
                || lease["state"] == "claimed";
            if started != runner_started || disconnected {
                let queued = format!("QUEUED · {}", forge.label());
                let stage = if started { "STARTING · SSH" } else { &queued };
                let _ = progress.send(format!("rworker-progress {stage}"));
            }
            runner_started = started;
            disconnected = false;
            match lease["state"].as_str() {
                Some("queued")
                    if !runner_started
                        && queue_timeout
                            .is_some_and(|timeout| queue_started.elapsed() >= timeout) =>
                {
                    let _ =
                        progress.send(format!("Timed out waiting for a runner for {name}{link}"));
                    return Err(Fail(75));
                }
                Some("ready") => {
                    let output = json!({
                        "name": name,
                        "ip": name,
                        "user": crate::worker_user::login_user(lease["endpoint"]["user"].as_str(), true),
                        "kind": "adhoc",
                        "provider": forge.key(),
                        "profile": profile.name,
                        "flavor": profile.label,
                        "system": profile.system,
                        "image_tags": forge.tags(),
                        "run_url": run_url,
                        "expires_at": expires,
                        "created_at": lease["created_at"],
                        "topic": lease["topic"],
                        "archive_logs": lease["archive_logs"],
                        "run_id": lease["run_id"],
                        "job_id": lease["job_id"],
                        "job_url": lease["job_url"],
                        "github_port": lease["endpoint"]["port"],
                        "host_key": lease["endpoint"]["host_key"],
                        "kvm": lease["kvm"],
                    });
                    state::remember_hosted(ctx, &name, &output)?;
                    return Ok(output);
                }
                Some("ended") => {
                    let conclusion = jq::or(&lease["workflow"]["conclusion"], "unknown");
                    let _ = progress.send(format!(
                        "Pipeline for {name} ended before SSH was ready ({conclusion}){link}"
                    ));
                    return Err(Fail(1));
                }
                Some("cancelling" | "dispatch_unknown") if crate::time::now() < expires => {
                    let _ = progress.send(format!(
                        "Worker lease {name} ended before SSH was ready{link}"
                    ));
                    return Err(Fail(1));
                }
                _ => std::thread::sleep(Duration::from_secs(3)),
            }
        }
        let stage = if runner_started {
            "SSH readiness"
        } else {
            "runner assignment"
        };
        let _ = progress.send(format!(
            "Lease expired while waiting for {stage} on {name}{link}"
        ));
        Err(Fail(1))
    })();
    if ready.is_err()
        && !matches!(
            state::remote_quiet(ctx, &[&action("down"), &id, &owner]),
            Ok(0)
        )
    {
        let _ = progress.send(format!(
            "Could not request cancellation of {name}; the controller will retry at expiry"
        ));
        return Err(Fail(255));
    }
    ready
}

pub fn up(ctx: &Ctx, args: &[String], capture: bool) -> R<Option<String>> {
    up_inner(ctx, args, capture, false, false)
}

pub fn ssh_create(ctx: &Ctx, args: &[String], remove_after_ssh: bool) -> R {
    let mut args = args.to_vec();
    args.push("--ssh".into());
    up_inner(ctx, &args, false, remove_after_ssh, false).map(|_| ())
}

fn up_inner(
    ctx: &Ctx,
    args: &[String],
    capture: bool,
    remove_after_ssh: bool,
    emit_stages: bool,
) -> R<Option<String>> {
    let mut pool = None;
    let mut explicit_profile = false;
    let mut rest = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--arch" {
            pool = Some(
                args.next()
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        ctx.err("--arch needs an architecture");
                        Fail(2)
                    })?
                    .clone(),
            );
        } else if let Some(value) = arg.strip_prefix("--arch=") {
            if value.is_empty() {
                ctx.err("--arch needs an architecture");
                return Err(Fail(2));
            }
            pool = Some(value.to_owned());
        } else if matches!(
            arg.as_str(),
            "--profile"
                | "--ttl"
                | "--flavor"
                | "--disk"
                | "--queue-timeout"
                | "--topic"
                | "--agent-id"
                | "-l"
        ) {
            explicit_profile |= arg == "--profile";
            rest.push(arg.clone());
            if let Some(value) = args.next() {
                rest.push(value.clone());
            }
        } else {
            explicit_profile |= arg.starts_with("--profile=");
            rest.push(arg.clone());
        }
    }
    let Some(pool) = pool else {
        return up_attempt(
            ctx,
            &rest,
            capture,
            remove_after_ssh,
            emit_stages,
            &std::cell::Cell::new(false),
        );
    };
    if explicit_profile {
        ctx.err("--arch cannot be combined with --profile");
        return Err(Fail(2));
    }
    let raw = state::remote(ctx, &["profiles"])?;
    let rows: Vec<Value> = serde_json::from_str(&raw).map_err(|_| {
        ctx.err("Could not decode profiles");
        Fail(1)
    })?;
    let candidates = pool_candidates(&rows, &pool);
    if candidates.is_empty() {
        ctx.err(&format!("No profiles match architecture {pool}"));
        return Err(Fail(75));
    }
    try_pool(&candidates, |name| {
        ctx.err(&format!("Trying profile {name} ({pool})"));
        let mut attempt = rest.clone();
        attempt.extend(["--profile".into(), name.to_owned()]);
        let created = std::cell::Cell::new(false);
        match up_attempt(
            ctx,
            &attempt,
            capture,
            remove_after_ssh,
            emit_stages,
            &created,
        ) {
            Ok(result) => Ok(result),
            Err(error) => {
                ctx.err(&format!("Profile {name} failed"));
                Err((error, created.get()))
            }
        }
    })
    .inspect_err(|_| {
        ctx.err(&format!(
            "Could not create a worker from the {pool} profile pool"
        ));
    })
}

fn try_pool<T>(
    candidates: &[&str],
    mut attempt: impl FnMut(&str) -> Result<T, (Fail, bool)>,
) -> R<T> {
    let mut unavailable = true;
    for name in candidates {
        match attempt(name) {
            Ok(result) => return Ok(result),
            Err((error, created)) if created || error.0 == 2 || error.0 >= 128 => {
                return Err(error);
            }
            Err((error, _)) => unavailable &= error == Fail(75),
        }
    }
    Err(Fail(if unavailable { 75 } else { 1 }))
}

fn pool_candidates<'a>(rows: &'a [Value], arch: &str) -> Vec<&'a str> {
    let mut candidates: Vec<_> = rows
        .iter()
        .filter(|row| row["arch"].as_str() == Some(arch))
        .filter_map(|row| {
            row["name"]
                .as_str()
                .map(|name| (row["default"] == true, name))
        })
        .collect();
    candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    candidates.into_iter().map(|(_, name)| name).collect()
}

fn up_attempt(
    ctx: &Ctx,
    args: &[String],
    capture: bool,
    remove_after_ssh: bool,
    emit_stages: bool,
    created: &std::cell::Cell<bool>,
) -> R<Option<String>> {
    let cleanup = |name: &str| {
        remove_after_ssh.then(|| {
            crate::signals::trap();
            super::exec::Cleanup::new(ctx, name.to_owned(), false)
        })
    };
    let key_file = ctx.key_file();
    let mut login = false;
    let mut login_user = String::new();
    let mut topic = String::new();
    let mut agent_id = String::new();
    let mut nix_cache: Option<bool> = None;
    let mut nix_auth: Option<bool> = None;
    let mut queue_timeout = None;
    let mut quiet_topic_warning = false;
    let mut rest: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() && !args[i].is_empty() {
        match args[i].as_str() {
            "--ssh" => login = true,
            "--nix-cache" => nix_cache = Some(true),
            "--no-nix-cache" => nix_cache = Some(false),
            "--nix-auth" => nix_auth = Some(true),
            "--no-nix-auth" => nix_auth = Some(false),
            "--no-credentials" => {
                nix_cache = Some(false);
                nix_auth = Some(false);
                rest.push("--no-niks3-token".to_string());
            }
            "--queue-timeout" => {
                let Some(seconds) = args
                    .get(i + 1)
                    .and_then(|value| value.parse::<u64>().ok())
                    .filter(|seconds| (1..=86400).contains(seconds))
                else {
                    ctx.err("--queue-timeout needs a number of seconds from 1 to 86400");
                    return Err(Fail(2));
                };
                queue_timeout = Some(Duration::from_secs(seconds));
                i += 1;
            }
            "-l" => {
                login_user = args.get(i + 1).cloned().unwrap_or_default();
                if login_user.is_empty() {
                    ctx.err("-l needs a user");
                    crate::help::show(ctx, "up");
                    return Err(Fail(2));
                }
                i += 1;
            }
            flag @ ("--topic" | "--agent-id") => {
                let v = args.get(i + 1).cloned();
                // `shift` past the end stopped the bash client (set -e).
                let Some(v) = v else {
                    return Err(Fail(1));
                };
                if flag == "--topic" {
                    topic = v;
                } else {
                    agent_id = v;
                }
                i += 1;
            }
            "--quiet-topic-warning" => quiet_topic_warning = true,
            other => rest.push(other.to_string()),
        }
        i += 1;
    }
    if topic.is_empty() && !quiet_topic_warning {
        ctx.err(&format!("{} no topic provided", paint::warn_mark(ctx)));
    }
    let hosted_profile = rest
        .windows(2)
        .filter(|pair| pair[0] == "--profile")
        .map(|pair| pair[1].as_str())
        .chain(rest.iter().filter_map(|arg| arg.strip_prefix("--profile=")))
        .last()
        .and_then(crate::github_profiles::from_name);
    if let Some(profile) = hosted_profile {
        let no_niks3_token = rest.iter().any(|arg| arg == "--no-niks3-token");
        let ttl = rest
            .windows(2)
            .filter(|pair| pair[0] == "--ttl")
            .map(|pair| pair[1].as_str())
            .chain(rest.iter().filter_map(|arg| arg.strip_prefix("--ttl=")))
            .last()
            .unwrap_or("30m");
        let started = crate::time::now();
        let mut output = std::thread::scope(|scope| {
            let (tx, rx) = mpsc::channel();
            let no_color = ctx.no_color;
            scope.spawn(move || render_progress_lines(rx, no_color, emit_stages));
            let result = hosted_up(ctx, profile, ttl, &key_file, &topic, queue_timeout, &tx);
            drop(tx);
            result
        })?;
        created.set(true);
        let name = need(&output, "name")?;
        setup_lease_credentials(
            ctx,
            &name,
            nix_cache,
            nix_auth,
            !no_niks3_token,
            &mut output,
        );
        if capture {
            if !login_user.is_empty() {
                output["ssh_user"] = json!(login_user);
            }
            return Ok(Some(jq::compact(&output)));
        }
        let _cleanup = cleanup(&name);
        if ctx.json.get() {
            crate::ctx::json_out(ctx, &jq::compact(&output));
        } else {
            let user = jq::raw(&output["user"]);
            let kvm = if output["kvm"] == true {
                "available"
            } else {
                "unavailable"
            };
            print_summary(
                ctx,
                &name,
                started,
                &[
                    ("Provider", profile.forge.label().to_owned()),
                    ("User", user),
                    ("Profile", profile.name.to_owned()),
                    ("Flavor", paint::flavor(ctx, profile.label)),
                    ("KVM", kvm.to_owned()),
                    (
                        "Expires",
                        paint(ctx, "33", &fmt_expiry(epoch(&output["expires_at"]))),
                    ),
                    ("SSH", ssh_hint(ctx, &name)),
                    (
                        "Workflow",
                        output["run_url"].as_str().map_or_else(
                            || "-".to_owned(),
                            |url| workflow_link(url, std::io::stdout().is_terminal()),
                        ),
                    ),
                ],
            );
        }
        if login {
            let user = if login_user.is_empty() {
                need(&output, "user")?
            } else {
                login_user
            };
            connect(ctx, &name, &user, &topic)?;
        }
        return Ok(None);
    }
    let meta = crate::creator::meta(&topic, &agent_id);
    let Ok(key) = std::fs::read_to_string(&key_file) else {
        ctx.err(&format!(
            "Public key {key_file} not readable (set RWORKER_KEY)"
        ));
        return Err(Fail(2));
    };
    std::fs::create_dir_all(&ctx.state_dir).map_err(|_| Fail(1))?;
    let started = crate::time::now();
    let mut remote_args: Vec<String> = vec![
        "up".into(),
        "--ssh-key".into(),
        proc::trim_nl(&key),
        "--owner".into(),
        ctx.owner(),
    ];
    remote_args.extend(meta);
    remote_args.extend(rest);
    let mut cmd = state::control_cmd(ctx, &remote_args);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| {
        ctx.err(&format!("{}: ssh: {e}", ctx.me));
        Fail(1)
    })?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let reader = std::thread::spawn(move || {
        let mut s = Vec::new();
        let _ = stdout.read_to_end(&mut s);
        s
    });
    render_progress(ctx, stderr, emit_stages);
    let st = child.wait().map_err(|_| Fail(1))?;
    let out = reader.join().unwrap_or_default();
    if !st.success() {
        return Err(Fail(if st.code() == Some(255) { 255 } else { 1 }));
    }
    let raw_info = proc::trim_nl(&String::from_utf8_lossy(&out));
    created.set(true);
    let parsed: Value = serde_json::from_str(&raw_info).map_err(|_| Fail(1))?;
    let name = need(&parsed, "name")?;
    let _cleanup = cleanup(&name);
    let ip = need(&parsed, "ip")?;
    state::remember_created(ctx, &name, &parsed)?;
    if parsed["archive_logs"] == true {
        let history = json!({
            "topic": parsed["topic"], "agent": parsed["agent"],
            "created_at": parsed["created_at"], "status": "ready",
        });
        if super::log_cache::send_record(ctx, &name, &history, None).is_err() {
            ctx.err(&format!("Could not add {name} to worker history"));
        }
    }

    let (want_cache, want_auth) = nix_wants(ctx, nix_cache, nix_auth);
    let mut info_text = raw_info;
    let mut info = parsed;
    if want_cache || want_auth {
        // Both are conveniences: a failed push leaves a usable worker. Keep a
        // JSON result the push printed, else make one.
        let opts = super::nix_cache::Opts {
            cache: want_cache,
            auth: want_auth,
            auth_hosts: Vec::new(),
        };
        let setup_info = super::nix_cache::push(ctx, &name, false, &opts)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({"ok": false}));
        if get(&setup_info, "daemon").as_str() == Some("image-too-old") {
            ctx.err(&format!(
                "{} image predates `setup nix`: nothing pushed to {}",
                paint::warn_mark(ctx),
                paint::vm(ctx, &name)
            ));
        } else if get(&setup_info, "ok") != &Value::Bool(true) {
            ctx.err(&format!(
                "{} could not set up Nix on {} (the worker is fine); retry: {} setup nix {name}",
                paint::warn_mark(ctx),
                paint::vm(ctx, &name),
                ctx.me
            ));
        }
        // "nix_cache" keeps its name for existing readers; it now also
        // carries the auth part ("auth", "auth_hosts").
        info = jq::merge(&info, &[("nix_cache", setup_info)]);
        info_text = jq::compact(&info);
    }
    warn_niks3_token(ctx, &name, &info);
    if capture {
        if !login_user.is_empty() {
            info["ssh_user"] = json!(login_user);
            info_text = jq::compact(&info);
        }
        return Ok(Some(info_text));
    }
    if ctx.json.get() {
        crate::ctx::json_out(ctx, &info_text);
        return Ok(None);
    }
    let me = &ctx.me;
    let rows = [
        (
            "IP",
            format!("{} {}", paint(ctx, "36", &ip), paint(ctx, "2", "(NetBird)")),
        ),
        (
            "User",
            crate::worker_user::login_user(get(&info, "user").as_str(), false).to_string(),
        ),
        ("Profile", jq::or(get(&info, "profile"), "standard")),
        (
            "Flavor",
            paint::flavor(ctx, &jq::or(get(&info, "flavor"), "-")),
        ),
        (
            "Expires",
            paint(ctx, "33", &fmt_expiry(epoch(get(&info, "expires_at")))),
        ),
        ("SSH", ssh_hint(ctx, &name)),
        (
            "Builder",
            format!(
                "{} \"{} {}\" {}",
                paint(ctx, "32", "nix build --max-jobs 0 --builders"),
                paint(ctx, "32", &format!("{me} builder")),
                paint::vm(ctx, &name),
                paint(ctx, "32", ".#pkg")
            ),
        ),
    ];
    print_summary(ctx, &name, started, &rows);
    if login {
        let user = if login_user.is_empty() {
            crate::worker_user::login_user(get(&info, "user").as_str(), false)
        } else {
            &login_user
        };
        connect(ctx, &name, user, &topic)?;
    }
    Ok(None)
}

fn workflow_link(url: &str, terminal: bool) -> String {
    if terminal {
        format!("\x1b]8;;{url}\x1b\\CI run\x1b]8;;\x1b\\")
    } else {
        url.to_owned()
    }
}

fn ssh_hint(ctx: &Ctx, name: &str) -> String {
    format!(
        "{} {}",
        paint(ctx, "32", &format!("{} ssh", ctx.me)),
        paint::vm(ctx, name),
    )
}

fn print_summary(ctx: &Ctx, name: &str, started: i64, rows: &[(&str, String)]) {
    paint::ok(
        ctx,
        &format!(
            "{} is {} {}",
            paint::vm(ctx, name),
            paint(ctx, "1;32", "READY"),
            paint(ctx, "2", &format!("({}s)", crate::time::now() - started))
        ),
    );
    for (label, value) in rows {
        outln(&format!(
            "  {} {value}",
            paint(ctx, "1", &format!("{label:<8}"))
        ));
    }
}

/// Provision in the background and wait for SSH before handing back the terminal.
pub fn provision_for_ssh(ctx: &Ctx, args: &[String], remove: bool) -> R {
    let info = up_inner(ctx, args, true, false, true)?.ok_or(Fail(1))?;
    let metadata: Value = serde_json::from_str(&info).map_err(|_| Fail(1))?;
    let name = need(&metadata, "name")?;
    let user = metadata["ssh_user"]
        .as_str()
        .or(metadata["user"].as_str())
        .unwrap_or(crate::worker_user::LEGACY_OPENSTACK_USER);
    if remove {
        crate::signals::trap();
    }
    eprintln!("rworker-progress ACTIVE · checking SSH");
    let ready = ssh_target(ctx, &name, user)
        .and_then(|(opts, login)| wait_for_ssh(ctx, &name, &opts, &login));
    if !matches!(ready, Ok(true)) {
        if remove {
            drop(super::exec::Cleanup::new(ctx, name.clone(), false));
        }
        ctx.err(&format!("{name} did not become reachable over SSH"));
        return Err(ready.err().unwrap_or(Fail(1)));
    }
    outln(&jq::compact(&json!({"name": name, "user": user})));
    Ok(())
}

/// Enter a newly provisioned worker from the TUI's interactive phase.
pub fn connect_created(ctx: &Ctx, name: &str, user: &str, remove: bool) -> R {
    let _cleanup = remove.then(|| {
        crate::signals::trap();
        super::exec::Cleanup::new(ctx, name.to_owned(), false)
    });
    let (opts, login) = ssh_target(ctx, name, user)?;
    crate::ctx::status(proc::run(ctx, &mut session_ssh(&opts, &login, ""))?)
}

/// The interactive login on a worker this command created: the session
/// shows up as an `ssh` job (with the worker's topic), like `exec`'s.
fn session_ssh(opts: &[String], login: &str, topic: &str) -> std::process::Command {
    let mut ssh = std::process::Command::new("ssh");
    ssh.args(opts).arg("-t").arg(login).arg(format!(
        "bash -c {} -- {}",
        proc::shell_quote(include_str!("../ssh-job.sh")),
        proc::shell_quote(topic)
    ));
    ssh
}

fn ssh_target(ctx: &Ctx, name: &str, user: &str) -> R<(Vec<String>, String)> {
    let ip = state::worker_ip(ctx, name)?;
    Ok((state::ssh_opts(ctx, name), format!("{user}@{ip}")))
}

/// Log in right after `up`: NetBird may still be setting up the route to
/// the fresh peer, so retry the connection for a little while.
pub fn connect(ctx: &Ctx, name: &str, user: &str, topic: &str) -> R {
    let (opts, login) = ssh_target(ctx, name, user)?;
    wait_for_ssh(ctx, name, &opts, &login)?;
    crate::ctx::status(proc::run(ctx, &mut session_ssh(&opts, &login, topic))?)
}

fn wait_for_ssh(ctx: &Ctx, name: &str, opts: &[String], login: &str) -> R<bool> {
    for i in 1..=10 {
        if let Some(signal) = crate::signals::pending() {
            return Err(Fail(128 + signal));
        }
        if probe(opts, login) {
            return Ok(true);
        }
        ctx.err(&format!(
            "Waiting for {} to accept SSH ({i})...",
            paint::vm(ctx, name)
        ));
        crate::signals::sleep(3.0);
    }
    if let Some(signal) = crate::signals::pending() {
        return Err(Fail(128 + signal));
    }
    Ok(false)
}

/// `ssh OPTS -o ConnectTimeout=5 LOGIN true 2>/dev/null`.
pub fn probe(opts: &[String], login: &str) -> bool {
    std::process::Command::new("ssh")
        .args(opts)
        .args(["-o", "ConnectTimeout=5"])
        .arg(login)
        .arg("true")
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(test)]
mod profile_pool_tests {
    use super::*;

    #[test]
    fn selects_matching_architecture_default_then_name() {
        let rows = vec![
            json!({"name":"z", "arch":"aarch64"}),
            json!({"name":"other", "arch":"x86_64"}),
            json!({"name":"a", "arch":"aarch64"}),
            json!({"name":"default", "arch":"aarch64", "default":true}),
            json!({"name":"unknown"}),
        ];
        assert_eq!(pool_candidates(&rows, "aarch64"), ["default", "a", "z"]);
        assert!(pool_candidates(&rows, "riscv64").is_empty());
    }

    #[test]
    fn tries_next_profile_after_failure_but_stops_after_creation() {
        let mut attempts = Vec::new();
        let result = try_pool(&["first", "second"], |name| {
            attempts.push(name.to_owned());
            if name == "first" {
                Err((Fail(1), false))
            } else {
                Ok(name.to_owned())
            }
        })
        .unwrap();
        assert_eq!(result, "second");
        assert_eq!(attempts, ["first", "second"]);
        attempts.clear();
        let result: R<()> = try_pool(&["first", "second"], |name| {
            attempts.push(name.to_owned());
            Err((Fail(1), true))
        });
        assert!(result.is_err());
        assert_eq!(attempts, ["first"]);
        attempts.clear();
        let result: R<()> = try_pool(&["first", "second"], |name| {
            attempts.push(name.to_owned());
            Err((Fail(255), false))
        });
        assert_eq!(result, Err(Fail(255)));
        assert_eq!(attempts, ["first"]);
    }

    #[test]
    fn only_reports_capacity_unavailable_when_every_attempt_did() {
        let exhausted: R<()> = try_pool(&["first", "second"], |_| Err((Fail(75), false)));
        assert_eq!(exhausted, Err(Fail(75)));
        let mixed: R<()> = try_pool(&["first", "second"], |name| {
            Err((Fail(if name == "first" { 1 } else { 75 }), false))
        });
        assert_eq!(mixed, Err(Fail(1)));
    }
}
