use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::Value;

use super::{niks3::token_id, openstack::setting};

const WEB_WORKER_KEY: &str = "rworker-web-worker-public-key";

pub(super) fn valid_public_key(key: &str) -> bool {
    if key.len() > 4096 || key.contains(['\r', '\n']) {
        return false;
    }
    let mut fields = key.split_ascii_whitespace();
    matches!(fields.next(), Some("ssh-ed25519" | "ssh-rsa"))
        && fields
            .next()
            .is_some_and(|encoded| STANDARD.decode(encoded).is_ok_and(|key| !key.is_empty()))
}

fn restrict_web_key(key: &str, existing: &[&str]) -> Result<Option<String>, String> {
    let key = key.trim();
    if !valid_public_key(key) {
        return Err("Invalid web console SSH public key".into());
    }
    let mut fields = key.split_ascii_whitespace();
    let kind = fields.next().expect("validated key type");
    let blob = fields.next().expect("validated key blob");
    if existing
        .iter()
        .any(|key| key.split_ascii_whitespace().take(2).eq([kind, blob]))
    {
        return Ok(None);
    }
    Ok(Some(format!(
        "no-agent-forwarding,no-port-forwarding,no-X11-forwarding,no-user-rc {kind} {blob}"
    )))
}

/// Add the configured console identity only to workers owned by its operator.
pub(super) fn web_worker_key(owner: &str, existing: &[&str]) -> Result<Option<String>, String> {
    let mut owner = owner.split('@');
    let operator = std::env::var("RWORKER_WEB_WORKER_OWNER").ok();
    let (Some(user), Some(host), None) = (owner.next(), owner.next(), owner.next()) else {
        return Ok(None);
    };
    if operator.as_deref() != Some(user) || user.is_empty() || host.is_empty() {
        return Ok(None);
    }
    let directory = std::env::var_os("CREDENTIALS_DIRECTORY")
        .ok_or("Missing systemd credentials directory for web worker access")?;
    let key = fs::read_to_string(Path::new(&directory).join(WEB_WORKER_KEY))
        .map_err(|error| format!("Read web console SSH public key: {error}"))?;
    restrict_web_key(&key, existing)
}

/// The console identity for a pool's workers, when its autoscaler was given
/// the key as a systemd credential.
pub(super) fn web_pool_key() -> Result<Option<String>, String> {
    let Some(directory) = std::env::var_os("CREDENTIALS_DIRECTORY") else {
        return Ok(None);
    };
    match fs::read_to_string(Path::new(&directory).join(WEB_WORKER_KEY)) {
        Ok(key) => restrict_web_key(&key, &[]),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("Read web console SSH public key: {error}")),
    }
}

struct KnownHosts(PathBuf);

