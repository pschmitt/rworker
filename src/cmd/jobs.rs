//! Registered jobs on workers: start, jobs, register, logs, ps and the
//! lifecycle events. The job manager itself runs on the worker
//! (`src/jobs.sh`, fed to `bash -s` over the pinned SSH connection).

use crate::ctx::{Ctx, Fail, R, outln};
use crate::jq::{self, get};
use crate::paint::{self, paint};
use crate::proc;
use crate::signals;
use crate::state;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::process::{Child, Command, Stdio};

pub const JOBS_SH: &str = include_str!("../jobs.sh");

/// The ssh command that runs the job manager's ACTION on WORKER.
fn jobs_command(ctx: &Ctx, args: &[String]) -> R<Command> {
    // `shift 2 || return 2`
    if args.len() < 2 {
        return Err(Fail(2));
    }
    let (action, worker) = (&args[0], &args[1]);
    let mut rest: Vec<String> = args[2..].to_vec();
    if action == "logs" {
        rest.extend(["--worker-prefix".to_string(), worker.clone()]);
    }
    if worker.is_empty() {
        ctx.err("worker name required");
        return Err(Fail(2));
    }
    let mut words = vec![action.clone()];
    words.extend(rest);
    let remote = format!(
        "bash -s --{}",
        words
            .iter()
            .map(|w| format!(" {}", proc::shell_quote(w)))
            .collect::<String>()
    );
    super::files::ssh_command(ctx, &[worker.clone(), remote])
}

/// `cmd_jobs_remote ACTION WORKER ARGS...`: output passed through.
pub fn jobs_remote(ctx: &Ctx, args: &[String]) -> R {
    let mut cmd = jobs_command(ctx, args)?;
    crate::ctx::status(proc::run_with_input(ctx, &mut cmd, JOBS_SH.as_bytes())?)
}

/// `$(cmd_jobs_remote ...)`.
fn jobs_capture(ctx: &Ctx, args: &[String]) -> R<String> {
    let mut cmd = jobs_command(ctx, args)?;
    capture_job_command(ctx, &mut cmd)
}

fn capture_job_command(ctx: &Ctx, cmd: &mut Command) -> R<String> {
    let (code, out) = proc::capture_with_input(ctx, cmd, JOBS_SH.as_bytes())?;
    crate::ctx::status(code)?;
    Ok(out)
}

/// Poll a bounded group of independent workers and retain request order.
fn poll_jobs(ctx: &Ctx, requests: &[Vec<String>]) -> Vec<R<String>> {
    let quiet = ctx.quiet.get();
    requests
        .chunks(4)
        .flat_map(|batch| {
            // Prepare commands in order, including host-key and local state writes.
            let commands: Vec<_> = batch.iter().map(|args| jobs_command(ctx, args)).collect();
            std::thread::scope(|scope| {
                let tasks: Vec<_> = commands
                    .into_iter()
                    .map(|command| {
                        scope.spawn(move || {
                            let ctx = Ctx::from_env();
                            ctx.quiet.set(quiet);
                            capture_job_command(&ctx, &mut command?)
                        })
                    })
                    .collect();
                tasks
                    .into_iter()
                    .map(|task| task.join().unwrap_or(Err(Fail(1))))
                    .collect::<Vec<_>>()
            })
        })
        .collect()
}

/// `__jobs WORKER`: job names for the completions.
pub fn jobs_names(ctx: &Ctx, worker: &str) -> R {
    let out = jobs_capture(ctx, &["jobs".to_string(), worker.to_string()]);
    let text = match &out {
        Ok(t) => t.as_str(),
        Err(_) => "",
    };
    for line in text.lines() {
        outln(line.split('\t').next().unwrap_or(""));
    }
    out.map(|_| ())
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| (*x).to_string()).collect()
}

