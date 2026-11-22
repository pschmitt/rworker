//! Durable controller records for GitHub-hosted worker leases.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::github::GitHub;

const MAX_LEASE_SECONDS: u64 = 7200;

#[derive(Serialize, Deserialize)]
struct Lease {
    id: String,
    repository: String,
    profile: String,
    expires_at: u64,
    run_id: Option<u64>,
    state: LeaseState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    token_hash: Option<String>,
    #[serde(default)]
    public_key: String,
    #[serde(default)]
    endpoint: Option<Endpoint>,
    #[serde(default)]
    kvm: bool,
    #[serde(default)]
    created_at: u64,
    #[serde(default)]
    owner: String,
    #[serde(default)]
    topic: String,
}

#[derive(Deserialize, Serialize)]
pub(super) struct Endpoint {
    port: u16,
    host_key: String,
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LeaseState {
    Dispatching,
    DispatchUnknown,
    Queued,
    Claimed,
    Ready,
    Cancelling,
    Ended,
}

pub(super) fn run(settings: &Value, action: &str, args: &[String]) -> Result<Value, String> {
    let config = &settings["githubActions"];
    let setting = |key: &str| {
        config[key]
            .as_str()
            .ok_or_else(|| format!("Missing githubActions.{key}"))
    };
    let repository = setting("repository")?;
    let token = fs::read_to_string(setting("tokenFile")?)
        .map_err(|error| format!("Read GitHub Actions credential: {error}"))?;
    let github = GitHub::new(repository, token.trim())?;
    let root = Path::new(setting("stateDirectory")?);
    if !root.is_absolute() {
        return Err("GitHub Actions state directory must be absolute".into());
    }

    match (action, args) {
        ("github-up", [profile, ttl, public_key, owner, topic]) => {
            if !matches!(profile.as_str(), "linux-arm64" | "linux-x64") {
                return Err("Unknown GitHub Actions runner profile".into());
            }
            let ttl = super::duration_seconds(ttl)
                .filter(|seconds| (1..=MAX_LEASE_SECONDS).contains(seconds))
                .ok_or("GitHub Actions lease must be between 1 second and 2 hours")?;
            if !valid_key(public_key) {
                return Err("Expected an Ed25519 or RSA SSH public key".into());
            }
            create(
                root,
                &github,
                Lease {
                    id: random_hex::<16>()?,
                    repository: repository.into(),
                    profile: profile.into(),
                    created_at: now(),
                    expires_at: now() + ttl,
                    run_id: None,
                    state: LeaseState::Dispatching,
                    token_hash: None,
                    public_key: public_key.into(),
                    endpoint: None,
                    kvm: false,
                    owner: owner.into(),
                    topic: topic.into(),
                },
            )
        }
        ("github-status", [id]) | ("github-down", [id, _]) => {
            let dir = lease_dir(root, id)?;
            let guard = File::open(dir.join("lock")).map_err(|error| error.to_string())?;
            lock(&guard)?;
            let mut lease = read_lease(&dir).map_err(|error| error.to_string())?;
            if lease.repository != repository {
                return Err("GitHub Actions lease belongs to a different repository".into());
            }
            if action == "github-down" && args[1] != lease.owner {
                return Err("GitHub Actions lease belongs to a different operator".into());
            }
            reconcile(&dir, &mut lease, &github, action == "github-down")?;
            Ok(public_lease(lease))
        }
        ("github-reap", []) => {
            for dir in directories(root)? {
                let result = (|| {
                    let guard = File::open(dir.join("lock")).map_err(|error| error.to_string())?;
                    lock(&guard)?;
                    let mut lease = read_lease(&dir).map_err(|error| error.to_string())?;
                    if lease.repository == repository && lease.run_id.is_some() {
                        reconcile(&dir, &mut lease, &github, false)?;
                    }
                    Ok::<_, String>(())
                })();
                if let Err(error) = result {
                    eprintln!("Reconcile GitHub lease: {error}");
                }
            }
            Ok(Value::Null)
        }
        _ => Err("Invalid GitHub Actions lease arguments".into()),
    }
}

pub(super) fn claim(
    settings: &Value,
    id: &str,
    oidc_token: &str,
    profile: &str,
    architecture: &str,
    kvm: bool,
) -> Result<Value, String> {
    let config = &settings["githubActions"];
    let repository = config["repository"]
        .as_str()
        .ok_or("Missing GitHub repository")?;
    let repository_id = config["repositoryId"]
        .as_str()
        .ok_or("Missing GitHub repository ID")?;
    let root = config["stateDirectory"]
        .as_str()
        .ok_or("Missing GitHub state directory")?;
    let root = Path::new(root);
    if !root.is_absolute() {
        return Err("GitHub Actions state directory must be absolute".into());
    }
    let run_id = super::github_oidc::verify(oidc_token, repository, repository_id)?;
    let dir = lease_dir(root, id)?;
    let guard = File::open(dir.join("lock")).map_err(|error| error.to_string())?;
    lock(&guard)?;
    let mut lease = read_lease(&dir).map_err(|error| error.to_string())?;
    if lease.run_id != Some(run_id)
        || lease.repository != repository
        || lease.profile != profile
        || !matches!(
            (profile, architecture),
            ("linux-arm64", "aarch64") | ("linux-x64", "x86_64")
        )
        || lease.state != LeaseState::Queued
        || now() >= lease.expires_at
    {
        return Err("GitHub workflow cannot claim the lease".into());
    }
    let token = random_hex::<32>()?;
    let hash = ring::digest::digest(&ring::digest::SHA256, token.as_bytes());
    lease.token_hash = Some(STANDARD.encode(hash.as_ref()));
    lease.state = LeaseState::Claimed;
    lease.kvm = kvm;
    write_lease(&dir, &lease).map_err(|error| error.to_string())?;
    Ok(json!({"token": token, "expires_at": lease.expires_at, "public_key": lease.public_key}))
}

pub(super) fn inventory(settings: &Value) -> Result<Vec<Value>, String> {
    let config = &settings["githubActions"];
    let Some(root) = config["stateDirectory"].as_str().map(Path::new) else {
        return Ok(Vec::new());
    };
    if !root.is_absolute() {
        return Err("GitHub Actions state directory must be absolute".into());
    }
    Ok(directories(root)?
        .into_iter()
        .filter_map(|dir| read_lease(&dir).ok())
        .filter(|lease| {
            Some(lease.repository.as_str()) == config["repository"].as_str()
                && lease.expires_at > now()
                && matches!(lease.state, LeaseState::Queued | LeaseState::Claimed | LeaseState::Ready | LeaseState::Cancelling)
        })
        .map(|lease| {
            let name = format!("gha-{}", lease.id);
            let ready = lease.state == LeaseState::Ready;
            json!({
                "name": name,
                "ip": ready.then_some(&name),
                "provider": "github",
                "kind": "adhoc",
                "profile": format!("github-{}", lease.profile.trim_start_matches("linux-")),
                "status": match lease.state {
                    LeaseState::Ready => "ACTIVE",
                    LeaseState::Cancelling => "DELETING",
                    _ => "BUILD",
                },
                "user": "rworker-build",
                "owner": lease.owner,
                "topic": lease.topic,
                "host_key": lease.endpoint.as_ref().map(|endpoint| &endpoint.host_key),
                "github_port": lease.endpoint.as_ref().map(|endpoint| endpoint.port),
                "expires_at": lease.expires_at,
                "created_at": jiff::Timestamp::from_second(lease.created_at as i64).ok().map(|time| time.to_string()),
                "kvm": lease.kvm,
            })
        })
        .collect())
}

fn directories(root: &Path) -> Result<Vec<PathBuf>, String> {
    match fs::read_dir(root) {
        Ok(entries) => Ok(entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .and_then(|id| lease_dir(root, id).ok())
            })
            .collect()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.to_string()),
    }
}