impl Drop for KnownHosts {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub(super) fn answers(ip: IpAddr) -> bool {
    TcpStream::connect_timeout(&SocketAddr::new(ip, 22), Duration::from_secs(4)).is_ok()
}

fn pinned_hosts(settings: &Value, ip: IpAddr, host_key: &str) -> Result<KnownHosts, String> {
    pinned_host(settings, &ip.to_string(), host_key)
}

fn pinned_host(settings: &Value, host: &str, host_key: &str) -> Result<KnownHosts, String> {
    if host_key.contains(['\r', '\n']) {
        return Err("Invalid worker host key".into());
    }
    let state = Path::new(setting(settings, "stateDir")?);
    let path = state.join(format!(
        "known_hosts.{}",
        token_id().map_err(|err| format!("Generate known-hosts suffix: {err}"))?
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(|err| format!("Create worker known-hosts file: {err}"))?;
    let known = KnownHosts(path);
    writeln!(file, "{host} {host_key}").map_err(|err| format!("Write worker host key: {err}"))?;
    drop(file);
    Ok(known)
}

/// A Nix remote store connection leaves both protocol streams untouched.
pub(super) fn daemon_stdio(settings: &Value, ip: IpAddr, host_key: &str) -> Result<i32, String> {
    let user = setting(settings, "sshUser")?;
    daemon_stdio_at(settings, ip, 22, user, host_key, "nix-daemon --stdio")
}

/// Like `daemon_stdio`, for a daemon behind another port, account or
/// program (a hosted lease's local relay and its isolated store).
pub(super) fn daemon_stdio_at(
    settings: &Value,
    ip: IpAddr,
    port: u16,
    user: &str,
    host_key: &str,
    remote: &str,
) -> Result<i32, String> {
    let host = if port == 22 {
        ip.to_string()
    } else {
        format!("[{ip}]:{port}")
    };
    let known = pinned_host(settings, &host, host_key)?;
    let status = Command::new("ssh")
        .args(["-T", "-F", "/dev/null", "-p", &port.to_string(), "-i"])
        .arg(setting(settings, "sshKeyFile")?)
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "IdentityAgent=none",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "GlobalKnownHostsFile=/dev/null",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "ServerAliveInterval=15",
            "-o",
            "ServerAliveCountMax=2",
            "-o",
        ])
        .arg(format!("UserKnownHostsFile={}", known.0.display()))
        .args(["--", &format!("{user}@{ip}"), remote])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|error| format!("Connect to worker Nix daemon: {error}"))?;
    Ok(crate::proc::code_of(status))
}

pub(super) fn run(
    settings: &Value,
    ip: IpAddr,
    host_key: &str,
    remote: &str,
    input: Option<&str>,
    timeout_secs: u64,
) -> Result<Output, String> {
    let known = pinned_hosts(settings, ip, host_key)?;
    let user = setting(settings, "sshUser")?;
    let mut child = Command::new("timeout")
        .args(["--kill-after=5", &timeout_secs.to_string(), "ssh", "-i"])
        .arg(setting(settings, "sshKeyFile")?)
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "GlobalKnownHostsFile=/dev/null",
            "-o",
        ])
        .arg(format!("UserKnownHostsFile={}", known.0.display()))
        .args(["--", &format!("{user}@{ip}"), remote])
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("Start worker SSH: {err}"))?;
    if let Some(input) = input
        && let Some(mut stdin) = child.stdin.take()
        && let Err(err) = stdin.write_all(input.as_bytes())
    {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("Write worker SSH input: {err}"));
    }
    child
        .wait_with_output()
        .map_err(|err| format!("Wait for worker SSH: {err}"))
}

pub(super) fn route(settings: &Value, name: &str) -> Result<Option<IpAddr>, String> {
    routes(settings, name).map(|(route, _)| route)
}

type Routes = (Option<IpAddr>, serde_json::Map<String, Value>);

pub(super) fn routes(settings: &Value, name: &str) -> Result<Routes, String> {
    let peers = super::netbird::NetBird::from_settings(settings)?.peers();
    let primary: Option<IpAddr> = peers
        .as_ref()
        .ok()
        .and_then(|peers| peers.iter().find(|peer| peer["name"] == name))
        .and_then(|peer| peer["ip"].as_str())
        .and_then(|ip| ip.parse().ok());
    let mut secondary = None;
    if settings["tailscale"].is_object() {
        match super::tailscale::Tailscale::from_settings(&settings["tailscale"])
            .and_then(|mut client| client.devices())
        {
            Ok(devices) => {
                secondary = devices
                    .get(name)
                    .and_then(|device| device["addresses"].as_array())
                    .and_then(|addresses| {
                        addresses
                            .iter()
                            .filter_map(Value::as_str)
                            .find_map(|ip| ip.parse::<std::net::Ipv4Addr>().ok())
                    })
                    .map(std::net::IpAddr::V4);
            }
            Err(error) => eprintln!("Tailscale device lookup for {name}: {error}"),
        }
    }
    // Without its API, NetBird is only optional for a worker Tailscale knows:
    // "no address" must not look like "gone" to the reaper.
    if let Err(error) = peers {
        if secondary.is_none() {
            return Err(error);
        }
        eprintln!("{name}: using Tailscale only: {error}");
    }
    let ips = [("netbird", primary), ("tailscale", secondary)]
        .into_iter()
        .filter_map(|(overlay, ip)| Some((overlay.into(), Value::String(ip?.to_string()))))
        .collect();
    Ok((
        primary.into_iter().chain(secondary).find(|ip| answers(*ip)),
        ips,
    ))
}