pub fn cmd_logs(ctx: &Ctx, args: &[String]) -> R {
    let mut positional: Vec<String> = Vec::new();
    let mut opts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        match a.as_str() {
            "--lines" | "--topic" | "--agent" | "--agent-id" => {
                opts.push(a.clone());
                opts.push(args.get(i + 1).cloned().unwrap_or_default());
                i += if i + 1 < args.len() { 2 } else { 1 };
            }
            s if s.starts_with('-') => {
                opts.push(a.clone());
                i += 1;
            }
            _ => {
                positional.push(a.clone());
                i += 1;
            }
        }
    }
    // Options may come before or after WORKER [JOB]: `logs -f WORKER` too.
    if !positional.is_empty() {
        let mut all = vec!["logs".to_string(), positional[0].clone()];
        all.extend(positional[1..].iter().cloned());
        all.extend(opts);
        return jobs_remote(ctx, &all);
    }
    let mut lines = "200".to_string();
    let (mut follow, mut no_events, mut quiet) = (false, false, false);
    let (mut topic, mut agent) = (String::new(), String::new());
    let mut i = 0;
    let need = |flag: &str, v: &str| -> R<String> {
        if v.is_empty() {
            ctx.err(&format!("{flag} requires a regex pattern"));
            Err(Fail(2))
        } else {
            Ok(v.to_string())
        }
    };
    while i < opts.len() && !opts[i].is_empty() {
        let a = opts[i].as_str();
        let next = opts.get(i + 1).cloned().unwrap_or_default();
        match a {
            "--lines" => {
                lines = next;
                i += 2;
            }
            "--follow" | "-f" => {
                follow = true;
                i += 1;
            }
            "--no-events" => {
                no_events = true;
                i += 1;
            }
            "--quiet" | "-q" => {
                quiet = true;
                i += 1;
            }
            "--topic" | "--agent" | "--agent-id" => {
                let v = need(a, &next)?;
                match a {
                    "--topic" => topic = v,
                    "--agent" => agent = v,
                    _ => *ctx.filter_agent_id.borrow_mut() = v,
                }
                i += 2;
            }
            _ if a.starts_with("--topic=")
                || a.starts_with("--agent=")
                || a.starts_with("--agent-id=") =>
            {
                let (flag, v) = a.split_once('=').unwrap_or((a, ""));
                let v = need(flag, v)?;
                match flag {
                    "--topic" => topic = v,
                    "--agent" => agent = v,
                    _ => *ctx.filter_agent_id.borrow_mut() = v,
                }
                i += 1;
            }
            _ => {
                ctx.err(&format!("Unknown logs option: {a}"));
                crate::help::plain(ctx, "logs");
                return Err(Fail(2));
            }
        }
    }
    if lines.is_empty() || !lines.bytes().all(|b| b.is_ascii_digit()) {
        ctx.err("invalid line count");
        return Err(Fail(2));
    }
    if !topic.is_empty() {
        super::ls::validate_regex(ctx, "--topic", &topic)?;
    }
    if !agent.is_empty() {
        super::ls::validate_regex(ctx, "--agent", &agent)?;
    }
    let id = ctx.filter_agent_id.borrow().clone();
    if !id.is_empty() {
        super::ls::validate_regex(ctx, "--agent-id", &id)?;
    }
    if follow {
        logs_all_follow(ctx, &lines, no_events, quiet, &topic, &agent)
    } else {
        logs_all(ctx, &lines, &topic, &agent)
    }
}

/// The filtered worker list of the control host, parsed.
fn workers(ctx: &Ctx, topic: &str, agent: &str) -> R<Value> {
    let out = state::remote_ls(ctx)?;
    let out = super::ls::filter_workers(ctx, &out, topic, agent)?;
    state::parse(ctx, &out)
}

fn names(list: &Value) -> Vec<String> {
    jq::items(list)
        .unwrap_or_default()
        .into_iter()
        .map(|w| jq::raw(get(w, "name")))
        .collect()
}

/// Workers that can be reached already (they have an IP and a host key).
fn ready(list: &Value) -> Vec<String> {
    jq::items(list)
        .unwrap_or_default()
        .into_iter()
        .filter(|w| {
            !jq::or_empty(get(w, "ip")).is_empty() && !jq::or_empty(get(w, "host_key")).is_empty()
        })
        .map(|w| jq::raw(get(w, "name")))
        .collect()
}

