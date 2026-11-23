//! Durable controller records for CI worker leases.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::github::{GitHub, RunStatus};
use super::gitlab::GitLab;
use super::netbird::NetBird;
use super::openstack::setting;
use super::tailscale::Tailscale;
use crate::github_profiles::{Forge, WORKER_SUFFIX_BYTES, valid_worker_suffix};

const MAX_LEASE_SECONDS: u64 = 7200;
const MESH_KEY_TTL: u64 = 600;

#[derive(Serialize, Deserialize)]
struct Lease {
    id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    worker_suffix: Option<String>,
    repository: String,
    profile: String,
    expires_at: u64,
    run_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    job_id: Option<u64>,
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
    #[serde(default)]
    archive_logs: bool,
    #[serde(default)]
    workflow: Option<RunStatus>,
    #[serde(default)]
    disk: Option<Disk>,
}

#[derive(Deserialize, Serialize, Clone, Copy)]
pub(super) struct Disk {
    total_bytes: u64,
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

enum Backend<'a> {
    GitHub(GitHub<'a>),
    GitLab(GitLab<'a>),
}

impl Backend<'_> {
    fn dispatch(&self, id: &str, profile: &str) -> Result<u64, String> {
        match self {
            Self::GitHub(api) => api.dispatch(id, profile),
            Self::GitLab(api) => api.dispatch(id, profile),
        }
    }

    fn status(&self, id: u64) -> Result<RunStatus, String> {
        match self {
            Self::GitHub(api) => api.status(id),
            Self::GitLab(api) => api.status(id),
        }
    }

    fn job_id(&self, id: u64) -> Option<u64> {
        match self {
            Self::GitHub(api) => api.job_id(id),
            Self::GitLab(api) => api.job_id(id),
        }
        .filter(|id| *id != 0)
    }

    fn cancel(&self, id: u64) -> Result<(), String> {
        match self {
            Self::GitHub(api) => api.cancel(id),
            Self::GitLab(api) => api.cancel(id),
        }
    }
}

fn repository(config: &Value, forge: Forge) -> Option<String> {
    match forge {
        Forge::GitHub => config["repository"].as_str().map(str::to_owned),
        Forge::GitLab => Some(format!(
            "{}:{}",
            config["instance"].as_str()?,
            config["projectId"].as_u64()?
        )),
    }
}

pub(super) fn action(action: &str) -> Option<(Forge, &str)> {
    let (provider, command) = action.split_once('-')?;
    let forge = Forge::ALL
        .into_iter()
        .find(|forge| forge.key() == provider)?;
    matches!(command, "up" | "status" | "down" | "reap").then_some((forge, command))
}

pub(super) fn run(
    settings: &Value,
    forge: Forge,
    command: &str,
    args: &[String],
) -> Result<Value, String> {
    let config = &settings[forge.settings_key()];
    let setting = |key: &str| {
        config[key]
            .as_str()
            .ok_or_else(|| format!("Missing {}.{key}", forge.settings_key()))
    };
    let repository = repository(config, forge).ok_or("Missing worker pipeline project")?;
    let token = fs::read_to_string(setting("tokenFile")?)
        .map_err(|error| format!("Read worker pipeline credential: {error}"))?;
    let backend = match forge {
        Forge::GitHub => Backend::GitHub(GitHub::new(&repository, token.trim())?),
        Forge::GitLab => Backend::GitLab(GitLab::new(
            setting("instance")?,
            config["projectId"]
                .as_u64()
                .ok_or("Missing GitLab project ID")?,
            token.trim(),
        )?),
    };
    let root = Path::new(setting("stateDirectory")?);
    if !root.is_absolute() {
        return Err("Worker lease state directory must be absolute".into());
    }

    match (command, args) {
        ("up", [profile, ttl, public_key, owner, topic]) => {
            if !forge.profiles().iter().any(|entry| entry.runner == profile) {
                return Err("Unknown worker runner profile".into());
            }
            let ttl = super::duration_seconds(ttl)
                .filter(|seconds| (1..=MAX_LEASE_SECONDS).contains(seconds))
                .ok_or("Worker lease must be between 1 second and 2 hours")?;
            if !super::worker_ssh::valid_public_key(public_key) {
                return Err("Expected an Ed25519 or RSA SSH public key".into());
            }
            // The runner writes this string as authorized_keys. A second
            // line works with existing runner agents and stored leases.
            let keys = match super::worker_ssh::web_worker_key(owner, &[public_key])? {
                Some(key) => format!("{public_key}\n{key}"),
                None => public_key.clone(),
            };
            let response = create(
                root,
                &backend,
                Lease {
                    id: random_hex::<16>()?,
                    worker_suffix: None,
                    repository: repository.clone(),
                    profile: profile.into(),
                    created_at: now(),
                    expires_at: now() + ttl,
                    run_id: None,
                    job_id: None,
                    state: LeaseState::Dispatching,
                    token_hash: None,
                    public_key: keys,
                    endpoint: None,
                    kvm: false,
                    owner: owner.into(),
                    topic: topic.into(),
                    archive_logs: settings["logCollectorOperator"].is_string(),
                    workflow: None,
                    disk: None,
                },
            )?;
            Ok(with_provider_links(response, forge, config))
        }
        ("status", [id]) | ("down", [id, _]) => {
            let dir = worker_lease_dir(root, id)?;
            let guard = File::open(dir.join("lock")).map_err(|error| error.to_string())?;
            lock(&guard)?;
            let mut lease = read_lease(&dir).map_err(|error| error.to_string())?;
            if lease.repository != repository {
                return Err("Worker lease belongs to a different repository".into());
            }
            if command == "down" && args[1] != lease.owner {
                return Err("Worker lease belongs to a different operator".into());
            }
            reconcile(&dir, &mut lease, &backend, command == "down")?;
            Ok(with_provider_links(public_lease(lease), forge, config))
        }
        ("reap", []) => {
            for dir in directories(root)? {
                let result = (|| {
                    let guard = File::open(dir.join("lock")).map_err(|error| error.to_string())?;
                    lock(&guard)?;
                    let mut lease = read_lease(&dir).map_err(|error| error.to_string())?;
                    if lease.repository == repository
                        && let Some(run_id) = lease.run_id
                        && lease.state != LeaseState::Ended
                    {
                        let started =
                            matches!(lease.state, LeaseState::Claimed | LeaseState::Ready);
                        reconcile(&dir, &mut lease, &backend, false)?;
                        let lookup = lease.job_id.is_none()
                            && (started
                                || lease.workflow.as_ref().is_some_and(|workflow| {
                                    matches!(workflow.status.as_str(), "in_progress" | "completed")
                                }));
                        // The provider's jobs API must not hold the lock needed
                        // by readiness probes, status requests, or heartbeats.
                        drop(guard);
                        if lookup && let Some(job_id) = backend.job_id(run_id) {
                            let guard =
                                File::open(dir.join("lock")).map_err(|error| error.to_string())?;
                            lock(&guard)?;
                            let mut current =
                                read_lease(&dir).map_err(|error| error.to_string())?;
                            if current.run_id == Some(run_id) && current.job_id.is_none() {
                                current.job_id = Some(job_id);
                                write_lease(&dir, &current).map_err(|error| error.to_string())?;
                            }
                        }
                    }
                    Ok::<_, String>(())
                })();
                if let Err(error) = result {
                    eprintln!("Reconcile worker lease: {error}");
                }
            }
            Ok(Value::Null)
        }
        _ => Err("Invalid worker lease arguments".into()),
    }
}

pub(super) fn claim(
    settings: &Value,
    id: &str,
    oidc_token: &str,
    profile: &str,
    architecture: &str,
    capabilities: &[String],
    disk: Option<Disk>,
) -> Result<Value, String> {
    let (forge, id) = agent_lease(id);
    let config = &settings[forge.settings_key()];
    let repository = repository(config, forge).ok_or("Missing worker pipeline project")?;
    let root = state_directory(config).ok_or("Missing worker lease state directory")?;
    let (run_id, job_id) = match forge {
        Forge::GitHub => (
            super::github_oidc::verify(
                oidc_token,
                &repository,
                config["repositoryId"]
                    .as_str()
                    .ok_or("Missing GitHub repository ID")?,
            )?,
            None,
        ),
        Forge::GitLab => super::github_oidc::verify_gitlab(oidc_token, config)?,
    };
    let dir = lease_dir(root, id)?;
    let guard = File::open(dir.join("lock")).map_err(|error| error.to_string())?;
    lock(&guard)?;
    let mut lease = read_lease(&dir).map_err(|error| error.to_string())?;
    if lease.run_id != Some(run_id)
        || lease.repository != repository
        || lease.profile != profile
        || !matches!(
            (profile, architecture),
            ("linux-arm64", "aarch64")
                | (
                    "linux-x64" | "gl-linux-x64" | "gl-linux-x64-lease",
                    "x86_64"
                )
        )
        || lease.state != LeaseState::Queued
        || now() >= lease.expires_at
    {
        return Err("Pipeline job cannot claim the lease".into());
    }
    if !config["meshProfiles"][profile].is_null()
        && !capabilities
            .iter()
            .any(|capability| capability == "mesh-v1")
    {
        return Err("Pipeline agent does not support mesh enrollment".into());
    }
    let token = random_hex::<32>()?;
    let hash = ring::digest::digest(&ring::digest::SHA256, token.as_bytes());
    lease.token_hash = Some(STANDARD.encode(hash.as_ref()));
    lease.state = LeaseState::Claimed;
    lease.job_id = job_id;
    lease.kvm = capabilities.iter().any(|capability| capability == "kvm");
    lease.disk = disk.filter(|disk| disk.total_bytes != 0);
    write_lease(&dir, &lease).map_err(|error| error.to_string())?;
    let mut assignment = json!({
        "token": token, "expires_at": lease.expires_at, "public_key": lease.public_key,
    });
    if let Some(mesh) = mesh_assignment(settings, config, forge, &lease)? {
        assignment["mesh"] = mesh;
    }
    Ok(assignment)
}

fn mesh_assignment(
    settings: &Value,
    config: &Value,
    forge: Forge,
    lease: &Lease,
) -> Result<Option<Value>, String> {
    let mesh = &config["meshProfiles"][&lease.profile];
    if mesh.is_null() {
        return Ok(None);
    }
    let ttl = MESH_KEY_TTL.min(lease.expires_at.saturating_sub(now()));
    if ttl == 0 {
        return Err("Lease has ended".into());
    }
    let name = format!(
        "{}{}",
        forge.prefix(),
        lease.worker_suffix.as_deref().unwrap_or(&lease.id)
    );
    let mut assignment = json!({"hostname": name});
    let netbird = if mesh["netbird"].is_null() {
        None
    } else {
        let config = &mesh["netbird"];
        let management_url = setting(config, "managementUrl")?;
        if !management_url.starts_with("https://") {
            return Err("NetBird management URL must use HTTPS".into());
        }
        let client = NetBird::from_settings(settings)?;
        let group = client.group(setting(config, "group")?)?;
        let key = client.create_ephemeral_key(&name, setting(&group, "id")?, ttl)?;
        assignment["netbird"] = json!({
            "management_url": management_url,
            "key": setting(&key, "key")?,
        });
        Some((client, setting(&key, "id")?.to_owned()))
    };
    if mesh["tailscale"].as_bool() == Some(true) {
        let tailscale = (|| {
            let mut client = Tailscale::from_settings(&settings["tailscale"])?;
            let key = client.create_ephemeral_key(&name, ttl)?;
            Ok::<_, String>(json!({"key": setting(&key, "key")?}))
        })();
        match tailscale {
            Ok(key) => assignment["tailscale"] = key,
            Err(error) => {
                if let Some((client, id)) = netbird {
                    client.delete_key(&id);
                }
                return Err(error);
            }
        }
    }
    Ok(Some(assignment))
}

pub(super) fn inventory(settings: &Value) -> Result<Vec<Value>, String> {
    let mut rows = Vec::new();
    for forge in Forge::ALL {
        rows.extend(provider_inventory(settings, forge)?);
    }
    Ok(rows)
}

fn provider_inventory(settings: &Value, forge: Forge) -> Result<Vec<Value>, String> {
    let config = &settings[forge.settings_key()];
    let Some(root) = state_directory(config) else {
        return Ok(Vec::new());
    };
    let repository = repository(config, forge);
    Ok(directories(root)?
        .into_iter()
        .filter_map(|dir| read_lease(&dir).ok())
        .filter(|lease| {
            Some(lease.repository.as_str()) == repository.as_deref()
                && lease.expires_at > now()
                && matches!(lease.state, LeaseState::Queued | LeaseState::Claimed | LeaseState::Ready | LeaseState::Cancelling)
        })
        .map(|lease| {
            let name = format!("{}{}", forge.prefix(), lease.worker_suffix.as_deref().unwrap_or(&lease.id));
            let ready = lease.state == LeaseState::Ready;
            with_provider_links(json!({
                "name": name,
                "ip": ready.then_some(&name),
                "provider": forge.key(),
                "kind": "adhoc",
                "profile": forge.profiles().iter().find(|profile| profile.runner == lease.profile).map(|profile| profile.name),
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
                "run_id": lease.run_id,
                "job_id": lease.job_id,
            }), forge, config)
        })
        .collect())
}

pub(super) fn add_disk_observations(settings: &Value, rows: &mut [Value]) {
    for forge in Forge::ALL {
        add_provider_disk_observations(settings, forge, rows);
    }
}

fn add_provider_disk_observations(settings: &Value, forge: Forge, rows: &mut [Value]) {
    let config = &settings[forge.settings_key()];
    let repository = repository(config, forge);
    let Some(root) = state_directory(config) else {
        return;
    };
    let Ok(dirs) = directories(root) else {
        return;
    };
    let mut latest = std::collections::BTreeMap::new();
    for lease in dirs.into_iter().filter_map(|dir| read_lease(&dir).ok()) {
        if Some(lease.repository.as_str()) != repository.as_deref() {
            continue;
        }
        if let Some(disk) = lease.disk {
            let entry = latest
                .entry(lease.profile)
                .or_insert((lease.created_at, disk));
            if lease.created_at > entry.0 {
                *entry = (lease.created_at, disk);
            }
        }
    }
    for row in rows {
        let Some(profile) = row["name"]
            .as_str()
            .and_then(crate::github_profiles::from_name)
        else {
            continue;
        };
        if row["provider"] == forge.key()
            && let Some((_, disk)) = latest.get(profile.runner)
        {
            row["disk_total_bytes"] = json!(disk.total_bytes);
        }
    }
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

pub(super) fn agent_update(
    settings: &Value,
    id: &str,
    token: &str,
    endpoint: Option<Endpoint>,
) -> Result<(), String> {
    if token.len() != 64 {
        return Err("Invalid lease credential".into());
    }
    let (forge, id) = agent_lease(id);
    let config = &settings[forge.settings_key()];
    let root = state_directory(config).ok_or("Missing worker lease state directory")?;
    let dir = lease_dir(root, id)?;
    let guard = File::open(dir.join("lock")).map_err(|error| error.to_string())?;
    lock(&guard)?;
    let mut lease = read_lease(&dir).map_err(|error| error.to_string())?;
    if repository(config, forge).as_deref() != Some(lease.repository.as_str()) {
        return Err("Invalid lease credential".into());
    }
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
            || !super::worker_ssh::valid_public_key(&endpoint.host_key)
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

fn agent_lease(id: &str) -> (Forge, &str) {
    match id.strip_prefix(Forge::GitLab.prefix()) {
        Some(id) => (Forge::GitLab, id),
        None => (Forge::GitHub, id),
    }
}

fn state_directory(config: &Value) -> Option<&Path> {
    config["stateDirectory"]
        .as_str()
        .map(Path::new)
        .filter(|path| path.is_absolute())
}

fn public_lease(mut lease: Lease) -> Value {
    lease.token_hash = None;
    json!(lease)
}

fn with_provider_links(mut row: Value, forge: Forge, config: &Value) -> Value {
    let Some(run) = row["run_id"].as_u64() else {
        return row;
    };
    let job = row["job_id"].as_u64();
    match forge {
        Forge::GitHub => {
            if let Some(repository) = config["repository"].as_str() {
                let run_url = format!("https://github.com/{repository}/actions/runs/{run}");
                if let Some(job) = job {
                    row["job_url"] = json!(format!("{run_url}/job/{job}"));
                }
                row["run_url"] = json!(run_url);
            }
        }
        Forge::GitLab => {
            if let (Some(instance), Some(project)) =
                (config["instance"].as_str(), config["project"].as_str())
            {
                let base = format!("{instance}/{project}/-");
                row["run_url"] = json!(format!("{base}/pipelines/{run}"));
                if let Some(job) = job {
                    row["job_url"] = json!(format!("{base}/jobs/{job}"));
                }
            }
        }
    }
    row
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
    if id.len() != 32 || !valid_worker_suffix(id) {
        return Err("Invalid worker lease ID".into());
    }
    Ok(root.join(id))
}

fn worker_lease_dir(root: &Path, suffix: &str) -> Result<PathBuf, String> {
    if suffix.len() != WORKER_SUFFIX_BYTES * 2 || !valid_worker_suffix(suffix) {
        return lease_dir(root, suffix);
    }
    let target =
        fs::read_link(root.join("names").join(suffix)).map_err(|error| error.to_string())?;
    let id = target
        .strip_prefix("..")
        .ok()
        .and_then(Path::to_str)
        .ok_or("Invalid worker lease alias")?;
    lease_dir(root, id)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn create(root: &Path, backend: &Backend<'_>, mut lease: Lease) -> Result<Value, String> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(root)
        .map_err(|error| error.to_string())?;
    let dir = lease_dir(root, &lease.id)?;
    DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .map_err(|error| error.to_string())?;
    let names = root.join("names");
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&names)
        .map_err(|error| error.to_string())?;
    loop {
        let suffix = random_hex::<WORKER_SUFFIX_BYTES>()?;
        // A symlink reserves the name atomically and records the assignment
        // ID without requiring a network lookup or a scan of past leases.
        match symlink(Path::new("..").join(&lease.id), names.join(&suffix)) {
            Ok(()) => {
                lease.worker_suffix = Some(suffix);
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.to_string()),
        }
    }
    let guard = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dir.join("lock"))
        .map_err(|error| error.to_string())?;
    lock(&guard)?;
    write_lease(&dir, &lease).map_err(|error| error.to_string())?;

    match backend.dispatch(&lease.id, &lease.profile) {
        Ok(run_id) => {
            lease.run_id = Some(run_id);
            lease.state = LeaseState::Queued;
            if let Err(error) = write_lease(&dir, &lease) {
                return Err(format!(
                    "Save pipeline run {run_id} for lease {}: {error}",
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
    backend: &Backend<'_>,
    cancel: bool,
) -> Result<(), String> {
    if lease.state == LeaseState::Ended {
        return Ok(());
    }
    let run_id = lease.run_id.ok_or("Pipeline run ID is unknown")?;
    let workflow = backend.status(run_id);
    let cancel = cancel || lease.state == LeaseState::Cancelling || now() >= lease.expires_at;
    if !cancel && workflow.is_err() {
        return workflow.map(|_| ());
    }
    let changed = match workflow {
        Ok(workflow) if lease.workflow.as_ref() != Some(&workflow) => {
            lease.workflow = Some(workflow);
            true
        }
        _ => false,
    };
    if lease
        .workflow
        .as_ref()
        .is_some_and(|workflow| workflow.status == "completed")
    {
        lease.state = LeaseState::Ended;
    } else if cancel {
        lease.state = LeaseState::Cancelling;
        write_lease(dir, lease).map_err(|error| error.to_string())?;
        return backend.cancel(run_id);
    } else if !changed {
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
