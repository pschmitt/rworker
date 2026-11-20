//! The controller's bounded record of observed worker and job transitions.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, File};
use std::io::{self, BufReader};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MAX_EVENTS: usize = 8192;

pub(crate) type Jobs = BTreeMap<String, BTreeMap<String, String>>;

#[derive(Clone, Default, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct Worker {
    pub agent: String,
    pub agent_id: String,
    pub topic: String,
    pub owner: String,
    status: String,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Event {
    pub id: u64,
    pub timestamp: i64,
    pub kind: String,
    pub target: String,
    pub details: String,
    #[serde(flatten)]
    pub worker: Worker,
}

#[derive(Default, Deserialize, Serialize)]
struct History {
    next_id: u64,
    events: VecDeque<Event>,
    workers: BTreeMap<String, Worker>,
    jobs: Jobs,
}

pub(crate) fn directory() -> Option<PathBuf> {
    std::env::var_os("RWORKER_EVENT_DIRECTORY").map(PathBuf::from)
}

fn read(dir: &Path) -> io::Result<History> {
    match File::open(dir.join("events.json")) {
        Ok(file) => serde_json::from_reader(BufReader::new(file)).map_err(io::Error::other),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(History::default()),
        Err(error) => Err(error),
    }
}

fn update(dir: &Path, change: impl FnOnce(&mut History, i64) -> bool) -> io::Result<()> {
    let guard = File::open(dir)?;
    // SAFETY: the live directory descriptor is held until the update ends.
    if unsafe { libc::flock(guard.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut history = read(dir)?;
    let now = jiff::Timestamp::now().as_second();
    let changed = change(&mut history, now);
    let days = std::env::var("RWORKER_EVENT_RETENTION_DAYS")
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(7);
    let cutoff = now.saturating_sub(days.saturating_mul(86400));
    let len = history.events.len();
    while history.events.len() > MAX_EVENTS
        || history
            .events
            .front()
            .is_some_and(|event| event.timestamp < cutoff)
    {
        history.events.pop_front();
    }
    if !changed && len == history.events.len() {
        return Ok(());
    }
    let temporary = dir.join(format!(".events-{}", std::process::id()));
    let result = (|| {
        let mut file = File::create(&temporary)?;
        serde_json::to_writer(&mut file, &history)?;
        file.set_permissions(fs::Permissions::from_mode(0o660))?;
        file.sync_all()?;
        fs::rename(&temporary, dir.join("events.json"))?;
        guard.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

impl History {
    fn push(&mut self, timestamp: i64, kind: &str, target: &str, details: String, worker: &Worker) {
        self.next_id = self.next_id.saturating_add(1);
        self.events.push_back(Event {
            id: self.next_id,
            timestamp,
            kind: kind.into(),
            target: target.into(),
            details,
            worker: worker.clone(),
        });
    }

    fn workers(&mut self, now: i64, current: BTreeMap<String, Worker>) -> bool {
        if self.workers == current {
            return false;
        }
        let old = std::mem::replace(&mut self.workers, current);
        let changes: Vec<_> = self
            .workers
            .iter()
            .filter_map(|(name, worker)| match old.get(name) {
                None => Some((
                    name.clone(),
                    worker.clone(),
                    "worker-added",
                    format!("status={}", worker.status),
                )),
                Some(before) if before.status != worker.status => Some((
                    name.clone(),
                    worker.clone(),
                    "worker-state",
                    format!("{} -> {}", before.status, worker.status),
                )),
                _ => None,
            })
            .collect();
        for (name, worker, kind, details) in changes {
            self.push(now, kind, &name, details, &worker);
        }
        for (name, worker) in old {
            if !self.workers.contains_key(&name) {
                self.jobs.remove(&name);
                self.push(now, "worker-deleted", &name, String::new(), &worker);
            }
        }
        true
    }

    fn jobs(&mut self, now: i64, polled: &Jobs) -> bool {
        let mut changed = false;
        for (name, current) in polled {
            let Some(worker) = self.workers.get(name).cloned() else {
                continue;
            };
            if self.jobs.get(name) == Some(current)
                || (current.is_empty() && !self.jobs.contains_key(name))
            {
                continue;
            }
            let old = self
                .jobs
                .insert(name.clone(), current.clone())
                .unwrap_or_default();
            changed = true;
            for (job, state) in current {
                match old.get(job) {
                    None => self.push(
                        now,
                        "job-added",
                        &format!("{name}/{job}"),
                        format!("state={state}"),
                        &worker,
                    ),
                    Some(before) if before != state => self.push(
                        now,
                        "job-state",
                        &format!("{name}/{job}"),
                        format!("{before} -> {state}"),
                        &worker,
                    ),
                    _ => {}
                }
            }
            for job in old.keys().filter(|job| !current.contains_key(*job)) {
                self.push(
                    now,
                    "job-deleted",
                    &format!("{name}/{job}"),
                    String::new(),
                    &worker,
                );
            }
        }
        changed
    }
}

pub(crate) fn observe_workers(dir: &Path, inventory: &Value) -> io::Result<()> {
    let rows = inventory
        .as_array()
        .ok_or_else(|| io::Error::other("invalid event inventory"))?;
    let workers = rows
        .iter()
        .filter_map(|row| {
            let name = row["name"].as_str()?.to_owned();
            let field = |key: &str| row[key].as_str().unwrap_or_default().to_owned();
            Some((
                name,
                Worker {
                    agent: field("agent"),
                    agent_id: field("agent_id"),
                    topic: field("topic"),
                    owner: field("owner"),
                    status: field("status"),
                },
            ))
        })
        .collect();
    update(dir, |history, now| history.workers(now, workers))
}

pub(crate) fn observe_jobs(dir: &Path, jobs: &Jobs) -> io::Result<()> {
    update(dir, |history, now| history.jobs(now, jobs))
}

pub(crate) fn list(dir: &Path) -> io::Result<VecDeque<Event>> {
    Ok(read(dir)?.events)
}

pub(crate) fn latest_id(dir: &Path) -> io::Result<u64> {
    Ok(read(dir)?.next_id)
}

pub(crate) fn watch(
    dir: &Path,
    mut cursor: u64,
    mut send: impl FnMut(Option<&Event>) -> io::Result<()>,
) -> io::Result<()> {
    let mut previous = None;
    let mut heartbeat = Instant::now();
    loop {
        let stamp = match fs::metadata(dir.join("events.json")) {
            Ok(metadata) => Some((metadata.ino(), metadata.modified()?)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        if stamp != previous {
            previous = stamp;
            for event in list(dir)? {
                if event.id > cursor {
                    send(Some(&event))?;
                    cursor = event.id;
                    heartbeat = Instant::now();
                }
            }
        }
        if heartbeat.elapsed() >= Duration::from_secs(5) {
            send(None)?;
            heartbeat = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_metadata_and_ignores_unobserved_jobs() {
        let mut history = History::default();
        let worker = Worker {
            agent: "fixture".into(),
            status: "BUILD".into(),
            ..Worker::default()
        };
        assert!(history.workers(1, BTreeMap::from([("worker".into(), worker.clone())])));
        let active = Worker {
            status: "ACTIVE".into(),
            ..worker
        };
        assert!(history.workers(2, BTreeMap::from([("worker".into(), active)])));
        let jobs = |state: &str| {
            BTreeMap::from([(
                "worker".into(),
                BTreeMap::from([("exec".into(), state.into())]),
            )])
        };
        assert!(history.jobs(3, &jobs("running")));
        assert!(!history.jobs(4, &Jobs::new()));
        assert!(history.jobs(5, &jobs("exit 0")));
        assert!(history.jobs(6, &BTreeMap::from([("worker".into(), BTreeMap::new())])));
        assert!(history.workers(7, BTreeMap::new()));
        let kinds: Vec<_> = history
            .events
            .iter()
            .map(|event| event.kind.as_str())
            .collect();
        assert_eq!(
            kinds,
            [
                "worker-added",
                "worker-state",
                "job-added",
                "job-state",
                "job-deleted",
                "worker-deleted"
            ]
        );
        assert_eq!(history.events.back().unwrap().worker.agent, "fixture");
        assert_eq!(history.events[3].details, "running -> exit 0");
        assert_eq!(history.events.back().unwrap().id, 6);
    }
}