/// List registered jobs on reachable workers without serial SSH round trips.
pub fn jobs_all(ctx: &Ctx) -> R {
    let list = workers(ctx, "", "")?;
    let mut names = ready(&list);
    names.retain(|name| !name.is_empty());
    names.sort_unstable();
    names.dedup();
    let requests: Vec<_> = names.iter().map(|name| s(&["jobs", name])).collect();
    let mut records = Vec::new();
    let mut failed = 0;
    for (worker, output) in names.iter().zip(poll_jobs(ctx, &requests)) {
        let Ok(output) = output else {
            failed += 1;
            continue;
        };
        for line in output.lines().filter(|line| !line.is_empty()) {
            if ctx.json.get() {
                let mut fields = line.splitn(3, '\t');
                if let (Some(job), Some(state), Some(started)) =
                    (fields.next(), fields.next(), fields.next())
                {
                    records.push(serde_json::json!({
                        "worker": worker, "job": job, "state": state, "started_at": started,
                    }));
                }
            } else {
                outln(&format!("{worker}\t{line}"));
            }
        }
    }
    if ctx.json.get() {
        crate::ctx::json_out(ctx, &Value::Array(records).to_string());
    }
    if failed > 0 {
        ctx.err(&format!(
            "Could not list jobs on {failed} of {} workers",
            names.len()
        ));
    }
    if failed > 0 && failed == names.len() {
        return Err(Fail(1));
    }
    Ok(())
}

fn logs_all(ctx: &Ctx, lines: &str, topic: &str, agent: &str) -> R {
    let list = workers(ctx, topic, agent)?;
    let mut status = Ok(());
    for w in names(&list) {
        if w.is_empty() {
            continue;
        }
        if jobs_remote(ctx, &s(&["logs", &w, "--lines", lines])).is_err() {
            ctx.err(&format!("could not read logs from {}", paint::vm(ctx, &w)));
            status = Err(Fail(1));
        }
    }
    status
}

/// A child process and everything below it.
pub fn kill_tree(pid: u32) {
    for child in children_of(pid) {
        kill_tree(child);
    }
    // SAFETY: sending a signal to a pid has no memory-safety preconditions.
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
}

fn children_of(pid: u32) -> Vec<u32> {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    dir.filter_map(Result::ok)
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|p| {
            std::fs::read_to_string(format!("/proc/{p}/stat"))
                .ok()
                .and_then(|st| {
                    let rest = st.rsplit_once(')')?.1;
                    rest.split_whitespace().nth(1)?.parse::<u32>().ok()
                })
                == Some(pid)
        })
        .collect()
}

/// This program again, for a background part of `logs -f`, with the
/// parent's color and quiet decisions.
fn self_command(ctx: &Ctx) -> Command {
    let exe = std::env::current_exe().unwrap_or_else(|_| "rworker".into());
    let mut cmd = Command::new(exe);
    cmd.env("__RWORKER_COLOR", if ctx.color { "1" } else { "0" });
    if ctx.quiet.get() {
        cmd.env("__RWORKER_QUIET", "1");
    }
    cmd
}

fn alive(child: &mut Child) -> bool {
    matches!(child.try_wait(), Ok(None))
}

