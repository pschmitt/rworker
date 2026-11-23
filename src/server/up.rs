use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use super::{
    duration_seconds, epoch, metadata,
    mutations::{self, Compute},
    netbird::NetBird,
    openstack::{OpenStack, setting},
    provision, worker_ssh,
};

#[derive(Default)]
pub(super) struct Args<'a> {
    ttl: Option<u64>,
    profile: Option<&'a str>,
    flavor: Option<&'a str>,
    disk: Option<i64>,
    owner: Option<&'a str>,
    pairs: Vec<String>,
    keys: Vec<&'a str>,
    scratch: bool,
    no_token: bool,
}

impl<'a> Args<'a> {
    pub fn parse(args: &'a [String]) -> Result<Self, String> {
        let mut parsed = Self::default();
        let mut args = args.iter().map(String::as_str);
        while let Some(arg) = args.next() {
            let (flag, inline) = arg
                .split_once('=')
                .map_or((arg, None), |(k, v)| (k, Some(v)));
            match flag {
                "--scratch" if inline.is_none() => {
                    parsed.scratch = true;
                    continue;
                }
                "--no-niks3-token" if inline.is_none() => {
                    parsed.no_token = true;
                    continue;
                }
                "--ttl" | "--profile" | "--flavor" | "--disk" | "--owner" | "--meta"
                | "--ssh-key" => {}
                _ => return Err(format!("unrecognized arguments: {arg}")),
            }
            let value = inline
                .or_else(|| args.next().filter(|arg| !arg.starts_with("--")))
                .ok_or_else(|| format!("argument {flag}: expected one argument"))?;
            match flag {
                "--ttl" => {
                    parsed.ttl = Some(
                        duration_seconds(value)
                            .ok_or_else(|| format!("invalid duration: {value}"))?,
                    )
                }
                "--profile" => parsed.profile = Some(value),
                "--flavor" => parsed.flavor = Some(value),
                "--disk" => {
                    parsed.disk = Some(
                        value
                            .parse()
                            .map_err(|_| format!("invalid int value: '{value}'"))?,
                    )
                }
                "--owner" => parsed.owner = Some(value),
                "--meta" => parsed.pairs.push(value.into()),
                "--ssh-key" if !value.trim().is_empty() => parsed.keys.push(value.trim()),
                _ => {}
            }
        }
        Ok(parsed)
    }
}

