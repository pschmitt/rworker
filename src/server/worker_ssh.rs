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

pub(crate) fn valid_extra_public_key(key: &str) -> bool {
    if key.len() > 4096 || key.contains(['\r', '\n', '\0']) {
        return false;
    }
    let mut fields = key.split_ascii_whitespace();
    let Some(kind @ ("ssh-ed25519" | "ssh-rsa")) = fields.next() else {
        return false;
    };
    let Some(mut data) = fields.next().and_then(|s| STANDARD.decode(s).ok()) else {
        return false;
    };
    fn part(data: &mut Vec<u8>) -> Option<Vec<u8>> {
        let length = u32::from_be_bytes(data.get(..4)?.try_into().ok()?) as usize;
        if length > data.len().checked_sub(4)? {
            return None;
        }
        data.drain(..4);
        Some(data.drain(..length).collect())
    }
    if part(&mut data).as_deref() != Some(kind.as_bytes()) {
        return false;
    }
    let valid = match kind {
        "ssh-ed25519" => part(&mut data).is_some_and(|key| key.len() == 32),
        "ssh-rsa" => {
            part(&mut data).is_some_and(|e| !e.is_empty())
                && part(&mut data).is_some_and(|n| !n.is_empty())
        }
        _ => false,
    };
    valid && data.is_empty()
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

/// The operator's own client keys (`RWORKER_OPERATOR_KEYS_FILE`, one key per
/// line) for workers owned by one of the operator's devices
/// (`RWORKER_OPERATOR_KEYS_OWNER@DEVICE`), so `jobs`, `logs` and `ssh` from any
/// of the operator's workstations reach a worker leased by, for example, a
/// phone. Restricted like the console key; keys already present are skipped.
pub(super) fn operator_keys(owner: &str, existing: &[&str]) -> Result<Vec<String>, String> {
    let Ok(operator) = std::env::var("RWORKER_OPERATOR_KEYS_OWNER") else {
        return Ok(Vec::new());
    };
    if !owned_by(owner, &operator) {
        return Ok(Vec::new());
    }
    let path = std::env::var_os("RWORKER_OPERATOR_KEYS_FILE")
        .ok_or("Missing RWORKER_OPERATOR_KEYS_FILE for the operator keys")?;
    let text = fs::read_to_string(&path)
        .map_err(|error| format!("Read operator SSH public keys: {error}"))?;
    restricted_keys(&text, existing)
}

/// Whether `owner` is `OPERATOR@HOST` for this operator.
fn owned_by(owner: &str, operator: &str) -> bool {
    let mut parts = owner.split('@');
    matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some(user), Some(host), None) if user == operator && !user.is_empty() && !host.is_empty()
    )
}

/// The keys of an authorized_keys-like text (blank lines and comments
/// skipped), restricted and without those already in `existing`.
fn restricted_keys(text: &str, existing: &[&str]) -> Result<Vec<String>, String> {
    // Keys are compared by type and blob, also behind an options prefix.
    let blob = |line: &str| -> Option<(String, String)> {
        let fields: Vec<&str> = line.split_ascii_whitespace().collect();
        fields
            .iter()
            .position(|field| matches!(*field, "ssh-ed25519" | "ssh-rsa"))
            .and_then(|at| Some((fields[at].to_owned(), (*fields.get(at + 1)?).to_owned())))
    };
    let mut seen: Vec<(String, String)> = existing.iter().filter_map(|key| blob(key)).collect();
    let mut keys = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let key = restrict_web_key(line, &[]).map_err(|_| {
            format!(
                "Invalid operator SSH public key: {}",
                line.split_ascii_whitespace().next().unwrap_or("")
            )
        })?;
        let Some(key) = key else { continue };
        let id = blob(&key).expect("a restricted key has a type and blob");
        if !seen.contains(&id) {
            seen.push(id);
            keys.push(key);
        }
    }
    Ok(keys)
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
    let (routes, netbird_error) = lookup(settings, name);
    match netbird_error {
        // Without its API, NetBird is only optional for a worker Tailscale
        // knows: "no address" must not look like "gone".
        Some(error) if routes.1.is_empty() => Err(error),
        Some(error) => {
            eprintln!("{name}: using Tailscale only: {error}");
            Ok(routes)
        }
        None => Ok(routes),
    }
}