fn logs_all_follow(
    ctx: &Ctx,
    lines: &str,
    no_events: bool,
    quiet: bool,
    topic: &str,
    agent: &str,
) -> R {
    let discovery = 5.0;
    let status_interval = 300;
    let mut no_events = no_events;
    if quiet {
        ctx.quiet.set(true);
        no_events = true;
    }
    signals::trap();
    let mut events: Option<Child> = None;
    if !no_events {
        // Events are status messages here: stderr, keeping stdout for logs.
        let mut cmd = self_command(ctx);
        cmd.args(["events", "--interval", "5"]);
        if !topic.is_empty() {
            cmd.args(["--topic", topic]);
        }
        if !agent.is_empty() {
            cmd.args(["--agent", agent]);
        }
        let id = ctx.filter_agent_id.borrow().clone();
        if !id.is_empty() {
            cmd.args(["--agent-id", &id]);
        }
        // SAFETY: dup returns a new descriptor for stderr or -1.
        let fd = unsafe { libc::dup(2) };
        if fd >= 0 {
            // SAFETY: fd is a fresh descriptor this Stdio now owns.
            cmd.stdout(unsafe { <Stdio as std::os::fd::FromRawFd>::from_raw_fd(fd) });
        }
        events = cmd.spawn().ok();
    }
    let mut followers: BTreeMap<String, Child> = BTreeMap::new();
    let (mut last_key, mut last_at) = (String::new(), 0i64);
    let stop = |followers: &mut BTreeMap<String, Child>, events: &mut Option<Child>| {
        for c in followers.values_mut() {
            kill_tree(c.id());
            let _ = c.wait();
        }
        if let Some(e) = events {
            kill_tree(e.id());
            let _ = e.wait();
        }
    };
    loop {
        if let Some(sig) = signals::pending() {
            stop(&mut followers, &mut events);
            return Err(Fail(if sig == libc::SIGINT { 130 } else { 143 }));
        }
        let Ok(out) = state::poll_ls(ctx) else {
            ctx.err("could not refresh worker list; retrying");
            signals::sleep(discovery);
            continue;
        };
        let Ok(list) =
            super::ls::filter_workers(ctx, &out, topic, agent).and_then(|f| state::parse(ctx, &f))
        else {
            signals::sleep(discovery);
            continue;
        };
        if !no_events {
            emit_status(ctx, &list, status_interval, &mut last_key, &mut last_at);
        }
        let current: BTreeSet<String> = names(&list).into_iter().collect();
        for w in ready(&list) {
            if w.is_empty() {
                continue;
            }
            if let Some(c) = followers.get_mut(&w)
                && alive(c)
            {
                continue;
            }
            followers.remove(&w);
            let mut cmd = self_command(ctx);
            cmd.args(["__jobs-remote", "logs", &w, "--lines", lines, "--follow"]);
            if quiet {
                cmd.arg("--quiet");
            }
            if let Ok(c) = cmd.spawn() {
                followers.insert(w, c);
            }
        }
        let gone: Vec<String> = followers
            .keys()
            .filter(|w| !current.contains(*w))
            .cloned()
            .collect();
        for w in gone {
            if let Some(mut c) = followers.remove(&w) {
                kill_tree(c.id());
                let _ = c.wait();
            }
        }
        signals::sleep(discovery);
    }
}

/// Periodic worker count for `logs -f`: every interval while workers
/// exist, and once (not repeatedly) when there are none until it changes.
fn emit_status(ctx: &Ctx, list: &Value, interval: i64, last_key: &mut String, last_at: &mut i64) {
    let Some(items) = jq::items(list) else {
        return;
    };
    let key = items.len().to_string();
    let now = crate::time::now();
    if key == *last_key && (key == "0" || now - *last_at < interval) {
        return;
    }
    let summary = if key == "0" {
        "no workers".to_string()
    } else {
        let mut kinds: Vec<&Value> = items.iter().map(|w| get(w, "kind")).collect();
        kinds.sort_by(|a, b| jq::cmp(a, b));
        let mut groups: Vec<(String, usize)> = Vec::new();
        for k in kinds {
            let k = jq::raw(k);
            match groups.last_mut() {
                Some((g, n)) if *g == k => *n += 1,
                _ => groups.push((k, 1)),
            }
        }
        let n = items.len();
        let k: Vec<String> = groups.iter().map(|(g, c)| format!("{c} {g}")).collect();
        format!(
            "{n} worker{} ({})",
            if n == 1 { "" } else { "s" },
            k.join(", ")
        )
    };
    // only the numbers are colored
    let summary = if ctx.color {
        regex::Regex::new("[0-9]+")
            .expect("valid regex")
            .replace_all(&summary, "\x1b[36m${0}\x1b[0m")
            .into_owned()
    } else {
        summary
    };
    ctx.err(&format!(
        "{} {}",
        paint::timestamp(ctx),
        paint::italic(ctx, &format!("status {summary}"))
    ));
    *last_key = key;
    *last_at = now;
}

fn emit_event(ctx: &Ctx, kind: &str, target: &str, details: &str) {
    let color = if kind.ends_with("-added") {
        "32"
    } else if kind.ends_with("-deleted") {
        "31"
    } else if kind == "job-state" {
        "33"
    } else {
        "36"
    };
    let mut text = format!(
        "{} {}",
        paint(ctx, color, kind),
        paint::event_target(ctx, target)
    );
    if !details.is_empty() {
        if let Some((a, b)) = details.split_once(" -> ") {
            text += &format!(" {} -> {}", paint::value(ctx, a), paint::value(ctx, b));
        } else if let Some(c) = regex::Regex::new(r"^([a-z_-]+=)(.*)$")
            .expect("valid regex")
            .captures(details)
        {
            // key=value: only the value is colored
            text += &format!(" {}{}", &c[1], paint::value(ctx, &c[2]));
        } else {
            text += &format!(" {}", paint(ctx, "33", details));
        }
    }
    outln(&format!(
        "{} {}",
        paint::timestamp(ctx),
        paint::italic(ctx, &text)
    ));
}

