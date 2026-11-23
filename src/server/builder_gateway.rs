//! One leased worker per gateway target. The default target keeps its lease
//! in the state directory; selectors (`profile:NAME`, `arch:ARCH`,
//! `pool:NAME`, sent as the Nix `remote-program`) lease their own worker
//! under `leases/`. The state lock serializes provisioning; the shared
//! session lock keeps idle cleanup out of running builds.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::net::IpAddr;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

use super::mutations::Compute;
use super::{inventory_cache, mutations, openstack::OpenStack, openstack::setting, up, worker_ssh};
use crate::ctx::{Ctx, Fail, R};
use crate::github_profiles::{self, Forge, Profile};

const AGENT: &str = "nix build";
const TOPIC: &str = "On-demand Nix builder";
const LEGACY_OWNER: &str = "rworker-builder@controller";
/// Hosted leases serve Nix from an isolated store through the controller's
/// local relay port, as the runner's build account.
const HOSTED_USER: &str = "rworker-build";
const HOSTED_DAEMON: &str = "/home/rworker-build/.local/bin/rworker-nix-daemon --stdio";
const HOSTED_MAX_SECONDS: u64 = 2 * 3600;
const HOSTED_START_SECONDS: u64 = 15 * 60;

/// What a gateway connection leases.
enum Target {
    OpenStack(String),
    Hosted(&'static Profile),
}

impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OpenStack(profile) => write!(f, "OpenStack({profile})"),
            Self::Hosted(profile) => write!(f, "Hosted({})", profile.name),
        }
    }
}

impl PartialEq for Target {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::OpenStack(a), Self::OpenStack(b)) => a == b,
            (Self::Hosted(a), Self::Hosted(b)) => a.name == b.name,
            _ => false,
        }
    }
}

fn valid_selector(selector: &str) -> bool {
    selector.split_once(':').is_some_and(|(kind, name)| {
        matches!(kind, "profile" | "arch" | "pool")
            && (1..=64).contains(&name.len())
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    })
}

fn normalize_arch(value: &str) -> Option<&'static str> {
    match value.to_ascii_lowercase().as_str() {
        "x86_64" | "x86-64" | "amd64" | "x64" | "x86_64-linux" => Some("x86_64"),
        "aarch64" | "arm64" | "aarch64-linux" => Some("aarch64"),
        _ => None,
    }
}

fn openstack_profile(settings: &Value, name: &str) -> bool {
    settings["profiles"][name]
        .as_object()
        .is_some_and(|profile| !profile.is_empty())
}

fn profile_target(settings: &Value, name: &str) -> Result<Target, String> {
    if openstack_profile(settings, name) {
        return Ok(Target::OpenStack(name.to_owned()));
    }
    match github_profiles::from_name(name) {
        Some(profile) if profile.forge == Forge::GitHub => Ok(Target::Hosted(profile)),
        Some(_) => Err(format!(
            "profile {name} does not provide an isolated Nix builder store"
        )),
        None => Err(format!("unknown worker profile: {name}")),
    }
}

/// The profile a pool's workers use: a profile named like the pool, else
/// the profile of its current workers.
fn pool_profile(settings: &Value, pool: &str) -> Result<Option<String>, String> {
    if openstack_profile(settings, pool) {
        return Ok(Some(pool.to_owned()));
    }
    let workers = match inventory_cache::directory()
        .and_then(|directory| inventory_cache::read(&directory, false))
    {
        Some((workers, _)) => workers,
        None => super::inventory::list(settings)?,
    };
    Ok(workers
        .as_array()
        .into_iter()
        .flatten()
        .filter(|worker| worker["pool"] == pool)
        .find_map(|worker| worker["profile"].as_str().map(str::to_owned)))
}