pub(super) fn up(cfg: &Value, args: Args<'_>) -> Result<Value, String> {
    let compute = OpenStack::from_settings(cfg)?;
    let netbird = NetBird::from_settings(cfg)?;
    let profile = args
        .profile
        .filter(|value| !value.is_empty())
        .or_else(|| cfg["defaultProfile"].as_str())
        .unwrap_or("standard");
    let options = &cfg["profiles"][profile];
    if !options
        .as_object()
        .is_some_and(|profile| !profile.is_empty())
    {
        return Err(format!("unknown worker profile: {profile}"));
    }
    let flavor = args
        .flavor
        .filter(|value| !value.is_empty())
        .or_else(|| options["flavor"].as_str().filter(|value| !value.is_empty()))
        .unwrap_or(setting(cfg, "adhocFlavor")?);
    let disk = args
        .disk
        .filter(|&value| value != 0)
        .or_else(|| options["disk"].as_i64().filter(|&value| value != 0))
        .or_else(|| cfg["adhocVolumeSize"].as_i64())
        .ok_or("Missing setting: adhocVolumeSize")?;
    let lifetime_key = if args.scratch {
        "adhocScratchMax"
    } else {
        "adhocMaxTtl"
    };
    let lifetime = cfg[lifetime_key]
        .as_u64()
        .ok_or_else(|| format!("Missing setting: {lifetime_key}"))?;
    let ttl = args
        .ttl
        .or_else(|| cfg["adhocDefaultTtl"].as_u64())
        .ok_or("Missing setting: adhocDefaultTtl")?
        .min(lifetime);
    let prefix = setting(cfg, "adhocPrefix")?;
    let output = compute.command(&["server", "list", "-f", "json", "-c", "Name"])?;
    let servers: Vec<Value> =
        serde_json::from_slice(&output).map_err(|_| "Invalid OpenStack server listing")?;
    let count = servers
        .iter()
        .filter_map(|server| server["Name"].as_str())
        .filter(|name| name.starts_with(prefix))
        .collect::<std::collections::HashSet<_>>()
        .len();
    let max = cfg["adhocMax"]
        .as_u64()
        .ok_or("Missing setting: adhocMax")?;
    if count as u64 >= max {
        return Err(format!(
            "already {count} ad-hoc workers (max {max}); free one with down"
        ));
    }
    if args.keys.is_empty() {
        return Err("--ssh-key is required (the agent user's key)".into());
    }
    let suffix = super::niks3::token_id().map_err(|err| err.to_string())?;
    let name = format!("{prefix}{}", &suffix[..8]);
    let expires = (epoch() + ttl as f64) as i64;
    let owner = args.owner.unwrap_or_default();
    let mut metadata: Map<String, Value> = [
        ("nixbot-worker", "adhoc".into()),
        ("expires_at", expires.to_string()),
        ("created_at", (epoch() as i64).to_string()),
        ("max_lifetime", lifetime.to_string()),
        ("owner", owner.into()),
        ("worker_profile", profile.into()),
    ]
    .into_iter()
    .map(|(key, value)| (key.into(), Value::String(value)))
    .collect();
    metadata.extend(metadata::creator_metadata(&args.pairs)?);
    let web_key = super::worker_ssh::web_worker_key(owner, &args.keys)?;
    let keys: Vec<_> = args.keys.into_iter().chain(web_key.as_deref()).collect();
    let mut user_data = Map::from_iter([("agent_authorized_keys".into(), json!(keys))]);
    if let Some(path) = cfg["logCollectorPublicKeyFile"].as_str()
        && let Some((owner, _)) = owner.split_once('@')
        && cfg["logCollectorOperator"].as_str() == Some(owner)
    {
        let key = std::fs::read_to_string(path)
            .map_err(|err| format!("could not read the log collector public key: {err}"))?;
        let mut words = key.split_whitespace();
        let blob = words
            .next()
            .filter(|kind| *kind == "ssh-ed25519")
            .and_then(|_| words.next())
            .filter(|blob| {
                blob.len() == 68
                    && blob
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(&b))
            })
            .ok_or("invalid Ed25519 collector public key")?;
        user_data.insert("log_collector_key".into(), json!(blob));
    }
    let archive_logs = user_data.contains_key("log_collector_key");
    let (id, mut metadata) = provision::create(
        cfg,
        &compute,
        provision::Request {
            name: &name,
            group: setting(cfg, "netbirdAdhocGroup")?,
            flavor,
            disk,
            profile,
            metadata,
            user_data,
        },
    )?;
    super::inventory_cache::invalidate_current();
    let result = (|| -> Result<Option<Value>, String> {
        let timeout = cfg["bootTimeout"]
            .as_u64()
            .ok_or("Missing setting: bootTimeout")?;
        let Some((host_key, ip, ips)) = ready(cfg, &compute, &id, &name, timeout)? else {
            return Ok(None);
        };
        let mut compute = compute;
        finish_boot(
            cfg,
            &mut compute,
            &netbird,
            &id,
            &name,
            &mut metadata,
            &host_key,
        )?;
        let mut response = json!({
            "name": name, "ip": ip, "ips": ips, "host_key": host_key, "user": "agent",
            "expires_at": expires, "flavor": flavor, "profile": profile,
            "builder": format!("ssh-ng://agent@{ip}"),
        });
        if archive_logs {
            response["archive_logs"] = json!(true);
            response["created_at"] = metadata.get("created_at").cloned().unwrap_or(Value::Null);
            response["topic"] = metadata.get("topic").cloned().unwrap_or(Value::Null);
            response["agent"] = metadata.get("agent").cloned().unwrap_or(Value::Null);
        }
        if !args.no_token
            && cfg["niks3"].is_object()
            && options["niks3Token"].as_bool().unwrap_or(true)
        {
            response["niks3_token"] =
                super::niks3::install(cfg, &mut compute, &id, &name, &metadata, expires)?;
        }
        Ok(Some(response))
    })();
    match result {
        Ok(Some(response)) => Ok(response),
        result => {
            let compute = OpenStack::from_settings(cfg)?;
            if matches!(result, Ok(None)) {
                eprintln!("{}", boot_diagnostics(cfg, &compute, &id, &name));
            }
            let why = if matches!(result, Ok(None)) {
                "did not come up"
            } else {
                "creation failed"
            };
            mutations::delete(cfg, &compute, &netbird, &id, &name, &metadata, ("up", why))?;
            result?.ok_or_else(|| format!("{name} did not come up; deleted it"))
        }
    }
}