pub fn cmd_events(ctx: &Ctx, args: &[String]) -> R {
    let mut interval = "2".to_string();
    let (mut topic, mut agent) = (String::new(), String::new());
    let need = |flag: &str, v: &str| -> R<String> {
        if v.is_empty() {
            ctx.err(&format!("{flag} requires a regex pattern"));
            Err(Fail(2))
        } else {
            Ok(v.to_string())
        }
    };
    let mut i = 0;
    while i < args.len() && !args[i].is_empty() {
        let a = args[i].as_str();
        let next = args.get(i + 1).cloned();
        match a {
            "--interval" => {
                // `shift 2` past the end stopped the bash client (set -e).
                let Some(v) = next else {
                    return Err(Fail(1));
                };
                interval = v;
                i += 2;
            }
            "--topic" | "--agent" | "--agent-id" => {
                let v = need(a, &next.unwrap_or_default())?;
                match a {
                    "--topic" => topic = v,
                    "--agent" => agent = v,
                    _ => *ctx.filter_agent_id.borrow_mut() = v,
                }
                i += 2;
            }
            _ if a.starts_with("--topic=")
                || a.starts_with("--agent=")
                || a.starts_with("--agent-id=") =>
            {
                let (flag, v) = a.split_once('=').unwrap_or((a, ""));
                let v = need(flag, v)?;
                match flag {
                    "--topic" => topic = v,
                    "--agent" => agent = v,
                    _ => *ctx.filter_agent_id.borrow_mut() = v,
                }
                i += 1;
            }
            _ => {
                ctx.err(&format!("Unknown events option: {a}"));
                crate::help::plain(ctx, "events");
                return Err(Fail(2));
            }
        }
    }
    let secs = match interval.parse::<u64>() {
        Ok(n) if n >= 1 && interval.bytes().all(|b| b.is_ascii_digit()) => n as f64,
        _ => {
            ctx.err("interval must be a positive whole number of seconds");
            return Err(Fail(2));
        }
    };
    if !topic.is_empty() {
        super::ls::validate_regex(ctx, "--topic", &topic)?;
    }
    if !agent.is_empty() {
        super::ls::validate_regex(ctx, "--agent", &agent)?;
    }
    let mut initialized = false;
    let mut prev_workers: BTreeSet<String> = BTreeSet::new();
    let mut prev_jobs: BTreeMap<String, String> = BTreeMap::new();
    loop {
        let Ok(out) = state::poll_ls(ctx) else {
            ctx.err("could not refresh worker list; retrying");
            proc::sleep_secs(secs);
            continue;
        };
        let Ok(list) = super::ls::filter_workers(ctx, &out, &topic, &agent)
            .and_then(|f| state::parse(ctx, &f))
        else {
            proc::sleep_secs(secs);
            continue;
        };
        let mut cur_workers: BTreeSet<String> = BTreeSet::new();
        let mut cur_jobs: BTreeMap<String, String> = BTreeMap::new();
        let mut polled: HashMap<String, bool> = HashMap::new();
        for w in names(&list) {
            if w.is_empty() {
                continue;
            }
            if initialized && !prev_workers.contains(&w) {
                emit_event(ctx, "worker-added", &w, "");
            }
            cur_workers.insert(w);
        }
        if initialized {
            for w in &prev_workers {
                if !cur_workers.contains(w) {
                    emit_event(ctx, "worker-deleted", w, "");
                }
            }
        }
        let workers: Vec<_> = ready(&list).into_iter().filter(|w| !w.is_empty()).collect();
        let requests: Vec<_> = workers.iter().map(|w| s(&["jobs", w])).collect();
        for (w, jobs) in workers.into_iter().zip(poll_jobs(ctx, &requests)) {
            let Ok(jobs) = jobs else { continue };
            polled.insert(w.clone(), true);
            for line in jobs.lines() {
                // `read job state started` with IFS=tab: empty fields merge.
                let mut fields = line.split('\t').filter(|f| !f.is_empty());
                let Some(job) = fields.next() else { continue };
                let state = fields.next().unwrap_or("").to_string();
                let key = format!("{w}/{job}");
                let previous = prev_jobs.get(&key).cloned().unwrap_or_default();
                cur_jobs.insert(key.clone(), state.clone());
                if initialized && previous.is_empty() {
                    emit_event(ctx, "job-added", &key, &format!("state={state}"));
                } else if initialized && state != previous {
                    emit_event(ctx, "job-state", &key, &format!("{previous} -> {state}"));
                }
            }
        }
        if initialized {
            for (key, state) in &prev_jobs {
                let w = key.split('/').next().unwrap_or("");
                if !cur_workers.contains(w) {
                    continue;
                }
                if !polled.contains_key(w) {
                    cur_jobs.insert(key.clone(), state.clone());
                } else if !cur_jobs.contains_key(key) {
                    emit_event(ctx, "job-deleted", key, "");
                }
            }
        }
        prev_workers = cur_workers;
        prev_jobs = cur_jobs;
        initialized = true;
        proc::sleep_secs(secs);
    }
}

