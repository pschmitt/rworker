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

pub(super) fn read_profiles(dir: &Path) -> Option<Value> {
    let file = File::open(dir.join("profiles.json")).ok()?;
    let rows: Value = serde_json::from_reader(file).ok()?;
    rows.is_array().then_some(rows)
}

fn refresh_profiles(config: &str, dir: &Path) -> io::Result<()> {
    let settings = serde_json::from_reader(BufReader::new(File::open(config)?))?;
    let rows = super::profiles(&settings)?;
    // Do not replace a useful snapshot when a provider lookup failed.
    if rows.iter().any(|row| !profile_has_resources(row)) {
        return Err(io::Error::other("Profile flavor lookup failed"));
    }
    replace(dir, "profiles.json", |file| {
        serde_json::to_writer(file, &rows).map_err(io::Error::other)
    })
}

fn profile_has_resources(row: &Value) -> bool {
    let ram_field = if row["provider"] == "github" {
        "ram_gb"
    } else {
        "ram_mib"
    };
    row["vcpus"].as_u64().is_some() && row[ram_field].as_u64().is_some()
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

#[cfg(feature = "web")]
pub(super) fn read_last(dir: &Path) -> Option<Value> {
    let snapshot = File::open(dir.join("inventory.json")).ok()?;
    let mut snapshot: Value = serde_json::from_reader(BufReader::new(snapshot)).ok()?;
    snapshot["workers"]
        .is_array()
        .then(|| snapshot["workers"].take())
}

/// Read changed snapshots only. The cache file is replaced atomically, so
/// its filesystem identity can be checked without decoding it on every tick.
pub(super) struct Events {
    dir: PathBuf,
    stamp: Option<(u64, SystemTime)>,
    body: Option<String>,
    last_valid: Instant,
}

impl Events {
    pub(super) fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            stamp: None,
            body: None,
            last_valid: Instant::now(),
        }
    }

    pub(super) fn next_snapshot(&mut self) -> io::Result<Option<&str>> {
        use std::os::unix::fs::MetadataExt;
        let observed = fs::metadata(self.dir.join("inventory.json"))
            .ok()
            .and_then(|metadata| Some((metadata.ino(), metadata.modified().ok()?)));
        if observed != self.stamp
            && let Some((workers, _)) = read(&self.dir, false)
        {
            self.stamp = observed;
            self.last_valid = Instant::now();
            let body = workers.to_string();
            if self.body.as_deref() != Some(body.as_str()) {
                return Ok(Some(self.body.insert(body)));
            }
        }
        if self.last_valid.elapsed() >= MAX_AGE {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "inventory stream is stale",
            ));
        }
        Ok(None)
    }
}

/// Invalidate immediately after provider creation so a TUI can show BUILD.
pub(super) fn invalidate_current() {
    if let Some(dir) = directory()
        && let Err(error) = invalidate(&dir)
    {
        eprintln!("Invalidate worker inventory: {error}");
    }
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
    let profile_config = config.to_owned();
    let profile_dir = dir.to_owned();
    thread::spawn(move || {
        loop {
            let age = fs::metadata(profile_dir.join("profiles.json"))
                .and_then(|metadata| metadata.modified())
                .and_then(|modified| modified.elapsed().map_err(io::Error::other));
            if (age.is_err() || age.is_ok_and(|age| age >= Duration::from_secs(86400)))
                && let Err(error) = refresh_profiles(&profile_config, &profile_dir)
            {
                eprintln!("Refresh profiles: {error}");
                thread::sleep(Duration::from_secs(60));
                continue;
            }
            thread::sleep(Duration::from_secs(60));
        }
    });
    let binary = std::env::current_exe()?;
    loop {
        let mutation = generation(dir, "mutation")?;
        let refresh = generation(dir, "refresh")?;
        let output = Command::new("timeout")
            .args(["--kill-after=2s", "25s"])
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
                if let Some(dir) = crate::event_history::directory()
                    && let Err(error) = crate::event_history::observe_workers(&dir, &workers)
                {
                    eprintln!("Record worker lifecycle events: {error}");
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_profile_sizes_can_be_cached() {
        assert!(super::profile_has_resources(&serde_json::json!({
            "provider": "github", "vcpus": 4, "ram_gb": 16
        })));
        assert!(!super::profile_has_resources(&serde_json::json!({
            "provider": "github", "vcpus": null, "ram_gb": null
        })));
    }

    #[test]
    fn profile_snapshot_rejects_invalid_data() -> io::Result<()> {
        let dir = std::env::temp_dir().join(format!("rworker-profiles-{}", std::process::id()));
        fs::create_dir(&dir)?;
        assert!(read_profiles(&dir).is_none());
        fs::write(dir.join("profiles.json"), b"{}")?;
        assert!(read_profiles(&dir).is_none());
        fs::write(
            dir.join("profiles.json"),
            br#"[{"name":"standard","vcpus":4}]"#,
        )?;
        assert_eq!(read_profiles(&dir).unwrap()[0]["vcpus"], 4);
        fs::remove_dir_all(dir)
    }

    #[test]
    fn stream_only_emits_current_changed_inventory() -> io::Result<()> {
        let dir =
            std::env::temp_dir().join(format!("rworker-inventory-events-{}", std::process::id()));
        fs::create_dir(&dir)?;
        let result = (|| {
            let write = |mutation: &str, workers: Value| {
                replace(&dir, "inventory.json", |file| {
                    serde_json::to_writer(
                        file,
                        &json!({
                            "mutation": mutation, "workers": workers,
                        }),
                    )
                    .map_err(io::Error::other)
                })
            };
            write("", json!([]))?;
            let mut events = Events::new(dir.clone());
            assert_eq!(events.next_snapshot()?, Some("[]"));
            write("", json!([]))?;
            assert_eq!(events.next_snapshot()?, None);
            invalidate(&dir)?;
            write("", json!([{"name":"synthetic","status":"BUILD"}]))?;
            assert_eq!(events.next_snapshot()?, None);
            write(
                &generation(&dir, "mutation")?,
                json!([{"name":"synthetic","status":"BUILD"}]),
            )?;
            assert!(
                events
                    .next_snapshot()?
                    .is_some_and(|snapshot| snapshot.contains("BUILD"))
            );
            Ok(())
        })();
        fs::remove_dir_all(dir)?;
        result
    }
}
