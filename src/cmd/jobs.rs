//! Registered jobs on workers: start, jobs, register, logs, ps and the
//! lifecycle events. The job manager itself runs on the worker
//! (`src/jobs.sh`, fed to `bash -s` over the pinned SSH connection).

use crate::ctx::{Ctx, Fail, R, outln};
use crate::jq::{self, get};
use crate::paint;
use crate::proc;
use crate::signals;
use crate::state;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::io::IsTerminal;
use std::process::{Child, Command, Stdio};

pub const JOBS_SH: &str = include_str!("../jobs.sh");

/// The ssh command that runs the job manager's ACTION on WORKER.
pub(super) fn jobs_command(ctx: &Ctx, args: &[String]) -> R<Command> {
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
    let cached = args.iter().any(|arg| arg == "--cached");
    let mut redact = false;
    let mut redact_regex = None;
    let mut redact_args = Vec::new();
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
            "--redact" => {
                redact = true;
                redact_args.push(i);
                i += 1;
            }
            "--redact-regex" => {
                let Some(pattern) = args.get(i + 1).filter(|pattern| !pattern.is_empty()) else {
                    ctx.err("--redact-regex requires a nonempty regex");
                    return Err(Fail(2));
                };
                redact = true;
                redact_regex = Some(pattern.as_str());
                redact_args.extend([i, i + 1]);
                i += 2;
            }
            "--cached" => i += 1,
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
    if redact {
        let unfiltered: Vec<_> = args
            .iter()
            .enumerate()
            .filter_map(|(index, value)| (!redact_args.contains(&index)).then_some(value))
            .collect();
        return super::log_cache::redact(ctx, &unfiltered, redact_regex);
    }
    // Options may come before or after WORKER [JOB]: `logs -f WORKER` too.
    let cache_lines = match opts.as_slice() {
        [] => Some(200),
        [flag, count] if flag == "--lines" => count.parse::<usize>().ok(),
        _ => None,
    };
    if cached && (!(1..=2).contains(&positional.len()) || cache_lines.is_none()) {
        ctx.err("--cached requires WORKER [JOB] and optionally --lines N");
        return Err(Fail(2));
    }
    let cache_lines = cache_lines.filter(|_| (1..=2).contains(&positional.len()));
    let read_cache = |lines: usize| {
        super::log_cache::client_read(
            ctx,
            &positional[0],
            positional.get(1).map_or("", String::as_str),
            &lines.to_string(),
        )
    };
    let prefer_cache = cached || std::env::var_os("RWORKER_LOG_CACHE_READ").is_some();
    if let Some(lines) = cache_lines.filter(|_| prefer_cache) {
        let result = read_cache(lines);
        if result.is_ok() {
            return result;
        }
        if cached {
            ctx.err(
                "No cached logs available. Check the exact worker name and control-host account.",
            );
            return result;
        }
    }
    if !positional.is_empty() {
        let mut all = vec!["logs".to_string(), positional[0].clone()];
        all.extend(positional[1..].iter().cloned());
        all.extend(opts);
        if let Some(lines) = cache_lines.filter(|_| !prefer_cache) {
            let read = || read_cache(lines);
            match ctx.muted(|| jobs_command(ctx, &all)) {
                Ok(mut cmd) => {
                    let result = proc::run_with_input(ctx, &mut cmd, JOBS_SH.as_bytes())
                        .and_then(crate::ctx::status);
                    return if result.is_err() && ctx.muted(read).is_ok() {
                        Ok(())
                    } else {
                        result
                    };
                }
                Err(_) if ctx.muted(read).is_ok() => return Ok(()),
                Err(_) => {}
            }
        }
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
                crate::help::show(ctx, "logs");
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

/// Whether the log collector's key is on WORKER: the controller adds it only
/// to ad-hoc workers of the collector's operator, which runs the collector.
fn collector_reaches(user: &str, worker: &Value) -> bool {
    get(worker, "kind").as_str() == Some("adhoc")
        && jq::raw(get(worker, "owner")).split('@').next() == Some(user)
}

pub(super) fn observed_jobs(ctx: &Ctx, inventory: &Value) -> crate::event_history::Jobs {
    let mut names = ready(inventory);
    // The collector polls every few seconds; logging in with a key a worker
    // refuses makes sshd penalize the controller's address for everyone.
    if std::env::var_os("RWORKER_LOG_CACHE_IDENTITY").is_some() {
        let reachable: Vec<String> = jq::items(inventory)
            .unwrap_or_default()
            .into_iter()
            .filter(|worker| collector_reaches(&ctx.user, worker))
            .map(|worker| jq::raw(get(worker, "name")))
            .collect();
        names.retain(|name| reachable.contains(name));
    }
    names.retain(|name| !name.is_empty());
    names.sort_unstable();
    names.dedup();
    let requests: Vec<_> = names.iter().map(|name| s(&["jobs", name])).collect();
    names
        .into_iter()
        .zip(poll_jobs(ctx, &requests))
        .filter_map(|(worker, output)| {
            let states = output
                .ok()?
                .lines()
                .filter_map(|line| {
                    let mut fields = line.split('\t');
                    let job = fields.next()?;
                    let state = fields.next()?;
                    Some((job.to_owned(), state.to_owned()))
                })
                .collect();
            Some((worker, states))
        })
        .collect()
}

pub(super) fn registered_jobs(ctx: &Ctx, inventory: &Value) -> Vec<(String, String)> {
    observed_jobs(ctx, inventory)
        .into_iter()
        .flat_map(|(worker, jobs)| jobs.into_keys().map(move |job| (worker.clone(), job)))
        .collect()
}

struct JobSnapshot {
    rows: Vec<(String, String)>,
    failed: usize,
    workers: usize,
}

fn job_snapshot(ctx: &Ctx, worker: Option<&str>) -> R<JobSnapshot> {
    let mut names = match worker {
        Some(name) => vec![name.to_owned()],
        None => ready(&workers(ctx, "", "")?),
    };
    names.retain(|name| !name.is_empty());
    names.sort_unstable();
    names.dedup();
    let requests: Vec<_> = names.iter().map(|name| s(&["jobs", name])).collect();
    let mut snapshot = JobSnapshot {
        rows: Vec::new(),
        failed: 0,
        workers: names.len(),
    };
    for (worker, output) in names.into_iter().zip(poll_jobs(ctx, &requests)) {
        match output {
            Ok(output) => snapshot.rows.extend(
                output
                    .lines()
                    .filter(|line| !line.is_empty())
                    .map(|line| (worker.clone(), line.to_owned())),
            ),
            Err(_) => snapshot.failed += 1,
        }
    }
    Ok(snapshot)
}

impl JobSnapshot {
    fn failure(&self) -> Option<String> {
        (self.failed > 0).then(|| {
            format!(
                "Could not list jobs on {} of {} workers",
                self.failed, self.workers
            )
        })
    }

    fn fields(line: &str) -> Option<(&str, &str, &str)> {
        let (job, fields) = line.split_once('\t')?;
        let (state, started) = fields.split_once('\t')?;
        Some((job, state, started))
    }

    fn json(&self) -> String {
        let records: Vec<_> = self
            .rows
            .iter()
            .filter_map(|(worker, line)| {
                let (job, state, started) = Self::fields(line)?;
                Some(serde_json::json!({
                    "worker": worker, "job": job, "state": state, "started_at": started,
                }))
            })
            .collect();
        Value::Array(records).to_string()
    }

    fn tsv(&self, all: bool) {
        for (worker, line) in &self.rows {
            if all {
                outln(&format!("{worker}\t{line}"));
            } else {
                outln(line);
            }
        }
    }

    fn table(&self, ctx: &Ctx, all: bool) -> String {
        let mut rows = vec![if all {
            vec!["WORKER", "JOB", "STATE", "STARTED"]
        } else {
            vec!["JOB", "STATE", "STARTED"]
        }];
        rows.extend(self.rows.iter().filter_map(|(worker, line)| {
            let (job, state, started) = Self::fields(line)?;
            let mut cells = vec![job, state, started];
            if all {
                cells.insert(0, worker);
            }
            Some(cells)
        }));
        if rows.len() == 1 {
            return "No jobs found.".to_string();
        }
        let state_col = if all { 2 } else { 1 };
        crate::table::styled_columns(ctx, &rows, |col, text| match col.cmp(&state_col) {
            std::cmp::Ordering::Less => paint::vm(ctx, text),
            std::cmp::Ordering::Equal if text == "exit 0" => paint::paint(ctx, "2", text),
            std::cmp::Ordering::Equal => paint::value(ctx, text),
            std::cmp::Ordering::Greater => paint::paint(ctx, "2", text),
        })
    }
}

pub fn cmd_jobs(ctx: &Ctx, args: &[String]) -> R {
    let mut worker = None;
    let mut watching = false;
    let mut seconds = 5;
    let mut args = args.iter().map(String::as_str);
    while let Some(arg) = args.next() {
        match arg {
            "-w" | "--watch" => watching = true,
            "-n" | "--interval" => {
                let Some(n) = args.next().and_then(super::watch::interval) else {
                    ctx.err("Interval must be a whole number of seconds");
                    return Err(Fail(2));
                };
                seconds = n;
            }
            _ if !arg.starts_with('-') && worker.is_none() => worker = Some(arg),
            _ => {
                crate::help::show(ctx, "jobs");
                return Err(Fail(2));
            }
        }
    }
    let terminal = std::io::stdout().is_terminal();
    if watching {
        return watch_jobs(ctx, worker, seconds, terminal && !ctx.json.get());
    }
    if let Some(worker) = worker
        && !terminal
        && !ctx.json.get()
    {
        return jobs_remote(ctx, &s(&["jobs", worker]));
    }
    let snapshot = job_snapshot(ctx, worker)?;
    if ctx.json.get() {
        crate::ctx::json_out(ctx, &snapshot.json());
    } else if terminal {
        outln(&snapshot.table(ctx, worker.is_none()));
    } else {
        snapshot.tsv(worker.is_none());
    }
    if let Some(error) = snapshot.failure() {
        ctx.err(&error);
        if snapshot.failed == snapshot.workers {
            return Err(Fail(1));
        }
    }
    Ok(())
}

fn watch_jobs(ctx: &Ctx, worker: Option<&str>, seconds: u64, interactive: bool) -> R {
    crate::signals::trap();
    let mut screen = interactive.then(super::watch::Screen::enter);
    let mut last = JobSnapshot {
        rows: Vec::new(),
        failed: 0,
        workers: 0,
    };
    while crate::signals::pending().is_none() {
        let error = match ctx.muted(|| job_snapshot(ctx, worker)) {
            Ok(snapshot) => {
                let error = snapshot.failure();
                if snapshot.failed == 0 || snapshot.failed < snapshot.workers {
                    last = snapshot;
                }
                error
            }
            Err(_) => Some("Could not refresh jobs".to_owned()),
        };
        if let Some(screen) = &mut screen {
            let target = worker.map_or(String::new(), |name| format!(" {name}"));
            let mut frame = format!(
                "Every {seconds}s: rworker jobs{target}    {}    {} jobs    (q quits, any key refreshes)\n",
                crate::time::now_hms(),
                last.rows.len(),
            );
            if let Some(error) = error {
                frame += &format!("{}\n", paint::paint(ctx, "33", &error));
            }
            frame += &format!("\n{}\n", last.table(ctx, worker.is_none()));
            screen.draw(&frame);
        } else {
            if let Some(error) = &error {
                ctx.err(error);
            }
            if ctx.json.get() {
                crate::ctx::json_out(ctx, &last.json());
            } else {
                last.tsv(worker.is_none());
            }
        }
        if !super::watch::wait(seconds as f64) {
            break;
        }
    }
    screen.map_or(Ok(()), super::watch::Screen::leave)
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
        cmd.args(["events", "--watch", "--since-now", "--interval", "5"]);
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

pub fn cmd_ps(ctx: &Ctx, args: &[String]) -> R {
    let mut target = String::new();
    let mut long = false;
    for a in args {
        match a.as_str() {
            "--long" => long = true,
            s if s.starts_with('-') => {
                crate::help::show(ctx, "ps");
                return Err(Fail(2));
            }
            s => {
                if !target.is_empty() {
                    crate::help::show(ctx, "ps");
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
    if ctx.color {
        let cells: Vec<_> = rows
            .lines()
            .map(|row| row.splitn(6, '\t').collect())
            .collect();
        outln(&crate::table::styled_columns(
            ctx,
            &cells,
            |col, text| match col {
                0 | 1 => paint::vm(ctx, text),
                2 | 3 => paint::paint(ctx, "34", text),
                4 => paint::paint(ctx, "2", text),
                _ => text.to_owned(),
            },
        ));
    } else {
        outln(&jq::column(&rows));
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn collector_polls_only_its_operators_ad_hoc_workers() {
        let own = json!({"name": "adhoc-1", "kind": "adhoc", "owner": "pschmitt@fnuc"});
        let other = json!({"name": "adhoc-2", "kind": "adhoc", "owner": "alice@laptop"});
        let pool = json!({"name": "circus-1", "kind": "circus", "owner": "circus@rofl-10"});
        assert!(collector_reaches("pschmitt", &own));
        assert!(!collector_reaches("pschmitt", &other));
        assert!(!collector_reaches("pschmitt", &pool));
        assert!(!collector_reaches("circus", &pool));
    }
}
