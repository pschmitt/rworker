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

/// `start WORKER [OPTIONS] -- COMMAND`. An explicit `--topic ''` counts as
/// a topic: no warning.
pub fn cmd_start(ctx: &Ctx, args: &[String]) -> R {
    let (args, wait) = super::files::take_wait(args);
    if let Some(timeout) = wait {
        let worker = args.first().cloned().unwrap_or_default();
        super::files::wait_for_worker(ctx, &[worker], timeout)?;
    }
    if !args
        .iter()
        .take_while(|arg| *arg != "--")
        .any(|arg| arg == "--topic")
    {
        ctx.err(&format!("{} no topic provided", paint::warn_mark(ctx)));
    }
    let mut remote = vec!["start".to_string()];
    remote.extend_from_slice(&args);
    jobs_remote(ctx, &remote)
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

thread_local! {
    // The SSH key for the jobs commands built now, instead of the log
    // collector's (see `observed_jobs`).
    static IDENTITY: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

pub(super) fn identity_override() -> Option<String> {
    IDENTITY.with(|identity| identity.borrow().clone())
}

/// Poll a bounded group of independent workers and retain request order.
fn poll_jobs(ctx: &Ctx, requests: &[Vec<String>]) -> Vec<R<String>> {
    let requests: Vec<_> = requests.iter().map(|args| (args.clone(), None)).collect();
    poll_jobs_as(ctx, &requests)
}

/// Like `poll_jobs`, each request with its own SSH key when given.
fn poll_jobs_as(ctx: &Ctx, requests: &[(Vec<String>, Option<String>)]) -> Vec<R<String>> {
    let quiet = ctx.quiet.get();
    requests
        .chunks(4)
        .flat_map(|batch| {
            // Prepare commands in order, including host-key and local state writes.
            let commands: Vec<_> = batch
                .iter()
                .map(|(args, identity)| {
                    IDENTITY.with(|current| current.replace(identity.clone()));
                    let command = jobs_command(ctx, args);
                    IDENTITY.with(|current| current.replace(None));
                    command
                })
                .collect();
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

/// Expand search terms without interpreting values belonging to existing flags.
fn expand_log_search(args: &[String]) -> Vec<String> {
    let mut expanded = Vec::with_capacity(args.len());
    let mut values = args.iter();
    while let Some(arg) = values.next() {
        let flag = arg.split_once(':').and_then(|(key, value)| {
            matches!(key, "topic" | "agent" | "agent-id").then_some((key, value))
        });
        if let Some((key, value)) = flag {
            expanded.extend([format!("--{key}"), value.to_owned()]);
        } else {
            expanded.push(arg.clone());
            if crate::help::flags("logs").any(|(name, takes_value)| name == arg && takes_value)
                && let Some(value) = values.next()
            {
                expanded.push(value.clone());
            }
        }
    }
    expanded
}

pub fn cmd_logs(ctx: &Ctx, args: &[String]) -> R {
    let expanded = expand_log_search(args);
    let args = expanded.as_slice();
    // Following in a terminal, the TUI shows the same log without chrome and
    // lets the user switch views; --plain (and any pipe or --json) keeps the
    // stream. Snapshots print and exit as always.
    let plain = args.iter().any(|arg| arg == "--plain");
    let follow = args.iter().any(|arg| arg == "-f" || arg == "--follow")
        && !args.iter().any(|arg| arg == "--cached");
    if follow && !plain && !ctx.json.get() && crate::tui::logs_interactive() {
        return crate::tui::cmd_logs(ctx, args);
    }
    let args: Vec<String> = args
        .iter()
        .filter(|arg| *arg != "--plain")
        .cloned()
        .collect();
    let args = args.as_slice();
    let cached = args.iter().any(|arg| arg == "--cached");
    let mut redact = false;
    let mut redact_regex = None;
    let mut redact_args = Vec::new();
    let mut filters = Vec::new();
    let mut filter_args = Vec::new();
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
            "--filter" | "--grep" | "-g" => {
                let Some(pattern) = args.get(i + 1).filter(|pattern| !pattern.is_empty()) else {
                    ctx.err(&format!("{a} requires a nonempty regex"));
                    return Err(Fail(2));
                };
                filters.push(pattern.clone());
                filter_args.extend([i, i + 1]);
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
    if !filters.is_empty() {
        let rest: Vec<_> = args
            .iter()
            .enumerate()
            .filter_map(|(index, value)| (!filter_args.contains(&index)).then_some(value))
            .collect();
        return super::log_cache::grep(ctx, &rest, &filters);
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
/// The agent the builder gateway records on its leased workers.
const BUILDER_GATEWAY_AGENT: &str = "nix build";

/// Workers with registered jobs to poll or follow: reachable ones, minus
/// the builder gateway's leases. Those serve only the Nix daemon, carry
/// none of the operators' keys, and a refused login gets the controller's
/// address penalized by their sshd, which breaks the gateway's own checks.
fn ready(list: &Value) -> Vec<String> {
    jq::items(list)
        .unwrap_or_default()
        .into_iter()
        .filter(|w| {
            !jq::or_empty(get(w, "ip")).is_empty()
                && !jq::or_empty(get(w, "host_key")).is_empty()
                && get(w, "agent").as_str() != Some(BUILDER_GATEWAY_AGENT)
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

/// Choose only a key known to be authorized on this worker. The console key
/// is installed on the operator's hosted leases and on opted-in pool workers.
fn collector_identity<'a>(
    user: &str,
    worker: &Value,
    console: Option<&'a str>,
    console_owner: Option<&str>,
) -> Option<Option<&'a str>> {
    let name = get(worker, "name").as_str().unwrap_or_default();
    if crate::state::hosted_id(name).is_some() {
        return console_owner
            .filter(|owner| collector_reaches(owner, worker))
            .and(console.map(Some));
    }
    if collector_reaches(user, worker) {
        return Some(None);
    }
    if matches!(
        get(worker, "kind").as_str(),
        None | Some("" | "adhoc" | "unknown")
    ) {
        return None;
    }
    console.map(Some)
}

/// The states and the topics of the jobs on the workers of INVENTORY.
/// The polled workers' job states and topics, and each worker's raw job
/// lines (for the job cache).
pub(super) type Observed = (
    crate::event_history::Jobs,
    crate::event_history::Jobs,
    BTreeMap<String, Vec<String>>,
);

pub(super) fn observed_jobs(ctx: &Ctx, inventory: &Value) -> Observed {
    let mut names = ready(inventory);
    names.retain(|name| !name.is_empty());
    names.sort_unstable();
    names.dedup();
    // The collector polls every few seconds. Its own key is only on its
    // operator's ad-hoc workers, and logging in with a key a worker refuses
    // makes sshd penalize the controller's address for everyone. Other
    // Pool workers and hosted leases owned by the console operator use
    // RWORKER_POOL_IDENTITY, when configured, and are skipped otherwise.
    let mut identities: Vec<Option<String>> = vec![None; names.len()];
    if std::env::var_os("RWORKER_LOG_CACHE_IDENTITY").is_some() {
        let console = std::env::var("RWORKER_POOL_IDENTITY")
            .ok()
            .filter(|path| !path.is_empty());
        let console_owner = std::env::var("RWORKER_WEB_WORKER_OWNER").ok();
        let workers = jq::items(inventory).unwrap_or_default();
        let mut kept = Vec::new();
        identities.clear();
        for name in names {
            let worker = workers
                .iter()
                .find(|worker| get(worker, "name").as_str() == Some(&name));
            if let Some(identity) = worker.and_then(|worker| {
                collector_identity(
                    &ctx.user,
                    worker,
                    console.as_deref(),
                    console_owner.as_deref(),
                )
            }) {
                kept.push(name);
                identities.push(identity.map(str::to_owned));
            }
        }
        names = kept;
    }
    let requests: Vec<_> = names
        .iter()
        .zip(identities)
        .map(|(name, identity)| (s(&["jobs", name]), identity))
        .collect();
    let mut observed = (BTreeMap::new(), BTreeMap::new(), BTreeMap::new());
    for (worker, output) in names.into_iter().zip(poll_jobs_as(ctx, &requests)) {
        let Ok(output) = output else {
            continue;
        };
        let (mut states, mut topics) = (BTreeMap::new(), BTreeMap::new());
        for [job, state, _, topic] in output.lines().filter_map(JobSnapshot::fields) {
            states.insert(job.to_owned(), state.to_owned());
            if !topic.is_empty() {
                topics.insert(job.to_owned(), topic.to_owned());
            }
        }
        observed.0.insert(worker.clone(), states);
        observed.1.insert(worker.clone(), topics);
        observed.2.insert(
            worker,
            output
                .lines()
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect(),
        );
    }
    observed
}

/// The job collector's last poll of every worker it reaches, on the control
/// host: `jobs` reads it instead of polling each worker over SSH.
#[derive(serde::Deserialize, serde::Serialize)]
struct JobCache {
    polled_at: i64,
    /// Whose workers were polled: the cache answers only that user.
    operator: String,
    /// Each worker's job lines (job, state, start time, topic).
    rows: BTreeMap<String, Vec<String>>,
}

const JOB_CACHE_FILE: &str = "jobs.json";
/// Older than this, the collector has stalled and `jobs` polls itself.
const JOB_CACHE_MAX_AGE: i64 = 30;

fn job_cache_dir() -> std::path::PathBuf {
    crate::event_history::directory()
        .unwrap_or_else(|| std::path::PathBuf::from("/var/lib/rworker-events"))
}

fn login_name() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_default()
}

/// Store the collector's poll. A worker that did not answer this time keeps
/// its rows from the last poll while it is still in the inventory.
pub(super) fn write_job_cache(
    inventory: &Value,
    mut rows: BTreeMap<String, Vec<String>>,
) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let dir = job_cache_dir();
    if let Some(previous) = read_job_cache_file(&dir) {
        keep_unanswered(&mut rows, &previous.rows, &ready(inventory));
    }
    let cache = JobCache {
        polled_at: jiff::Timestamp::now().as_second(),
        operator: login_name(),
        rows,
    };
    let temporary = dir.join(format!(".{JOB_CACHE_FILE}.{}", std::process::id()));
    let mut file = std::fs::File::create(&temporary)?;
    // The web console reads it as the controller's group.
    file.set_permissions(std::fs::Permissions::from_mode(0o640))?;
    file.write_all(&serde_json::to_vec(&cache).map_err(std::io::Error::other)?)?;
    file.sync_all()?;
    std::fs::rename(&temporary, dir.join(JOB_CACHE_FILE))
}

/// Keep the last rows of live workers that this poll did not answer.
fn keep_unanswered(
    rows: &mut BTreeMap<String, Vec<String>>,
    previous: &BTreeMap<String, Vec<String>>,
    live: &[String],
) {
    for name in live {
        if !rows.contains_key(name)
            && let Some(kept) = previous.get(name)
        {
            rows.insert(name.clone(), kept.clone());
        }
    }
}

fn read_job_cache_file(dir: &std::path::Path) -> Option<JobCache> {
    let file = std::fs::File::open(dir.join(JOB_CACHE_FILE)).ok()?;
    serde_json::from_reader(std::io::BufReader::new(file)).ok()
}

/// The cache when it is fresh.
fn fresh_job_cache() -> Option<JobCache> {
    let cache = read_job_cache_file(&job_cache_dir())?;
    (jiff::Timestamp::now().as_second() - cache.polled_at <= JOB_CACHE_MAX_AGE).then_some(cache)
}

/// The cached jobs as `jobs --json` rows, for the web console.
#[cfg(feature = "web")]
pub(crate) fn cached_job_records() -> Option<Vec<Value>> {
    let cache = fresh_job_cache()?;
    let snapshot = JobSnapshot::from_cache(&cache);
    serde_json::from_str(&snapshot.json()).ok()
}

/// `__cached-jobs` on the control host: `WORKER<TAB>job line` for every
/// cached job; it fails when the cache is stale or another user's.
pub fn cached_jobs_command() -> R {
    let cache = fresh_job_cache().ok_or(Fail(1))?;
    if cache.operator != login_name() {
        return Err(Fail(1));
    }
    let mut out = String::new();
    for (worker, lines) in &cache.rows {
        for line in lines {
            out.push_str(worker);
            out.push('\t');
            out.push_str(line);
            out.push('\n');
        }
    }
    crate::ctx::out(&out);
    Ok(())
}

/// The control host's job cache, read over SSH (a few milliseconds instead
/// of a poll of every worker, which waits for the slowest one).
fn cached_snapshot(ctx: &Ctx) -> Option<JobSnapshot> {
    // On the control host itself, read the file instead of SSH to it.
    if std::env::var_os("RWORKER_CONTROL_SOCKET").is_some() {
        let cache = fresh_job_cache()?;
        return Some(JobSnapshot::from_cache(&cache));
    }
    let output = state::control_ssh_cmd(ctx, &["__cached-jobs"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    let text = String::from_utf8(output.stdout).ok()?;
    let rows: Vec<(String, String)> = text
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(worker, line)| (worker.to_owned(), line.to_owned()))
        .collect();
    let workers = rows
        .iter()
        .map(|(worker, _)| worker)
        .collect::<BTreeSet<_>>()
        .len();
    Some(JobSnapshot {
        rows,
        failed: 0,
        workers,
    })
}

pub(super) fn registered_jobs(ctx: &Ctx, inventory: &Value) -> Vec<(String, String)> {
    observed_jobs(ctx, inventory)
        .0
        .into_iter()
        .flat_map(|(worker, jobs)| jobs.into_keys().map(move |job| (worker.clone(), job)))
        .collect()
}

struct JobSnapshot {
    rows: Vec<(String, String)>,
    failed: usize,
    workers: usize,
}

fn job_snapshot(ctx: &Ctx, worker: Option<&str>, fresh: bool) -> R<JobSnapshot> {
    if worker.is_none()
        && !fresh
        && let Some(snapshot) = cached_snapshot(ctx)
    {
        return Ok(snapshot);
    }
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
    fn from_cache(cache: &JobCache) -> Self {
        let rows = cache
            .rows
            .iter()
            .flat_map(|(worker, lines)| lines.iter().map(|line| (worker.clone(), line.clone())))
            .collect();
        Self {
            rows,
            failed: 0,
            workers: cache.rows.len(),
        }
    }

    fn failure(&self) -> Option<String> {
        (self.failed > 0).then(|| {
            format!(
                "Could not list jobs on {} of {} workers",
                self.failed, self.workers
            )
        })
    }

    /// Job, state, start time and topic (empty when none was given).
    fn fields(line: &str) -> Option<[&str; 4]> {
        let mut fields = line.splitn(4, '\t');
        Some([
            fields.next()?,
            fields.next()?,
            fields.next()?,
            fields.next().unwrap_or_default(),
        ])
    }

    fn json(&self) -> String {
        let records: Vec<_> = self
            .rows
            .iter()
            .filter_map(|(worker, line)| {
                let [job, state, started, topic] = Self::fields(line)?;
                Some(serde_json::json!({
                    "worker": worker, "job": job, "topic": topic, "state": state,
                    "started_at": started,
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
            vec!["WORKER", "JOB", "TOPIC", "STATE", "STARTED"]
        } else {
            vec!["JOB", "TOPIC", "STATE", "STARTED"]
        }];
        rows.extend(self.rows.iter().filter_map(|(worker, line)| {
            let [job, state, started, topic] = Self::fields(line)?;
            let topic = if topic.is_empty() { "-" } else { topic };
            let mut cells = vec![job, topic, state, started];
            if all {
                cells.insert(0, worker);
            }
            Some(cells)
        }));
        if rows.len() == 1 {
            return "No jobs found.".to_string();
        }
        let state_col = if all { 3 } else { 2 };
        crate::table::styled_columns(ctx, &rows, |col, text| match col.cmp(&state_col) {
            std::cmp::Ordering::Less if col + 1 == state_col => text.to_owned(),
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
    let mut fresh = false;
    let mut seconds = 5;
    let mut args = args.iter().map(String::as_str);
    while let Some(arg) = args.next() {
        match arg {
            "-w" | "--watch" => watching = true,
            "--fresh" => fresh = true,
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
        return watch_jobs(ctx, worker, seconds, terminal && !ctx.json.get(), fresh);
    }
    if let Some(worker) = worker
        && !terminal
        && !ctx.json.get()
    {
        return jobs_remote(ctx, &s(&["jobs", worker]));
    }
    let snapshot = job_snapshot(ctx, worker, fresh)?;
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

fn watch_jobs(ctx: &Ctx, worker: Option<&str>, seconds: u64, interactive: bool, fresh: bool) -> R {
    crate::signals::trap();
    let jobs_changed = super::events::job_signal();
    let mut screen = interactive.then(super::watch::Screen::enter);
    let mut last = JobSnapshot {
        rows: Vec::new(),
        failed: 0,
        workers: 0,
    };
    while crate::signals::pending().is_none() {
        let error = match ctx.muted(|| job_snapshot(ctx, worker, fresh)) {
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
        if !super::watch::wait_or(seconds as f64, &jobs_changed) {
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
                // The worker's account, from what this host remembered of it.
                let user = crate::state::worker_field(ctx, worker, "user")
                    .ok()
                    .filter(|user| !user.is_empty())
                    .unwrap_or_else(|| crate::worker_user::LEGACY_OPENSTACK_USER.to_owned());
                format!("{user}@{worker}")
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
    fn job_cache_keeps_rows_of_live_workers_that_did_not_answer() {
        let rows = |pairs: &[(&str, &str)]| -> BTreeMap<String, Vec<String>> {
            pairs
                .iter()
                .map(|(worker, line)| ((*worker).to_owned(), vec![(*line).to_owned()]))
                .collect()
        };
        let previous = rows(&[("a", "old-a"), ("b", "old-b"), ("gone", "old-gone")]);
        let mut polled = rows(&[("a", "new-a")]);
        keep_unanswered(
            &mut polled,
            &previous,
            &["a".into(), "b".into(), "c".into()],
        );
        // a answered, b kept from the last poll, gone left the inventory.
        assert_eq!(polled, rows(&[("a", "new-a"), ("b", "old-b")]));
    }

    #[test]
    fn job_cache_rows_become_a_snapshot() {
        let cache = JobCache {
            polled_at: 0,
            operator: "op".into(),
            rows: [(
                "w1".to_owned(),
                vec!["job-1\trunning\t2026-11-25T12:00:00Z\tbuild".to_owned()],
            )]
            .into_iter()
            .collect(),
        };
        let snapshot = JobSnapshot::from_cache(&cache);
        assert_eq!(snapshot.workers, 1);
        assert_eq!(snapshot.failed, 0);
        let records: Vec<Value> = serde_json::from_str(&snapshot.json()).unwrap();
        assert_eq!(records[0]["worker"], "w1");
        assert_eq!(records[0]["state"], "running");
        assert_eq!(records[0]["topic"], "build");
    }

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

    #[test]
    fn collector_uses_console_key_for_own_hosted_leases() {
        let hosted = json!({"name":"gha-15d6fb35", "kind":"adhoc", "owner":"operator@phone"});
        assert_eq!(
            collector_identity("operator", &hosted, Some("console-key"), Some("operator")),
            Some(Some("console-key"))
        );
        assert_eq!(
            collector_identity("operator", &hosted, None, Some("operator")),
            None
        );
        assert_eq!(
            collector_identity("operator", &hosted, Some("console-key"), Some("other")),
            None
        );
        let vm = json!({"name":"adhoc-12345678", "kind":"adhoc", "owner":"operator@phone"});
        assert_eq!(
            collector_identity("operator", &vm, Some("console-key"), Some("operator")),
            Some(None)
        );
        let pool = json!({"name":"circus-12345678", "kind":"circus", "owner":"circus@control"});
        assert_eq!(
            collector_identity("operator", &pool, Some("console-key"), Some("operator")),
            Some(Some("console-key"))
        );
    }

    #[test]
    fn expands_log_search_terms_without_changing_flag_values() {
        let args = [
            "-f",
            "topic:nixpp",
            "agent:codex",
            "agent-id:!old",
            "-g",
            "topic:literal",
        ]
        .map(str::to_owned);
        assert_eq!(
            expand_log_search(&args),
            [
                "-f",
                "--topic",
                "nixpp",
                "--agent",
                "codex",
                "--agent-id",
                "!old",
                "-g",
                "topic:literal"
            ]
        );
        assert_eq!(expand_log_search(&["topic:".into()]), ["--topic", ""]);
        assert_eq!(
            expand_log_search(&["worker-name".into(), "job-name".into()]),
            ["worker-name", "job-name"]
        );
    }

    #[test]
    fn job_lines_carry_an_optional_topic() {
        assert_eq!(
            JobSnapshot::fields("build\trunning\ttoday\trelease build"),
            Some(["build", "running", "today", "release build"])
        );
        assert_eq!(
            JobSnapshot::fields("build\trunning\ttoday\t").unwrap()[3],
            ""
        );
        assert_eq!(JobSnapshot::fields("build\trunning\ttoday").unwrap()[3], "");
    }

    #[test]
    fn job_manager_records_topics() {
        let home = std::env::temp_dir().join(format!("rworker-jobs-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let manager = |args: &[&str]| {
            let out = Command::new("bash")
                .args(["-c", JOBS_SH, "--"])
                .args(args)
                .env("HOME", &home)
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}");
            String::from_utf8(out.stdout).unwrap()
        };
        let log = home.join("x.log");
        std::fs::write(&log, "").unwrap();
        let log = log.to_str().unwrap();
        manager(&["start", "--name", "a", "--topic", "one\ttwo", "--", "true"]);
        manager(&["start", "--name", "b", "--topic", "", "--", "true"]);
        manager(&["register", "c", "--log", log, "--topic", "a log"]);
        manager(&["register", "d", "--log", log]);
        let listing = manager(&["jobs"]);
        let topics: Vec<_> = listing
            .lines()
            .map(|line| JobSnapshot::fields(line).unwrap()[3])
            .collect();
        assert_eq!(topics, ["one two", "", "a log", ""]);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn builder_gateway_leases_are_not_polled() {
        let list = json!([
            {"name": "adhoc-1", "ip": "192.0.2.1", "host_key": "k", "agent": "claude"},
            {"name": "adhoc-2", "ip": "192.0.2.2", "host_key": "k", "agent": "nix build"},
            {"name": "adhoc-3", "ip": "", "host_key": "k", "agent": "claude"},
        ]);
        assert_eq!(ready(&list), ["adhoc-1"]);
    }
}
