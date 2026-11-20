//! One leased worker per gateway. The state lock serializes provisioning;
//! the shared session lock keeps idle cleanup out of running builds.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::net::IpAddr;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

use super::mutations::Compute;
use super::{inventory_cache, mutations, openstack::OpenStack, openstack::setting, up, worker_ssh};
use crate::ctx::{Ctx, Fail, R};

const OWNER: &str = "rworker-builder@controller";

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
    if let Some((id, metadata)) = cloud.worker(prefix, name)? {
        if metadata.get("owner").and_then(Value::as_str) != Some(OWNER) {
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
) -> Result<i32, Box<dyn std::error::Error>> {
    let gate = lock(&directory.join("state.lock"), libc::LOCK_EX)?;
    let mut current = load(directory)?;
    if let Some(lease) = &current
        && let Some(_idle) = idle_lock(directory)?
        && (expired(lease, idle_seconds) || !ready(settings, lease))
    {
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
                OWNER,
                "--ssh-key",
                public_key,
                "--no-niks3-token",
                "--meta",
                "topic=On-demand Nix builder",
            ]
            .map(str::to_owned);
            let mut lease = up::up(settings, up::Args::parse(&args)?)?;
            // Retain the connection details, without writing the full up response.
            lease = json!({
                "name": lease["name"], "ip": lease["ip"],
                "host_key": lease["host_key"], "expires_at": lease["expires_at"],
            });
            inventory_cache::invalidate_current();
            lease
        }
    };
    lease["last_use"] = json!(now());
    save(directory, &lease)?;
    let active = lock(&directory.join("sessions.lock"), libc::LOCK_SH)?;
    drop(gate);

    let (ip, key) = connection(&lease)?;
    let result = worker_ssh::daemon_stdio(settings, ip, key);

    let _gate = lock(&directory.join("state.lock"), libc::LOCK_EX)?;
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
    let _gate = lock(&directory.join("state.lock"), libc::LOCK_EX)?;
    if let Some(lease) = load(directory)?
        && expired(&lease, idle_seconds)
        && let Some(_idle) = idle_lock(directory)?
    {
        remove(settings, directory, &lease)?;
    }
    Ok(())
}

pub(crate) fn run(ctx: &Ctx, args: &[String]) -> R {
    let [settings_path, directory, mode, profile, ttl, idle] = args else {
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
        session(&settings, directory, profile, ttl, idle_seconds)
    };
    match run() {
        Ok(code) => crate::ctx::status(code),
        Err(error) => {
            ctx.err(&format!("builder gateway: {error}"));
            Err(Fail(1))
        }
    }
}