/// The routes to a worker, and why NetBird could not be asked.
fn lookup(settings: &Value, name: &str) -> (Routes, Option<String>) {
    let peers = super::netbird::NetBird::from_settings(settings).peers();
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
    let ips = [("netbird", primary), ("tailscale", secondary)]
        .into_iter()
        .filter_map(|(overlay, ip)| Some((overlay.into(), Value::String(ip?.to_string()))))
        .collect();
    let route = primary.into_iter().chain(secondary).find(|ip| answers(*ip));
    ((route, ips), peers.err())
}

/// Whether a worker is in use; `None` if it could not be reached. An error
/// means that NetBird, which may reach it, could not be asked.
pub(super) fn activity(
    settings: &Value,
    name: &str,
    host_key: &str,
    workloads_only: bool,
) -> Result<Option<bool>, String> {
    let ((preferred, overlays), netbird_error) = lookup(settings, name);
    let mut routes = preferred.into_iter();
    let mut alternate = overlays
        .values()
        .filter_map(Value::as_str)
        .filter_map(|ip| ip.parse::<IpAddr>().ok())
        .filter(|ip| Some(*ip) != preferred);
    let result = routes.find_map(|ip| active(settings, ip, host_key, workloads_only));
    let result = if workloads_only {
        result.or_else(|| alternate.find_map(|ip| active(settings, ip, host_key, workloads_only)))
    } else {
        result
    };
    match (result, netbird_error) {
        (None, Some(error)) => Err(error),
        (result, _) => Ok(result),
    }
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

#[cfg(test)]
mod route_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_failing_netbird_api_is_unknown_not_unreachable() {
        let settings = json!({
            "netbirdApi": "http://127.0.0.1:1", "netbirdTokenFile": "/nonexistent/token",
        });
        assert!(routes(&settings, "adhoc-1").is_err());
        assert!(activity(&settings, "adhoc-1", "key", false).is_err());
        assert!(activity(&settings, "adhoc-1", "key", true).is_err());
    }

    #[test]
    fn unconfigured_netbird_just_has_no_route() {
        let (route, ips) = routes(&json!({}), "adhoc-1").unwrap();
        assert!(route.is_none() && ips.is_empty());
        assert_eq!(activity(&json!({}), "adhoc-1", "key", false), Ok(None));
    }
}

#[cfg(test)]
mod operator_key_tests {
    use super::*;

    #[test]
    fn validates_key_wire_format() {
        assert!(super::valid_extra_public_key(
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEB comment"
        ));
        assert!(!super::valid_extra_public_key("ssh-ed25519 YWJj"));
        assert!(!super::valid_extra_public_key(
            "ssh-rsa AAAAC3NzaC1lZDI1NTE5AAAAIAEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEB"
        ));
        assert!(!super::valid_extra_public_key(
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEB\nssh-rsa YWJj"
        ));
    }

    const PHONE: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEB pschmitt@p11";
    const DESK: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgIC pschmitt@fnuc";

    #[test]
    fn only_the_operators_devices_get_the_operator_keys() {
        assert!(owned_by("pschmitt@zf10", "pschmitt"));
        assert!(!owned_by("someone@zf10", "pschmitt"));
        assert!(!owned_by("pschmitt", "pschmitt"));
        assert!(!owned_by("pschmitt@", "pschmitt"));
        assert!(!owned_by("pschmitt@a@b", "pschmitt"));
        assert!(!owned_by("@host", ""));
    }

    #[test]
    fn operator_keys_are_restricted_and_not_repeated() {
        let text = format!("# workstations\n\n{DESK}\n{DESK}\n{PHONE}\n");
        let keys = restricted_keys(&text, &[PHONE]).unwrap();
        assert_eq!(keys.len(), 1, "{keys:?}");
        assert!(keys[0].starts_with(
            "no-agent-forwarding,no-port-forwarding,no-X11-forwarding,no-user-rc ssh-ed25519 "
        ));
        assert!(
            keys[0]
                .ends_with("AAAAC3NzaC1lZDI1NTE5AAAAIAICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgIC")
        );
        assert!(restricted_keys("not a key\n", &[]).is_err());
        assert!(restricted_keys("", &[]).unwrap().is_empty());
    }
}