fn resolve(
    settings: &Value,
    default: &str,
    selector: Option<&str>,
    pool: impl Fn(&str) -> Result<Option<String>, String>,
) -> Result<Target, String> {
    let Some(selector) = selector else {
        return Ok(Target::OpenStack(default.to_owned()));
    };
    let (kind, name) = selector.split_once(':').ok_or("invalid builder selector")?;
    match kind {
        "profile" => profile_target(settings, name),
        "pool" => {
            let profile = pool(name)?.ok_or_else(|| format!("unknown worker pool: {name}"))?;
            profile_target(settings, &profile)
        }
        "arch" => {
            let arch =
                normalize_arch(name).ok_or_else(|| format!("unknown architecture: {name}"))?;
            let arch_of = |profile: &str| {
                settings["profiles"][profile]["arch"]
                    .as_str()
                    .and_then(normalize_arch)
            };
            if arch_of(default).unwrap_or("x86_64") == arch {
                return Ok(Target::OpenStack(default.to_owned()));
            }
            let mut names: Vec<&String> = settings["profiles"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(name, _)| name)
                .filter(|name| openstack_profile(settings, name) && arch_of(name) == Some(arch))
                .collect();
            names.sort();
            if let Some(name) = names.first() {
                return Ok(Target::OpenStack((*name).clone()));
            }
            let system = format!("{arch}-linux");
            github_profiles::PROFILES
                .iter()
                .find(|profile| profile.forge == Forge::GitHub && profile.system == system)
                .map(Target::Hosted)
                .ok_or_else(|| format!("no profile builds {arch}"))
        }
        _ => Err("invalid builder selector".into()),
    }
}

/// The lease directory of a target. The default profile keeps the
/// original location, so a running lease survives an upgrade.
fn lease_directory(base: &Path, default: &str, target: &Target) -> io::Result<std::path::PathBuf> {
    let key = match target {
        Target::OpenStack(profile) if profile == default => return Ok(base.to_owned()),
        Target::OpenStack(profile) => format!("openstack-{profile}"),
        Target::Hosted(profile) => format!("github-{}", profile.name),
    };
    let directory = base.join("leases").join(key);
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&directory)?;
    Ok(directory)
}

fn hosted(lease: &Value) -> bool {
    lease["kind"] == "github"
}