pub(super) fn activity(
    settings: &Value,
    name: &str,
    host_key: &str,
    workloads_only: bool,
) -> Result<Option<bool>, String> {
    let (preferred, overlays) = routes(settings, name)?;
    let mut routes = preferred.into_iter();
    let mut alternate = overlays
        .values()
        .filter_map(Value::as_str)
        .filter_map(|ip| ip.parse::<IpAddr>().ok())
        .filter(|ip| Some(*ip) != preferred);
    let result = routes.find_map(|ip| active(settings, ip, host_key, workloads_only));
    Ok(if workloads_only {
        result.or_else(|| alternate.find_map(|ip| active(settings, ip, host_key, workloads_only)))
    } else {
        result
    })
}

fn active(settings: &Value, ip: IpAddr, host_key: &str, workloads_only: bool) -> Option<bool> {
    let process_filter = if workloads_only {
        // pgrep matches Linux comm, which is limited to 15 bytes.
        "-x 'nix|nix-build|nix-store|cargo.*|rustc|gcc.*|g\\+\\+.*|cc|cc1.*|c\\+\\+|clang.*|make|gmake|ninja|cmake|ld|ld\\.lld|mold|go|javac|gradle.*|meson|bazel.*|buck2|zig'".to_owned()
    } else {
        let users = settings["busyUsers"]
            .as_array()?
            .iter()
            .map(Value::as_str)
            .collect::<Option<Vec<_>>>()?
            .join(",");
        format!("-u {}", crate::proc::quote_words(&[users]))
    };
    let command = format!(
        "printf '%s %s %s\\n' \"$(pgrep -c {process_filter} || true)\" \"$(pgrep -c -G nixbld || true)\" \"$(cut -d' ' -f1 /proc/loadavg)\""
    );
    let output = match run(settings, ip, host_key, &command, None, 30) {
        Ok(output) if output.status.success() => output,
        Ok(output) => {
            eprintln!("checking {ip}: SSH {}", output.status);
            return None;
        }
        Err(error) => {
            eprintln!("checking {ip}: {error}");
            return None;
        }
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let mut fields = text.split_whitespace();
    let processes = fields.next()?.parse::<i64>().ok()?;
    let builds = fields.next()?.parse::<i64>().ok()?;
    let load = fields.next()?.parse::<f64>().ok()?;
    Some(processes > 0 || builds > 0 || load > 1.0)
}

#[cfg(test)]
mod web_key_tests {
    use super::*;

    #[test]
    fn restricts_the_console_key_and_ignores_comments_for_deduplication() {
        let key = "ssh-ed25519 Zml4dHVyZQ== console";
        let restricted = restrict_web_key(key, &[]).unwrap().unwrap();
        assert!(
            restricted.starts_with(
                "no-agent-forwarding,no-port-forwarding,no-X11-forwarding,no-user-rc "
            )
        );
        assert!(restricted.ends_with("ssh-ed25519 Zml4dHVyZQ=="));
        assert_eq!(
            restrict_web_key(key, &["ssh-ed25519 Zml4dHVyZQ== another comment"]).unwrap(),
            None
        );
        assert!(
            restrict_web_key("ssh-ed25519 Zml4dHVyZQ==\nssh-ed25519 Zml4dHVyZQ==", &[]).is_err()
        );
    }
}
