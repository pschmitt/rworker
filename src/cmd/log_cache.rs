//! Per-operator cache of registered job logs on the control host.

use crate::ctx::{Ctx, Fail, R};
use std::fs::{self, File, FileTimes, OpenOptions};
use std::io::{self, BufRead, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const EXPORT_SH: &[u8] = include_bytes!("../job-log-export.sh");
const MIB: u64 = 1024 * 1024;

pub(super) fn component(name: &str) -> bool {
    name.len() <= 255
        && name
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}

fn root(ctx: &Ctx) -> PathBuf {
    ctx.state_dir.join("job-logs")
}

struct Entry {
    path: PathBuf,
    size: u64,
    modified: SystemTime,
}

fn entries(root: &Path) -> io::Result<Vec<Entry>> {
    let mut result = Vec::new();
    for worker in fs::read_dir(root)? {
        let worker = worker?;
        if !worker.file_type()?.is_dir() {
            continue;
        }
        for job in fs::read_dir(worker.path())? {
            let job = job?;
            if !job.file_type()?.is_file() || !job.file_name().to_str().is_some_and(component) {
                continue;
            }
            let meta = job.metadata()?;
            result.push(Entry {
                path: job.path(),
                size: meta.len(),
                modified: meta.modified()?,
            });
        }
    }
    Ok(result)
}

fn remove(entry: &Entry) -> io::Result<()> {
    fs::remove_file(&entry.path)?;
    // Readers can race with pruning. They open the log before emitting output.
    if let Some(parent) = entry.path.parent() {
        let _ = fs::remove_dir(parent);
    }
    Ok(())
}

fn prune(root: &Path, age: Duration, budget: u64, incoming: Option<&Entry>) -> io::Result<bool> {
    let mut existing = entries(root)?;
    let previous = existing
        .iter()
        .position(|entry| incoming.is_some_and(|new| new.path == entry.path))
        .map(|index| existing.swap_remove(index));
    let mut ranked: Vec<_> = existing.iter().map(|entry| (entry, false)).collect();
    ranked.extend(incoming.map(|entry| (entry, true)));
    ranked.sort_unstable_by_key(|(entry, incoming)| (entry.modified, *incoming, &entry.path));
    let mut size: u64 = ranked.iter().map(|(entry, _)| entry.size).sum();
    let mut keep_incoming = true;
    for (entry, incoming) in ranked {
        if entry.modified.elapsed().unwrap_or_default() >= age || size > budget {
            if incoming {
                keep_incoming = false;
                if let Some(previous) = &previous {
                    remove(previous)?;
                }
            } else {
                remove(entry)?;
            }
            size -= entry.size;
        }
    }
    Ok(keep_incoming)
}

pub(super) fn private_dir(path: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

pub(super) fn put(root: &Path, path: &Path, bytes: &[u8], modified: SystemTime) -> io::Result<()> {
    let parent = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
    private_dir(parent)?;
    let tmp = root.join(".partial");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    let result = file
        .write_all(bytes)
        .and_then(|()| file.set_times(FileTimes::new().set_modified(modified)))
        .and_then(|()| fs::rename(&tmp, path));
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

fn capture(command: Command, limit: u64) -> io::Result<Vec<u8>> {
    let mut timeout = Command::new("timeout");
    timeout
        .args(["--kill-after=2s", "20s"])
        .arg(command.get_program())
        .args(command.get_args());
    let mut child = timeout
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let result = (|| {
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(EXPORT_SH)?;
        }
        let mut bytes = Vec::new();
        if let Some(stdout) = child.stdout.take() {
            stdout.take(limit + 1).read_to_end(&mut bytes)?;
        }
        if bytes.len() as u64 > limit {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(bytes)
    })();
    let status = child.wait()?;
    result.and_then(|bytes| {
        if status.success() {
            Ok(bytes)
        } else {
            Err(io::Error::other("worker log retrieval failed"))
        }
    })
}

fn limits(age: &str, budget: &str, limit: &str) -> R<(Duration, u64, u64)> {
    let parse = |v: &str| v.parse::<u64>().ok().filter(|v| *v > 0).ok_or(Fail(2));
    let age = Duration::from_secs(parse(age)?.checked_mul(86400).ok_or(Fail(2))?);
    let budget = parse(budget)?.checked_mul(MIB).ok_or(Fail(2))?;
    let limit = parse(limit)?
        .checked_mul(MIB)
        .filter(|v| *v <= budget && *v <= 256 * MIB)
        .ok_or(Fail(2))?;
    Ok((age, budget, limit))
}

fn lock(root: &Path) -> io::Result<File> {
    private_dir(root)?;
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(root.join(".lock"))?;
    use std::os::fd::AsRawFd;
    // SAFETY: flock borrows the live descriptor. Dropping the file releases the lock.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}

pub fn collect(ctx: &Ctx, args: &[String]) -> R {
    let [age, budget, limit] = args else {
        return Err(Fail(2));
    };
    let (age, budget, limit) = limits(age, budget, limit)?;
    let root = root(ctx);
    let report = |err: io::Error| {
        ctx.err(&format!("log cache: {err}"));
        Fail(1)
    };
    {
        let _lock = lock(&root).map_err(report)?;
        if let Err(err) = fs::remove_file(root.join(".partial"))
            && err.kind() != io::ErrorKind::NotFound
        {
            return Err(report(err));
        }
        prune(&root, age, budget, None).map_err(report)?;
    }
    let inventory: serde_json::Value =
        serde_json::from_str(&crate::state::remote_ls(ctx)?).map_err(|_| Fail(1))?;
    {
        let _lock = lock(&root).map_err(report)?;
        super::history::observe(ctx, &inventory)?;
    }
    let jobs = super::jobs::registered_jobs(ctx, &inventory)?;
    let cutoff = (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default())
    .saturating_sub(age)
    .as_secs()
    .to_string();
    let mut failed = 0;
    for batch in jobs.chunks(4) {
        // Resolve host keys and addresses on one thread so the local SSH
        // state has a single writer. The actual reads are independent.
        let commands: Vec<_> = batch
            .iter()
            .map(|(worker, job)| {
                if !component(worker) || !component(job) {
                    return None;
                }
                let previous = fs::metadata(root.join(worker).join(job))
                    .ok()
                    .filter(|meta| meta.len() <= limit)
                    .and_then(|meta| meta.modified().ok()?.duration_since(UNIX_EPOCH).ok())
                    .map(|time| format!("{}.{:09}", time.as_secs(), time.subsec_nanos()))
                    .unwrap_or_default();
                let args = [
                    "log-cache-tail".into(),
                    worker.clone(),
                    job.clone(),
                    limit.to_string(),
                    previous,
                    cutoff.clone(),
                ];
                ctx.muted(|| super::jobs::jobs_command(ctx, &args)).ok()
            })
            .collect();
        let output = std::thread::scope(|scope| {
            let handles: Vec<_> = commands
                .into_iter()
                .map(|command| scope.spawn(move || capture(command?, limit + 32).ok()))
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().ok().flatten())
                .collect::<Vec<_>>()
        });
        for ((worker, job), data) in batch.iter().zip(output) {
            if data.as_ref().is_some_and(Vec::is_empty) {
                continue;
            }
            let Some((modified, data)) = data.as_deref().and_then(|data| snapshot(data, limit))
            else {
                failed += 1;
                continue;
            };
            if modified.elapsed().unwrap_or_default() >= age {
                continue;
            }
            let entry = Entry {
                path: root.join(worker).join(job),
                size: data.len() as u64,
                modified,
            };
            let _lock = lock(&root).map_err(report)?;
            if prune(&root, age, budget, Some(&entry)).map_err(report)? {
                put(&root, &entry.path, data, modified).map_err(report)?;
            }
        }
    }
    if failed > 0 {
        ctx.err(&format!("could not collect {failed} registered job logs"));
        return Err(Fail(1));
    }
    Ok(())
}

const EXEC_TAIL_BYTES: usize = 32 * MIB as usize;

pub(super) fn keep_tail(bytes: &[u8]) -> &[u8] {
    &bytes[bytes.len().saturating_sub(EXEC_TAIL_BYTES)..]
}

pub(super) fn stream_tail(
    mut input: impl Read,
    mut display: Option<impl Write>,
) -> io::Result<Vec<u8>> {
    use std::collections::VecDeque;
    let mut tail = VecDeque::new();
    let mut buf = [0; 16384];
    loop {
        let count = input.read(&mut buf)?;
        if count == 0 {
            break;
        }
        if let Some(writer) = display.as_mut()
            && writer
                .write_all(&buf[..count])
                .and_then(|()| writer.flush())
                .is_err()
        {
            display = None;
        }
        let excess = (tail.len() + count).saturating_sub(EXEC_TAIL_BYTES);
        tail.drain(..excess);
        tail.extend(&buf[..count]);
    }
    Ok(Vec::from(tail))
}

pub(super) fn send_record(
    ctx: &Ctx,
    name: &str,
    info: &serde_json::Value,
    streams: Option<(&[u8], &[u8])>,
) -> R {
    let mut header = serde_json::to_vec(info).map_err(|_| Fail(1))?;
    header.push(b'\n');
    let mut args = vec!["__cached-archive-exec", name];
    if streams.is_none() {
        args.push("created");
    }
    let mut cmd = if std::env::var_os("RWORKER_CONTROL_SOCKET").is_some() {
        let mut cmd = Command::new(std::env::current_exe().map_err(|_| Fail(1))?);
        cmd.args(&args);
        cmd
    } else {
        crate::state::control_ssh_cmd(ctx, &args)
    };
    // The archive consumes SSH stdin.
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().map_err(|_| Fail(1))?;
    let result = (|| -> io::Result<()> {
        let mut input = child.stdin.take().ok_or(io::ErrorKind::BrokenPipe)?;
        input.write_all(&header)?;
        if let Some((stdout, stderr)) = streams {
            input.write_all(&(stdout.len() as u64).to_be_bytes())?;
            input.write_all(stdout)?;
            input.write_all(stderr)?;
        }
        Ok(())
    })();
    let status = child.wait().map_err(|_| Fail(1))?;
    result.map_err(|_| Fail(1))?;
    crate::ctx::status(status.code().unwrap_or(1))
}

pub fn receive_exec(ctx: &Ctx, args: &[String]) -> R {
    let (name, created) = match args {
        [name] => (name, false),
        [name, mode] if mode == "created" => (name, true),
        _ => return Err(Fail(2)),
    };
    if !component(name) {
        return Err(Fail(2));
    }
    let settings = std::env::var("RWORKER_LOG_ARCHIVE_LIMITS").map_err(|_| Fail(1))?;
    let values: Vec<_> = settings.split(':').collect();
    let [age, budget, limit] = values.as_slice() else {
        return Err(Fail(2));
    };
    let (age, budget, limit) = limits(age, budget, limit)?;
    let mut input = io::stdin().lock();
    let mut header = Vec::new();
    (&mut input)
        .take(8192)
        .read_until(b'\n', &mut header)
        .map_err(|_| Fail(1))?;
    if header.last() != Some(&b'\n') {
        return Err(Fail(2));
    }
    let info = serde_json::from_slice(&header).map_err(|_| Fail(2))?;
    if created {
        let _lock = lock(&root(ctx)).map_err(|_| Fail(1))?;
        return super::history::record_created(ctx, name, &info);
    }
    let mut length = [0; 8];
    input.read_exact(&mut length).map_err(|_| Fail(1))?;
    let length = u64::from_be_bytes(length);
    if length > EXEC_TAIL_BYTES as u64 {
        return Err(Fail(2));
    }
    let mut stdout = Vec::new();
    (&mut input)
        .take(length)
        .read_to_end(&mut stdout)
        .map_err(|_| Fail(1))?;
    if stdout.len() as u64 != length {
        return Err(Fail(2));
    }
    let mut stderr = Vec::new();
    input
        .take(EXEC_TAIL_BYTES as u64 + 1)
        .read_to_end(&mut stderr)
        .map_err(|_| Fail(1))?;
    if stderr.len() > EXEC_TAIL_BYTES {
        return Err(Fail(2));
    }
    let root = root(ctx);
    let _lock = lock(&root).map_err(|_| Fail(1))?;
    super::history::record_exec(ctx, name, &info)?;
    let limit = limit as usize;
    let stdout_limit = stdout.len().min(limit - stderr.len().min(limit / 2));
    let stderr_limit = stderr.len().min(limit - stdout_limit);
    let streams = [
        ("exec.stdout", &stdout[stdout.len() - stdout_limit..]),
        ("exec.stderr", &stderr[stderr.len() - stderr_limit..]),
    ];
    for (job, bytes) in streams {
        let entry = Entry {
            path: root.join(name).join(job),
            size: bytes.len() as u64,
            modified: SystemTime::now(),
        };
        if !prune(&root, age, budget, Some(&entry)).map_err(|_| Fail(1))? {
            return Err(Fail(1));
        }
        put(&root, &entry.path, bytes, entry.modified).map_err(|_| Fail(1))?;
    }
    Ok(())
}

fn snapshot(bytes: &[u8], limit: u64) -> Option<(SystemTime, &[u8])> {
    let split = bytes.iter().position(|&byte| byte == b'\n')?;
    let (seconds, nanos) = std::str::from_utf8(&bytes[..split]).ok()?.split_once('.')?;
    if nanos.len() != 9 {
        return None;
    }
    let modified =
        UNIX_EPOCH.checked_add(Duration::new(seconds.parse().ok()?, nanos.parse().ok()?))?;
    let data = &bytes[split + 1..];
    (data.len() as u64 <= limit).then(|| (modified.min(SystemTime::now()), data))
}

fn tail(bytes: &[u8], lines: usize) -> &[u8] {
    if lines == 0 || bytes.is_empty() {
        return &[];
    }
    let without_final_newline = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let start = without_final_newline
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, b)| **b == b'\n')
        .nth(lines - 1)
        .map_or(0, |(i, _)| i + 1);
    &bytes[start..]
}

pub(super) fn client_read(ctx: &Ctx, worker: &str, job: &str, lines: &str) -> R {
    client_read_mode(ctx, "__cached-logs", worker, job, lines)
}

pub fn preview(ctx: &Ctx, args: &[String]) -> R {
    let [worker] = args else {
        return Err(Fail(2));
    };
    client_read_mode(ctx, "__cached-preview", worker, "", "100")
}

fn client_read_mode(ctx: &Ctx, action: &str, worker: &str, job: &str, lines: &str) -> R {
    let preview = action == "__cached-preview";
    let args = [worker.to_owned(), job.to_owned(), lines.to_owned()];
    if std::env::var_os("RWORKER_CONTROL_SOCKET").is_some() {
        return read(ctx, &args, preview);
    }
    let action = if preview {
        "__cached-preview-local"
    } else {
        action
    };
    let status = crate::state::control_ssh_cmd(ctx, &[action, worker, job, lines])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|_| Fail(1))?;
    crate::ctx::status(status.code().unwrap_or(1))
}

