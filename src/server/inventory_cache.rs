use std::fs::{self, File};
use std::io::{self, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

const REFRESH_INTERVAL: Duration = Duration::from_secs(5);
const MAX_AGE: Duration = Duration::from_secs(30);
const READ_TIMEOUT: Duration = Duration::from_secs(12);

pub(super) fn directory() -> Option<PathBuf> {
    std::env::var_os("RWORKER_INVENTORY_DIRECTORY").map(PathBuf::from)
}

fn generation(dir: &Path, marker: &str) -> io::Result<String> {
    match fs::read_to_string(dir.join(marker)) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        result => result,
    }
}

fn replace(
    dir: &Path,
    name: &str,
    write: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    let temporary = dir.join(format!(".{name}-{}", std::process::id()));
    let result = (|| {
        write(&mut File::create(&temporary)?)?;
        fs::rename(&temporary, dir.join(name))
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

pub(super) fn invalidate(dir: &Path) -> io::Result<()> {
    mark(dir, "mutation")
}

fn mark(dir: &Path, marker: &str) -> io::Result<()> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?;
    replace(dir, marker, |file| {
        write!(file, "{}:{}", now.as_nanos(), std::process::id())
    })
}

pub(super) fn read(dir: &Path, fresh: bool) -> Option<(Value, Duration)> {
    let snapshot = File::open(dir.join("inventory.json")).ok()?;
    let age = snapshot.metadata().ok()?.modified().ok()?.elapsed().ok()?;
    if age >= MAX_AGE {
        return None;
    }
    let mut snapshot: Value = serde_json::from_reader(BufReader::new(snapshot)).ok()?;
    (snapshot["mutation"].as_str()? == generation(dir, "mutation").ok()?
        && (!fresh || snapshot["refresh"].as_str()? == generation(dir, "refresh").ok()?))
    .then(|| (snapshot["workers"].take(), age))
}

pub(super) fn list(dir: &Path, fresh: bool) -> Result<Value, String> {
    if fresh && read(dir, true).is_some() {
        mark(dir, "refresh").map_err(|error| format!("Request inventory refresh: {error}"))?;
    }
    let deadline = Instant::now() + READ_TIMEOUT;
    loop {
        if let Some((workers, _)) = read(dir, fresh) {
            return Ok(workers);
        }
        if Instant::now() >= deadline {
            return Err("Control host has no current inventory snapshot".into());
        }
        thread::sleep(Duration::from_millis(50));
    }
}

pub(super) fn refresh_forever(config: &str, dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let binary = std::env::current_exe()?;
    loop {
        let mutation = generation(dir, "mutation")?;
        let refresh = generation(dir, "refresh")?;
        let output = Command::new("timeout")
            .args(["--kill-after=2s", "10s"])
            .arg(&binary)
            .args(["server", config, "ls"])
            .env_remove("RWORKER_INVENTORY_DIRECTORY")
            .stdin(Stdio::null())
            .stderr(Stdio::inherit())
            .output()?;
        match output
            .status
            .success()
            .then(|| serde_json::from_slice::<Value>(&output.stdout))
        {
            Some(Ok(workers)) => {
                let snapshot =
                    json!({"mutation": mutation, "refresh": refresh, "workers": workers});
                replace(dir, "inventory.json", |file| {
                    serde_json::to_writer(file, &snapshot).map_err(io::Error::other)
                })?;
            }
            Some(Err(error)) => eprintln!("Decode worker inventory: {error}"),
            None => eprintln!("Refresh worker inventory: {}", output.status),
        }
        let deadline = Instant::now() + REFRESH_INTERVAL;
        while Instant::now() < deadline
            && generation(dir, "mutation")? == mutation
            && generation(dir, "refresh")? == refresh
        {
            thread::sleep(Duration::from_millis(250));
        }
    }
}