fn valid_key(key: &str) -> bool {
    if key.len() > 4096 || key.contains(['\r', '\n']) {
        return false;
    }
    let mut fields = key.split_ascii_whitespace();
    matches!(fields.next(), Some("ssh-ed25519" | "ssh-rsa"))
        && fields
            .next()
            .is_some_and(|encoded| STANDARD.decode(encoded).is_ok_and(|key| !key.is_empty()))
}

pub(super) fn agent_update(
    settings: &Value,
    id: &str,
    token: &str,
    endpoint: Option<Endpoint>,
) -> Result<(), String> {
    if token.len() != 64 {
        return Err("Invalid lease credential".into());
    }
    let root = settings["githubActions"]["stateDirectory"]
        .as_str()
        .map(Path::new)
        .filter(|path| path.is_absolute())
        .ok_or("Missing GitHub state directory")?;
    let dir = lease_dir(root, id)?;
    let guard = File::open(dir.join("lock")).map_err(|error| error.to_string())?;
    lock(&guard)?;
    let mut lease = read_lease(&dir).map_err(|error| error.to_string())?;
    let hash = ring::digest::digest(&ring::digest::SHA256, token.as_bytes());
    let expected = lease
        .token_hash
        .as_deref()
        .ok_or("Invalid lease credential")?;
    let expected = STANDARD
        .decode(expected)
        .map_err(|_| "Invalid lease credential")?;
    // Verify the hash without comparing secret-derived strings byte by byte.
    #[allow(deprecated)]
    ring::constant_time::verify_slices_are_equal(hash.as_ref(), &expected)
        .map_err(|_| "Invalid lease credential")?;
    if now() >= lease.expires_at || !matches!(lease.state, LeaseState::Claimed | LeaseState::Ready)
    {
        return Err("Lease has ended".into());
    }
    if let Some(endpoint) = endpoint {
        if endpoint.port < 1024
            || !valid_key(&endpoint.host_key)
            || !endpoint.host_key.starts_with("ssh-ed25519 ")
        {
            return Err("Invalid SSH endpoint".into());
        }
        lease.endpoint = Some(endpoint);
        lease.state = LeaseState::Ready;
        write_lease(&dir, &lease).map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn public_lease(mut lease: Lease) -> Value {
    lease.token_hash = None;
    json!(lease)
}

fn random_hex<const N: usize>() -> Result<String, String> {
    let mut random = [0u8; N];
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut random))
        .map_err(|error| error.to_string())?;
    Ok(random.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn lock(file: &File) -> Result<(), String> {
    // SAFETY: flock borrows a live descriptor; closing it releases the lock.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error().to_string())
    }
}

fn lease_dir(root: &Path, id: &str) -> Result<PathBuf, String> {
    if id.len() != 32
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("Invalid GitHub Actions lease ID".into());
    }
    Ok(root.join(id))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn create(root: &Path, github: &GitHub<'_>, mut lease: Lease) -> Result<Value, String> {
    let dir = lease_dir(root, &lease.id)?;
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(root)
        .map_err(|error| error.to_string())?;
    DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .map_err(|error| error.to_string())?;
    let guard = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dir.join("lock"))
        .map_err(|error| error.to_string())?;
    lock(&guard)?;
    write_lease(&dir, &lease).map_err(|error| error.to_string())?;

    match github.dispatch(&lease.id, &lease.profile) {
        Ok(run_id) => {
            lease.run_id = Some(run_id);
            lease.state = LeaseState::Queued;
            if let Err(error) = write_lease(&dir, &lease) {
                return Err(format!(
                    "Save GitHub Actions run {run_id} for lease {}: {error}",
                    lease.id
                ));
            }
        }
        Err(error) => {
            // A lost HTTP response can hide a successful dispatch. Do not
            // automatically submit another run for this lease.
            lease.state = LeaseState::DispatchUnknown;
            write_lease(&dir, &lease).map_err(|error| error.to_string())?;
            return Err(format!("Dispatch lease {}: {error}", lease.id));
        }
    }
    Ok(public_lease(lease))
}

fn reconcile(
    dir: &Path,
    lease: &mut Lease,
    github: &GitHub<'_>,
    cancel: bool,
) -> Result<(), String> {
    if lease.state == LeaseState::Ended {
        return Ok(());
    }
    let run_id = lease.run_id.ok_or("GitHub workflow run ID is unknown")?;
    if github.completed(run_id)? {
        lease.state = LeaseState::Ended;
    } else if cancel || now() >= lease.expires_at {
        lease.state = LeaseState::Cancelling;
        write_lease(dir, lease).map_err(|error| error.to_string())?;
        github.cancel(run_id)?;
        return Ok(());
    } else {
        return Ok(());
    }
    write_lease(dir, lease).map_err(|error| error.to_string())
}

fn read_lease(dir: &Path) -> io::Result<Lease> {
    serde_json::from_reader(File::open(dir.join("lease.json"))?).map_err(io::Error::other)
}

fn write_lease(dir: &Path, lease: &Lease) -> io::Result<()> {
    let pending = dir.join("lease.pending");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&pending)?;
    serde_json::to_writer(&mut file, lease)?;
    file.sync_all()?;
    fs::rename(pending, dir.join("lease.json"))?;
    File::open(dir)?.sync_all()
}