pub(super) fn redact(ctx: &Ctx, args: &[String], index: usize) -> R {
    let pattern = args
        .get(index + 1)
        .filter(|pattern| !pattern.is_empty())
        .ok_or_else(|| {
            ctx.err("--redact requires a nonempty regex");
            Fail(2)
        })?;
    let regex = regex::bytes::Regex::new(pattern).map_err(|_| {
        ctx.err("invalid --redact regex");
        Fail(2)
    })?;
    let exe = std::env::current_exe().map_err(|_| Fail(1))?;
    let mut child = Command::new(exe)
        .arg("logs")
        .args(&args[..index])
        .args(&args[index + 2..])
        .env("__RWORKER_COLOR", if ctx.color { "1" } else { "0" })
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|_| Fail(1))?;
    let mut source = io::BufReader::new(child.stdout.take().ok_or(Fail(1))?);
    let mut stdout = io::stdout().lock();
    let result = (|| -> io::Result<()> {
        let mut line = Vec::new();
        while source.read_until(b'\n', &mut line)? != 0 {
            let ending = line.strip_suffix(b"\n");
            stdout.write_all(&regex.replace_all(ending.unwrap_or(&line), &b"[REDACTED]"[..]))?;
            if ending.is_some() {
                stdout.write_all(b"\n")?;
            }
            stdout.flush()?;
            line.clear();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = child.kill();
    }
    let status = child.wait().map_err(|_| Fail(1))?;
    result.map_err(|_| Fail(1))?;
    crate::ctx::status(status.code().unwrap_or(1))
}

pub fn read(ctx: &Ctx, args: &[String], preview: bool) -> R {
    let [worker, job, lines] = args else {
        return Err(Fail(2));
    };
    if !component(worker) || (!job.is_empty() && !component(job)) {
        return Err(Fail(2));
    }
    let lines = lines.parse::<usize>().map_err(|_| Fail(2))?;
    let dir = root(ctx).join(worker);
    let mut paths = Vec::new();
    if job.is_empty() {
        for entry in fs::read_dir(&dir).map_err(|_| Fail(1))? {
            let entry = entry.map_err(|_| Fail(1))?;
            if entry.file_type().is_ok_and(|t| t.is_file())
                && entry.file_name().to_str().is_some_and(component)
            {
                paths.push(entry.path());
            }
        }
        paths.sort_unstable();
    } else {
        if job == "exec" {
            paths.extend([dir.join("exec.stdout"), dir.join("exec.stderr")]);
        }
        paths.push(dir.join(job));
    }
    let mut stdout = io::stdout().lock();
    let mut found = false;
    let mut remaining = if preview { 256 * 1024 } else { usize::MAX };
    for path in paths {
        let Ok(mut file) = File::open(&path) else {
            continue;
        };
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(|_| Fail(1))?;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(Fail(1))?;
        let name = match name {
            "exec.stdout" => "exec/stdout",
            "exec.stderr" => "exec/stderr",
            _ => name,
        };
        let header = format!("==> {worker}/{name} <==\n");
        if remaining <= header.len() {
            break;
        }
        stdout.write_all(header.as_bytes()).map_err(|_| Fail(1))?;
        remaining -= header.len();
        let data = tail(&bytes, lines);
        let data = &data[data.len().saturating_sub(remaining)..];
        stdout.write_all(data).map_err(|_| Fail(1))?;
        remaining -= data.len();
        found = true;
    }
    if found { Ok(()) } else { Err(Fail(1)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tails_verbatim_with_or_without_final_newline() {
        assert_eq!(tail(b"a\nb\xff\n", 1), b"b\xff\n");
        assert_eq!(tail(b"a\nb\xff", 1), b"b\xff");
        assert_eq!(tail(b"a\nb", 2), b"a\nb");
        assert_eq!(tail(b"a\n", 0), b"");
    }

    #[test]
    fn expires_sources_and_keeps_newest_when_full() -> io::Result<()> {
        let root =
            std::env::temp_dir().join(format!("rworker-log-cache-test-{}", std::process::id()));
        fs::create_dir(&root)?;
        let result = (|| {
            let worker = root.join("worker");
            let old = worker.join("old");
            let fresh = worker.join("fresh");
            let now = SystemTime::now();
            put(&root, &old, b"old", now - Duration::from_secs(7200))?;
            put(&root, &fresh, b"fresh", now)?;
            let age = Duration::from_secs(3600);
            prune(&root, age, 5, None)?;
            assert!(!old.exists());
            let incoming = Entry {
                path: worker.join("incoming"),
                size: 4,
                modified: now - Duration::from_secs(1),
            };
            assert!(!prune(&root, age, 5, Some(&incoming))?);
            assert_eq!(fs::read(fresh)?, b"fresh");
            Ok(())
        })();
        fs::remove_dir_all(root)?;
        result
    }
}
