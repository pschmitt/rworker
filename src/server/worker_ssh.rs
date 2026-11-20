use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use serde_json::Value;

use super::{niks3::token_id, openstack::setting};

struct KnownHosts(PathBuf);

impl Drop for KnownHosts {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub(super) fn answers(ip: IpAddr) -> bool {
    TcpStream::connect_timeout(&SocketAddr::new(ip, 22), Duration::from_secs(4)).is_ok()
}

pub(super) fn run(
    settings: &Value,
    ip: IpAddr,
    host_key: &str,
    remote: &str,
    input: Option<&str>,
    timeout_secs: u64,
) -> Result<Output, String> {
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
    writeln!(file, "{ip} {host_key}").map_err(|err| format!("Write worker host key: {err}"))?;
    drop(file);
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
    let netbird = super::netbird::NetBird::from_settings(settings)?;
    let peer = netbird
        .peers()?
        .into_iter()
        .find(|peer| peer["name"] == name);
    let primary: Option<IpAddr> = peer
        .as_ref()
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
    Ok((
        primary.into_iter().chain(secondary).find(|ip| answers(*ip)),
        ips,
    ))
}

pub(super) fn active(settings: &Value, ip: IpAddr, host_key: &str) -> Option<bool> {
    let users = settings["busyUsers"]
        .as_array()?
        .iter()
        .map(Value::as_str)
        .collect::<Option<Vec<_>>>()?
        .join(",");
    let users = crate::proc::quote_words(&[users]);
    let command = format!(
        "printf '%s %s %s\\n' \"$(pgrep -c -u {users} || true)\" \"$(pgrep -c -G nixbld || true)\" \"$(cut -d' ' -f1 /proc/loadavg)\""
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