pub fn cmd_ps(ctx: &Ctx, args: &[String]) -> R {
    let mut target = String::new();
    let mut long = false;
    for a in args {
        match a.as_str() {
            "--long" => long = true,
            s if s.starts_with('-') => {
                crate::help::plain(ctx, "ps");
                return Err(Fail(2));
            }
            s => {
                if !target.is_empty() {
                    crate::help::plain(ctx, "ps");
                    return Err(Fail(2));
                }
                target = s.to_string();
            }
        }
    }
    let out = state::remote_ls(ctx)?;
    let list = state::parse(ctx, &out)?;
    let all = state::items(ctx, &list)?;
    let workers: Vec<(String, String)> = if target.is_empty() {
        all.iter()
            .map(|w| (jq::tsv(get(w, "name")), jq::tsv(get(w, "kind"))))
            .collect()
    } else {
        let bare = target.split_once('@').map_or(target.as_str(), |(_, n)| n);
        let found: Vec<(String, String)> = all
            .iter()
            .filter(|w| get(w, "name").as_str() == Some(bare))
            .map(|w| (jq::tsv(get(w, "name")), jq::tsv(get(w, "kind"))))
            .collect();
        if found.is_empty() {
            ctx.err(&format!("No worker named {target}"));
            return Err(Fail(1));
        }
        found
    };
    let mut rows = String::from("WORKER\tJOB\tPID\tPPID\tELAPSED\tCOMMAND");
    let mut status = Ok(());
    let workers: Vec<_> = workers
        .into_iter()
        .filter(|(worker, _)| !worker.is_empty())
        .collect();
    let requests: Vec<_> = workers
        .iter()
        .map(|(worker, kind)| {
            let login = if target.contains('@') {
                target.clone()
            } else if kind == "adhoc" {
                format!("agent@{worker}")
            } else {
                worker.clone()
            };
            let mut a = s(&["ps", &login]);
            if long {
                a.push("--long".into());
            }
            a
        })
        .collect();
    for ((worker, _), processes) in workers.into_iter().zip(poll_jobs(ctx, &requests)) {
        let Ok(processes) = processes else {
            ctx.err(&format!("could not read job processes from {worker}"));
            status = Err(Fail(1));
            continue;
        };
        for line in processes.lines().filter(|l| !l.is_empty()) {
            rows += &format!("\n{worker}\t{line}");
        }
    }
    let rows = jq::column(&rows);
    if ctx.color {
        let first = regex::Regex::new(r"^([^ ]+)( +)([^ ]+)").expect("valid regex");
        let rest = regex::Regex::new(r"^(([^ ]+ +){5})(.*)$").expect("valid regex");
        let painted: Vec<String> = rows
            .split('\n')
            .enumerate()
            .map(|(i, l)| {
                if i == 0 {
                    return format!("\x1b[1m{l}\x1b[0m");
                }
                let l = first
                    .replacen(l, 1, "\x1b[36m${1}\x1b[0m${2}\x1b[32m${3}\x1b[0m")
                    .into_owned();
                rest.replacen(&l, 1, "${1}\x1b[33m${3}\x1b[0m").into_owned()
            })
            .collect();
        outln(&painted.join("\n"));
    } else {
        outln(&rows);
    }
    status
}
