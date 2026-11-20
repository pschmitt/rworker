//! One leased worker per gateway. The state lock serializes provisioning;
//! the shared session lock keeps idle cleanup out of running builds.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::net::IpAddr;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

use super::mutations::Compute;
use super::{inventory_cache, mutations, openstack::OpenStack, openstack::setting, up, worker_ssh};
use crate::ctx::{Ctx, Fail, R};

const AGENT: &str = "nix build";
const TOPIC: &str = "On-demand Nix builder";
const LEGACY_OWNER: &str = "rworker-builder@controller";

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

fn client_ip(mut input: impl Read) -> io::Result<IpAddr> {
    let mut address = Vec::with_capacity(45);
    let mut byte = [0];
    for _ in 0..46 {
        input.read_exact(&mut byte)?;
        if byte[0] == b'\n' {
            return std::str::from_utf8(&address)
                .ok()
                .and_then(|value| value.parse().ok())
                .ok_or_else(|| io::Error::other("invalid builder client address"));
        }
        address.push(byte[0]);
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

fn archive_logs(settings: &Value, directory: &Path, lease: &Value) -> bool {
    let Ok(limits) = std::env::var("RWORKER_BUILDER_LOG_LIMITS") else {
        return true;
    };
    let collect = || -> Result<(), Box<dyn std::error::Error>> {
        let (ip, key) = connection(lease)?;
        let name = setting(lease, "name")?;
        let output = worker_ssh::run(settings, ip, key, BUILD_LOGS, None, 15)?;
        if !output.status.success() {
            return Err("worker log snapshot failed".into());
        }
        crate::cmd::log_cache::archive_builder(
            &directory.join("job-logs"),
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
        mutations::delete(settings, &cloud, &netbird, &id, name, &metadata)?;
        inventory_cache::invalidate_current();
    }
    fs::remove_file(directory.join("lease.json"))?;
    Ok(())
}

fn connection(lease: &Value) -> Result<(IpAddr, &str), Box<dyn std::error::Error>> {
    Ok((setting(lease, "ip")?.parse()?, setting(lease, "host_key")?))
}

fn ready(settings: &Value, lease: &Value) -> bool {
    connection(lease).is_ok_and(|(ip, key)| {
        worker_ssh::run(settings, ip, key, "true", None, 8)
            .is_ok_and(|output| output.status.success())
    })
}

fn session(
    settings: &Value,
    directory: &Path,
    profile: &str,
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
            archive_logs(settings, directory, lease);
        }
        remove(settings, directory, lease)?;
        current = None;
    }
    let mut lease = match current {
        Some(lease) => lease,
        None => {
            // Provisioning requires an agent login key. Reuse the controller's
            // existing worker identity without copying its private key.
            let key = Command::new("ssh-keygen")
                .args(["-y", "-P", "", "-f", setting(settings, "sshKeyFile")?])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()?;
            if !key.status.success() || key.stdout.is_empty() {
                return Err(
                    "could not read the public key for the controller's worker identity".into(),
                );
            }
            let public_key = std::str::from_utf8(&key.stdout)?.trim();
            let args = [
                "--profile",
                profile,
                "--ttl",
                ttl,
                "--owner",
                owner,
                "--ssh-key",
                public_key,
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
        if crate::cmd::history::record_builder(&directory.join("worker-history"), name, &record)
            .is_err()
        {
            eprintln!("Could not record builder worker {name} in history");
        }
    }
    let active = lock(&directory.join("sessions.lock"), libc::LOCK_SH)?;
    drop(gate);

    let (ip, key) = connection(&lease)?;
    let result = worker_ssh::daemon_stdio(settings, ip, key);

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
    let _gate = lock(&directory.join("state.lock"), libc::LOCK_EX)?;
    if let Some(mut lease) = load(directory)?
        && let Some(_idle) = idle_lock(directory)?
    {
        let archived = lease["logs_pending"] != false && archive_logs(settings, directory, &lease);
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
        let peer = client_ip(File::from(io::stdin().as_fd().try_clone_to_owned()?))?;
        let owner = client_name(names_file, peer)?;
        session(&settings, directory, profile, ttl, idle_seconds, &owner)
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
        assert_eq!(client_ip(&mut input)?.to_string(), "2001:db8::1");
        assert_eq!(input, b"\x00\xff");
        assert!(client_ip(b"invalid address\n".as_slice()).is_err());
        Ok(())
    }
}