pub(super) type Ready = (String, std::net::IpAddr, Map<String, Value>);

/// Record a booted worker's host key and drop its one-time enrollment keys.
/// `up` does this once the worker is ready; the reaper repeats it for a
/// worker whose `up` was cut short (for example by a controller restart).
pub(super) fn finish_boot(
    cfg: &Value,
    compute: &mut OpenStack,
    netbird: &super::netbird::NetBird,
    id: &str,
    name: &str,
    metadata: &mut Map<String, Value>,
    host_key: &str,
) -> Result<(), String> {
    let id = id.to_owned();
    if let Some(key) = metadata.get("setup_key_id").and_then(Value::as_str) {
        netbird.delete_key(key);
    }
    compute.set_metadata(
        &id,
        [("host_key".into(), json!(host_key))].into_iter().collect(),
    )?;
    metadata.insert("host_key".into(), json!(host_key));
    if let Err(error) = compute.delete_metadata(&id, &["setup_key_id"]) {
        eprintln!("removing setup_key_id from {name}: {error}");
    }
    if let Some(key) = metadata.get("ts_key_id").and_then(Value::as_str) {
        match super::tailscale::Tailscale::from_settings(&cfg["tailscale"]) {
            Ok(mut ts) => {
                ts.delete_key(key);
                if let Err(error) = compute.delete_metadata(&id, &["ts_key_id"]) {
                    eprintln!("releasing the Tailscale key of {name}: {error}");
                }
            }
            Err(error) => eprintln!("releasing the Tailscale key of {name}: {error}"),
        }
    }
    Ok(())
}

/// Lines a console may show that must not reach logs: tokens, keys and
/// passwords from cloud-init or services. The worker's public host key
/// line is fine.
fn secret_line(line: &str) -> bool {
    if line.contains("NIXBOT-WORKER-HOSTKEY") {
        return false;
    }
    let lower = line.to_ascii_lowercase();
    [
        "token",
        "secret",
        "password",
        "passwd",
        "setup_key",
        "setup-key",
        "setupkey",
        "authkey",
        "auth_key",
        "auth-key",
        "private key",
        "-----begin",
        "key=",
    ]
    .iter()
    .any(|word| lower.contains(word))
}

/// Why a worker never became ready, logged before it is deleted: the
/// server's state and fault, the readiness step it stopped at (ACTIVE?
/// host key on the console? overlay address answering SSH?) and the
/// filtered console tail.
pub(super) fn boot_diagnostics(cfg: &Value, compute: &OpenStack, id: &str, name: &str) -> String {
    let mut out = vec![format!("boot diagnostics for {name} ({id}):")];
    match compute.command(&[
        "server",
        "show",
        "-f",
        "json",
        "-c",
        "status",
        "-c",
        "OS-EXT-STS:task_state",
        "-c",
        "fault",
        "--",
        id,
    ]) {
        Ok(output) => {
            let detail: Value = serde_json::from_slice(&output).unwrap_or_default();
            let fault = detail["fault"]["message"]
                .as_str()
                .or_else(|| detail["fault"].as_str())
                .unwrap_or("-");
            out.push(format!(
                "  server: status={} task_state={} fault={fault}",
                detail["status"].as_str().unwrap_or("?"),
                detail["OS-EXT-STS:task_state"].as_str().unwrap_or("-"),
            ));
        }
        Err(error) => out.push(format!("  server: {error}")),
    }
    match boot_status(cfg, compute, id, name) {
        Ok((stage, _)) => out.push(format!("  readiness stopped at: {stage}")),
        Err(error) => out.push(format!("  readiness: {error}")),
    }
    match worker_ssh::routes(cfg, name) {
        Ok((reachable, ips)) => {
            if ips.is_empty() {
                out.push("  overlay: no NetBird peer or Tailscale device with this name".into());
            }
            for (overlay, ip) in &ips {
                let ip = ip.as_str().unwrap_or_default();
                let answers = ip
                    .parse()
                    .is_ok_and(|ip| reachable == Some(ip) || worker_ssh::answers(ip));
                out.push(format!(
                    "  overlay {overlay}: {ip} ssh {}",
                    if answers {
                        "answers"
                    } else {
                        "does not answer"
                    }
                ));
            }
        }
        Err(error) => out.push(format!("  overlay: {error}")),
    }
    match compute.command(&["console", "log", "show", "--lines", "40", "--", id]) {
        Ok(output) => {
            out.push("  console (last 40 lines, secrets filtered):".into());
            let text = String::from_utf8_lossy(&output);
            for line in text.lines() {
                if secret_line(line) {
                    out.push("    [filtered]".into());
                } else {
                    out.push(format!("    {}", line.trim_end()));
                }
            }
        }
        Err(error) => out.push(format!("  console: {error}")),
    }
    out.join("\n")
}