fn lock(path: &Path, operation: libc::c_int) -> io::Result<File> {
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    loop {
        // SAFETY: flock uses the descriptor only while this file is alive.
        if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
            return Ok(file);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn idle_lock(directory: &Path) -> io::Result<Option<File>> {
    match lock(
        &directory.join("sessions.lock"),
        libc::LOCK_EX | libc::LOCK_NB,
    ) {
        Ok(lock) => Ok(Some(lock)),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(error) => Err(error),
    }
}

/// The gate's preface line: the client address and an optional selector.
fn client_preface(mut input: impl Read) -> io::Result<(IpAddr, Option<String>)> {
    let mut line = Vec::with_capacity(128);
    let mut byte = [0];
    for _ in 0..128 {
        input.read_exact(&mut byte)?;
        if byte[0] == b'\n' {
            let invalid = || io::Error::other("invalid builder client preface");
            let line = std::str::from_utf8(&line).map_err(|_| invalid())?;
            let (address, selector) = match line.split_once(' ') {
                Some((address, selector)) => (address, Some(selector)),
                None => (line, None),
            };
            if selector.is_some_and(|selector| !valid_selector(selector)) {
                return Err(invalid());
            }
            let address = address.parse().map_err(|_| invalid())?;
            return Ok((address, selector.map(str::to_owned)));
        }
        line.push(byte[0]);
    }
    Err(io::Error::other("missing builder client address"))
}

fn client_name(path: &str, peer: IpAddr) -> Result<String, Box<dyn std::error::Error>> {
    if path != "-" {
        let names: std::collections::HashMap<IpAddr, String> =
            serde_json::from_reader(File::open(path)?)?;
        if let Some(name) = names.get(&peer).filter(|name| !name.trim().is_empty()) {
            return Ok(name.trim().to_owned());
        }
    }
    Ok(peer.to_string())
}

fn save(directory: &Path, lease: &Value) -> io::Result<()> {
    let temporary = directory.join(".lease.tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)?;
    serde_json::to_writer(&mut file, lease)?;
    file.flush()?;
    fs::rename(temporary, directory.join("lease.json"))
}

fn load(directory: &Path) -> io::Result<Option<Value>> {
    match File::open(directory.join("lease.json")) {
        Ok(file) => serde_json::from_reader(file)
            .map(Some)
            .map_err(io::Error::other),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn now() -> u64 {
    super::epoch() as u64
}

fn expired(lease: &Value, idle_seconds: u64) -> bool {
    let last_use = lease["last_use"].as_u64().unwrap_or_default();
    let expiry = lease["expires_at"].as_u64().unwrap_or_default();
    now() >= expiry || now().saturating_sub(last_use) >= idle_seconds
}

// Read the most recent derivation logs in one SSH request. Each frame is
// a validated filename and byte count followed by unmodified log bytes.
const BUILD_LOGS: &str = r#"set -eu
cd /nix/var/log/nix/drvs 2>/dev/null || exit 0
scratch=$(mktemp)
trap 'rm -f -- "$scratch"' EXIT
find . -mindepth 2 -maxdepth 2 -type f -printf '%T@ %p\n' |
  sort -nr | head -n 32 | while IFS=' ' read -r _ path; do
    folder=${path%/*}
    name=${folder##*/}${path##*/}
    case "$path" in
      *.drv.bz2)
        bzip2 -dc -- "$path" | tail -c 1048576 > "$scratch"
        name=${name%.bz2}
        ;;
      *.drv) tail -c 1048576 -- "$path" > "$scratch" ;;
      *) continue ;;
    esac
    printf '%s %s\n' "$name" "$(wc -c < "$scratch")"
    cat -- "$scratch"
  done
"#;

fn archive_logs(settings: &Value, base: &Path, lease: &Value) -> bool {
    let Ok(limits) = std::env::var("RWORKER_BUILDER_LOG_LIMITS") else {
        return true;
    };
    if hosted(lease) {
        return true;
    }
    let collect = || -> Result<(), Box<dyn std::error::Error>> {
        let (ip, key) = connection(lease)?;
        let name = setting(lease, "name")?;
        let output = worker_ssh::run(settings, ip, key, BUILD_LOGS, None, 15)?;
        if !output.status.success() {
            return Err("worker log snapshot failed".into());
        }
        crate::cmd::log_cache::archive_builder(
            &base.join("job-logs"),
            name,
            &output.stdout,
            &limits,
        )?;
        Ok(())
    };
    if let Err(error) = collect() {
        eprintln!("Could not archive Nix build logs: {error}");
        return false;
    }
    true
}

fn remove(
    settings: &Value,
    directory: &Path,
    lease: &Value,
) -> Result<(), Box<dyn std::error::Error>> {
    if hosted(lease) {
        let id = setting(lease, "id")?.to_owned();
        let owner = setting(lease, "owner")?.to_owned();
        if let Err(error) = super::github_leases::run(settings, Forge::GitHub, "down", &[id, owner])
        {
            // An ended lease cannot be cancelled; keep retrying only while
            // it could still be running.
            if now() < lease["expires_at"].as_u64().unwrap_or_default() {
                return Err(error.into());
            }
            eprintln!("Could not end hosted builder lease: {error}");
        }
        fs::remove_file(directory.join("lease.json"))?;
        return Ok(());
    }
    let name = setting(lease, "name")?;
    let prefix = setting(settings, "adhocPrefix")?;
    if !name.starts_with(prefix) || prefix.is_empty() {
        return Err("lease does not identify an ad-hoc worker".into());
    }
    let mut cloud = OpenStack::from_settings(settings)?;
    let pools = super::pools::Pools::from_settings(settings)?;
    if let Some((id, metadata)) = cloud.worker(name)? {
        if pools.classify(name, &metadata) != Some(super::pools::Kind::Adhoc) {
            return Err("lease does not identify an ad-hoc worker".into());
        }
        if metadata.get("owner").and_then(Value::as_str)
            != Some(lease["owner"].as_str().unwrap_or(LEGACY_OWNER))
        {
            return Err("worker is no longer owned by the builder gateway".into());
        }
        let netbird = super::netbird::NetBird::from_settings(settings)?;
        mutations::delete(
            settings,
            &cloud,
            &netbird,
            &id,
            name,
            &metadata,
            ("builder gateway", "idle or unreachable lease"),
        )?;
        inventory_cache::invalidate_current();
    }
    fs::remove_file(directory.join("lease.json"))?;
    Ok(())
}

fn connection(lease: &Value) -> Result<(IpAddr, &str), Box<dyn std::error::Error>> {
    Ok((setting(lease, "ip")?.parse()?, setting(lease, "host_key")?))
}

fn ready(settings: &Value, lease: &Value) -> bool {
    if hosted(lease) {
        let Ok(id) = setting(lease, "id") else {
            return false;
        };
        return super::github_leases::run(settings, Forge::GitHub, "status", &[id.to_owned()])
            .is_ok_and(|status| {
                status["state"] == "ready" && status["endpoint"]["port"] == lease["port"]
            });
    }
    connection(lease).is_ok_and(|(ip, key)| {
        worker_ssh::run(settings, ip, key, "true", None, 8)
            .is_ok_and(|output| output.status.success())
    })
}

fn controller_public_key(settings: &Value) -> Result<String, Box<dyn std::error::Error>> {
    // Provisioning requires an agent login key. Reuse the controller's
    // existing worker identity without copying its private key.
    let key = Command::new("ssh-keygen")
        .args(["-y", "-P", "", "-f", setting(settings, "sshKeyFile")?])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()?;
    if !key.status.success() || key.stdout.is_empty() {
        return Err("could not read the public key for the controller's worker identity".into());
    }
    Ok(std::str::from_utf8(&key.stdout)?.trim().to_owned())
}

/// Start a hosted lease and wait until its relay endpoint is ready.
fn hosted_up(
    settings: &Value,
    profile: &Profile,
    ttl: &str,
    owner: &str,
) -> Result<Value, Box<dyn std::error::Error>> {
    let ttl = super::duration_seconds(ttl)
        .unwrap_or(HOSTED_MAX_SECONDS)
        .clamp(60, HOSTED_MAX_SECONDS);
    let lease = super::github_leases::run(
        settings,
        Forge::GitHub,
        "up",
        &[
            profile.runner.to_owned(),
            ttl.to_string(),
            controller_public_key(settings)?,
            owner.to_owned(),
            TOPIC.to_owned(),
        ],
    )?;
    let id = setting(&lease, "id")?.to_owned();
    let name = format!(
        "{}{}",
        Forge::GitHub.prefix(),
        lease["worker_suffix"].as_str().unwrap_or(&id)
    );
    let deadline = now() + HOSTED_START_SECONDS;
    loop {
        match super::github_leases::run(
            settings,
            Forge::GitHub,
            "status",
            std::slice::from_ref(&id),
        ) {
            Ok(status) => {
                if status["state"] == "ready"
                    && let (Some(port), Some(host_key)) = (
                        status["endpoint"]["port"].as_u64(),
                        status["endpoint"]["host_key"].as_str(),
                    )
                {
                    return Ok(json!({
                        "kind": "github", "id": id, "name": name,
                        "ip": "127.0.0.1", "port": port, "host_key": host_key,
                        "expires_at": status["expires_at"], "owner": owner,
                        "profile": profile.name, "created_at": jiff::Timestamp::now().to_string(),
                    }));
                }
                if matches!(status["state"].as_str(), Some("ended" | "cancelling")) {
                    return Err(format!("hosted lease {name} ended before it was ready").into());
                }
            }
            Err(error) => eprintln!("Hosted lease {name}: {error}"),
        }
        if now() >= deadline {
            let _ = super::github_leases::run(
                settings,
                Forge::GitHub,
                "down",
                &[id.clone(), owner.to_owned()],
            );
            return Err(format!("hosted lease {name} was not ready in time").into());
        }
        std::thread::sleep(Duration::from_secs(5));
    }
}

#[allow(clippy::too_many_arguments)]
fn session(
    settings: &Value,
    base: &Path,
    directory: &Path,
    target: &Target,
    ttl: &str,
    idle_seconds: u64,
    owner: &str,
) -> Result<i32, Box<dyn std::error::Error>> {
    let gate = lock(&directory.join("state.lock"), libc::LOCK_EX)?;
    let mut current = load(directory)?;
    if let Some(lease) = &current
        && let Some(_idle) = idle_lock(directory)?
        && (expired(lease, idle_seconds) || !ready(settings, lease))
    {
        if expired(lease, idle_seconds) && lease["logs_pending"] != false {
            archive_logs(settings, base, lease);
        }
        remove(settings, directory, lease)?;
        current = None;
    }
    let mut lease = match (current, target) {
        (Some(lease), _) => lease,
        (None, Target::Hosted(profile)) => {
            let lease = hosted_up(settings, profile, ttl, owner)?;
            inventory_cache::invalidate_current();
            lease
        }
        (None, Target::OpenStack(profile)) => {
            let public_key = controller_public_key(settings)?;
            let profile = profile.as_str();
            let args = [
                "--profile",
                profile,
                "--ttl",
                ttl,
                "--owner",
                owner,
                "--ssh-key",
                &public_key,
                "--no-niks3-token",
                "--meta",
                &format!("topic={TOPIC}"),
                "--meta",
                &format!("agent={AGENT}"),
            ]
            .map(str::to_owned);
            let mut lease = up::up(settings, up::Args::parse(&args)?)?;
            // Retain the connection details, without writing the full up response.
            lease = json!({
                "name": lease["name"], "ip": lease["ip"],
                "host_key": lease["host_key"], "expires_at": lease["expires_at"],
                "owner": owner, "profile": profile, "created_at": jiff::Timestamp::now().to_string(),
            });
            inventory_cache::invalidate_current();
            lease
        }
    };
    lease["last_use"] = json!(now());
    save(directory, &lease)?;
    if lease["created_at"].is_string() {
        let record = json!({
            "owner": lease["owner"], "profile": lease["profile"], "created_at": lease["created_at"],
            "topic": TOPIC, "agent": AGENT, "status": "ACTIVE",
        });
        let name = setting(&lease, "name")?;
        if crate::cmd::history::record_builder(&base.join("worker-history"), name, &record).is_err()
        {
            eprintln!("Could not record builder worker {name} in history");
        }
    }
    let active = lock(&directory.join("sessions.lock"), libc::LOCK_SH)?;
    drop(gate);

    let (ip, key) = connection(&lease)?;
    let result = match lease["port"].as_u64() {
        Some(port) if hosted(&lease) => worker_ssh::daemon_stdio_at(
            settings,
            ip,
            u16::try_from(port)?,
            HOSTED_USER,
            key,
            HOSTED_DAEMON,
        ),
        _ => worker_ssh::daemon_stdio(settings, ip, key),
    };

    let _gate = lock(&directory.join("state.lock"), libc::LOCK_EX)?;
    lease["logs_pending"] = json!(true);
    lease["last_use"] = json!(now());
    save(directory, &lease)?;
    drop(active);
    Ok(result?)
}

fn reap(
    settings: &Value,
    directory: &Path,
    idle_seconds: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let logs = directory.join("job-logs");
    if logs.is_dir()
        && let Ok(limits) = std::env::var("RWORKER_BUILDER_LOG_LIMITS")
        && let Err(error) = crate::cmd::log_cache::archive_builder(&logs, "prune", &[], &limits)
    {
        eprintln!("Could not prune Nix build logs: {error}");
    }
    let mut directories = vec![directory.to_owned()];
    if let Ok(entries) = fs::read_dir(directory.join("leases")) {
        directories.extend(
            entries
                .flatten()
                .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                .map(|entry| entry.path()),
        );
    }
    for lease_directory in directories {
        if let Err(error) = reap_one(settings, directory, &lease_directory, idle_seconds) {
            eprintln!(
                "Could not reap builder lease {}: {error}",
                lease_directory.display()
            );
        }
    }
    Ok(())
}

fn reap_one(
    settings: &Value,
    base: &Path,
    directory: &Path,
    idle_seconds: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let _gate = lock(&directory.join("state.lock"), libc::LOCK_EX)?;
    if let Some(mut lease) = load(directory)?
        && let Some(_idle) = idle_lock(directory)?
    {
        let archived = lease["logs_pending"] != false && archive_logs(settings, base, &lease);
        if expired(&lease, idle_seconds) {
            remove(settings, directory, &lease)?;
        } else if archived {
            lease["logs_pending"] = json!(false);
            save(directory, &lease)?;
        }
    }
    Ok(())
}

pub(crate) fn run(ctx: &Ctx, args: &[String]) -> R {
    let [
        settings_path,
        directory,
        mode,
        profile,
        ttl,
        idle,
        names_file,
    ] = args
    else {
        return Err(Fail(2));
    };
    if !matches!(mode.as_str(), "stdio" | "reap") {
        return Err(Fail(2));
    }
    let idle_seconds = idle
        .parse()
        .ok()
        .filter(|seconds| *seconds > 0)
        .ok_or(Fail(2))?;
    let run = || -> Result<i32, Box<dyn std::error::Error>> {
        let settings = serde_json::from_reader(File::open(settings_path)?)?;
        let directory = Path::new(directory);
        if mode == "reap" {
            reap(&settings, directory, idle_seconds)?;
            return Ok(0);
        }
        // Read only the SSH address preface. A buffered reader could swallow
        // Nix protocol bytes before the SSH child inherits this descriptor.
        let (peer, selector) =
            client_preface(File::from(io::stdin().as_fd().try_clone_to_owned()?))?;
        let owner = client_name(names_file, peer)?;
        let target = resolve(&settings, profile, selector.as_deref(), |pool| {
            pool_profile(&settings, pool)
        })?;
        let leases = lease_directory(directory, profile, &target)?;
        session(
            &settings,
            directory,
            &leases,
            &target,
            ttl,
            idle_seconds,
            &owner,
        )
    };
    match run() {
        Ok(code) => crate::ctx::status(code),
        Err(error) => {
            ctx.err(&format!("builder gateway: {error}"));
            Err(Fail(1))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_address_preface_leaves_nix_bytes_unread() -> io::Result<()> {
        let mut input: &[u8] = b"2001:db8::1\n\x00\xff";
        let (address, selector) = client_preface(&mut input)?;
        assert_eq!(address.to_string(), "2001:db8::1");
        assert_eq!(selector, None);
        assert_eq!(input, b"\x00\xff");
        let mut input: &[u8] = b"192.0.2.1 arch:aarch64\n\x00";
        let (_, selector) = client_preface(&mut input)?;
        assert_eq!(selector.as_deref(), Some("arch:aarch64"));
        assert_eq!(input, b"\x00");
        assert!(client_preface(b"invalid address\n".as_slice()).is_err());
        assert!(client_preface(b"192.0.2.1 shell:sh\n".as_slice()).is_err());
        assert!(client_preface(b"192.0.2.1 profile:a b\n".as_slice()).is_err());
        Ok(())
    }

    #[test]
    fn selectors_pick_a_profile_or_a_hosted_lease() {
        let settings = json!({"profiles": {
            "standard": {"arch": "x86_64"},
            "large-builder": {"arch": "x86_64"},
        }});
        let no_pools = |_: &str| Ok(None);
        let resolve = |selector| resolve(&settings, "standard", selector, no_pools);
        let openstack = |name: &str| Ok(Target::OpenStack(name.into()));
        assert_eq!(resolve(None), openstack("standard"));
        assert_eq!(
            resolve(Some("profile:large-builder")),
            openstack("large-builder")
        );
        assert_eq!(resolve(Some("arch:amd64")), openstack("standard"));
        assert_eq!(
            resolve(Some("arch:arm64")),
            Ok(Target::Hosted(
                github_profiles::from_name("gh-aarch64").unwrap()
            ))
        );
        assert_eq!(
            resolve(Some("profile:gh-aarch64")),
            Ok(Target::Hosted(
                github_profiles::from_name("gh-aarch64").unwrap()
            ))
        );
        assert!(resolve(Some("profile:gl-x86_64")).is_err());
        assert!(resolve(Some("profile:missing")).is_err());
        assert!(resolve(Some("arch:riscv64")).is_err());
        assert!(resolve(Some("pool:unknown")).is_err());
        let pools = |pool: &str| Ok((pool == "builders").then(|| "large-builder".to_owned()));
        assert_eq!(
            super::resolve(&settings, "standard", Some("pool:builders"), pools),
            openstack("large-builder")
        );
    }
}