fn ready(
    cfg: &Value,
    compute: &OpenStack,
    id: &str,
    name: &str,
    timeout: u64,
) -> Result<Option<Ready>, String> {
    let now = Instant::now();
    let mut previous = String::new();
    while now.elapsed() < Duration::from_secs(timeout) {
        let (stage, result) = boot_status(cfg, compute, id, name)?;
        if stage != previous {
            eprintln!("rworker-progress {stage}");
            previous = stage;
        }
        match result {
            Boot::Waiting => {}
            Boot::Failed => return Ok(None),
            Boot::Ready(worker) => return Ok(Some(worker)),
        }
        std::thread::sleep(Duration::from_secs(5));
    }
    Ok(None)
}

pub(super) enum Boot {
    Waiting,
    Failed,
    Ready(Ready),
}

pub(super) fn boot_status(
    cfg: &Value,
    compute: &OpenStack,
    id: &str,
    name: &str,
) -> Result<(String, Boot), String> {
    let output = compute.command(&["server", "show", "-f", "json", "-c", "status", "--", id])?;
    let output: Value =
        serde_json::from_slice(&output).map_err(|_| "Invalid OpenStack server status")?;
    let status = setting(&output, "status")?;
    let mut stage = if status == "BUILD" {
        "BUILDING"
    } else {
        status
    };
    let mut ready = None;
    if status == "ACTIVE" {
        let output = compute.command(&["console", "log", "show", "--lines", "400", "--", id])?;
        let output = String::from_utf8_lossy(&output);
        let key = output
            .lines()
            .rev()
            .find_map(|line| line.trim().strip_prefix("NIXBOT-WORKER-HOSTKEY "))
            .map(str::trim)
            .filter(|key| !key.is_empty());
        stage = "ACTIVE · booting";
        if let Some(key) = key {
            stage = "ACTIVE · joining overlay";
            let (ip, ips) = worker_ssh::routes(cfg, name)?;
            if let Some(ip) = ip {
                stage = "ACTIVE · ready";
                ready = Some((key.to_owned(), ip, ips));
            }
        }
    }

    let result = match (status, ready) {
        ("ERROR", _) => Boot::Failed,
        (_, Some(worker)) => Boot::Ready(worker),
        _ => Boot::Waiting,
    };
    Ok((stage.to_owned(), result))
}

#[cfg(test)]
mod boot_diagnostics_tests {
    use super::secret_line;

    #[test]
    fn console_secrets_are_filtered_but_the_host_key_stays() {
        assert!(!secret_line("NIXBOT-WORKER-HOSTKEY ssh-ed25519 AAAA"));
        assert!(secret_line("netbird_setup_key: ABC"));
        assert!(secret_line("Using token=deadbeef"));
        assert!(secret_line("-----BEGIN OPENSSH PRIVATE KEY-----"));
        assert!(!secret_line("[  OK  ] Started NetBird."));
    }
}
