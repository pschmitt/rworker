//! The control host connection, the `ls` cache and what this host knows
//! about each worker: its state file and its pinned SSH host key.

use crate::ctx::{Ctx, Fail, R};
use crate::jq;
use crate::paint;
use crate::proc;
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::net::{TcpStream, ToSocketAddrs};
use std::process::Command;
use std::time::Duration;

/// `ssh CONTROL rworker ARGS...` over one shared connection (kept 10 min):
/// repeated calls (ls --watch, the completions' refresh) skip the handshake.
pub fn control_cmd<S: AsRef<str>>(ctx: &Ctx, args: &[S]) -> Command {
    if args.first().is_some_and(|action| {
        matches!(
            action.as_ref(),
            "up" | "down" | "extend" | "claim" | "topic" | "niks3-token"
        )
    }) {
        ctx.resolution_snapshot.borrow_mut().take();
        let _ = fs::remove_file(discovery_cache(ctx));
    }
    if let Some(path) = std::env::var_os("RWORKER_CONTROL_SOCKET").filter(|p| !p.is_empty()) {
        let exe = std::env::current_exe().unwrap_or_else(|_| "rworker".into());
        let mut cmd = Command::new(exe);
        cmd.arg("__control-socket")
            .arg(path)
            .args(args.iter().map(AsRef::as_ref));
        return cmd;
    }
    control_ssh_cmd(ctx, args)
}

pub fn control_ssh_cmd<S: AsRef<str>>(ctx: &Ctx, args: &[S]) -> Command {
    let _ = fs::create_dir_all(&ctx.state_dir);
    let mut cmd = Command::new("ssh");
    cmd.args(["-q", "-o", "BatchMode=yes"]);
    if args.first().is_some_and(|action| {
        action.as_ref().starts_with("__cached-")
            || matches!(
                action.as_ref(),
                "github-up"
                    | "github-status"
                    | "github-down"
                    | "github-mesh-setup"
                    | "github-mesh-status"
                    | "gitlab-up"
                    | "gitlab-status"
                    | "gitlab-down"
                    | "gitlab-mesh-setup"
                    | "gitlab-mesh-status"
            )
    }) {
        cmd.args([
            "-o",
            "ConnectTimeout=5",
            "-o",
            "ServerAliveInterval=5",
            "-o",
            "ServerAliveCountMax=1",
        ]);
    }
    if ctx.control_master {
        cmd.args(["-o", "ControlMaster=auto", "-o", "ControlPersist=10m"]);
        cmd.arg("-o").arg(format!(
            "ControlPath={}/control-%C",
            ctx.state_dir.display()
        ));
    } else {
        cmd.args(["-o", "ControlMaster=no", "-o", "ControlPath=none"]);
    }
    cmd.arg(&ctx.control_host);
    cmd.arg(format!("rworker {}", proc::quote_words(args)));
    cmd
}

/// `out=$(remote ARGS) || return 1`.
pub fn remote<S: AsRef<str>>(ctx: &Ctx, args: &[S]) -> R<String> {
    proc::capture_ok(ctx, &mut control_cmd(ctx, args))
}

/// `remote ARGS > /dev/null`, its status.
pub fn remote_quiet<S: AsRef<str>>(ctx: &Ctx, args: &[S]) -> R<i32> {
    proc::run_quiet_stdout(ctx, &mut control_cmd(ctx, args))
}

/// `remote ls`, keeping the answer in the cache for the completions.
pub fn remote_ls(ctx: &Ctx) -> R<String> {
    let out = remote(ctx, &["ls"])?;
    cache_inventory(ctx, &ctx.ls_cache(), &out);
    Ok(out)
}

pub(crate) fn cache_inventory(ctx: &Ctx, path: &std::path::Path, inventory: &str) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    if fs::create_dir_all(&ctx.state_dir).is_ok() {
        let tmp = path.with_extension(format!(
            "{}.{}.tmp",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        if fs::write(&tmp, format!("{inventory}\n")).is_ok() && fs::rename(&tmp, path).is_ok() {
            return;
        }
        let _ = fs::remove_file(tmp);
    }
}

fn discovery_cache(ctx: &Ctx) -> std::path::PathBuf {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut key = DefaultHasher::new();
    ctx.control_host.hash(&mut key);
    std::env::var_os("RWORKER_CONTROL_SOCKET").hash(&mut key);
    ctx.state_dir
        .join(format!(".discovery-{:016x}.json", key.finish()))
}

/// Discovery loops can share the latest successful inventory with each other.
pub fn poll_ls(ctx: &Ctx) -> R<String> {
    let path = discovery_cache(ctx);
    let cached = (|| {
        let mut file = fs::File::open(&path).ok()?;
        let age = file.metadata().ok()?.modified().ok()?.elapsed().ok()?;
        if age >= Duration::from_secs(2) {
            return None;
        }
        let mut out = String::new();
        std::io::Read::read_to_string(&mut file, &mut out).ok()?;
        serde_json::from_str::<Value>(&out)
            .ok()?
            .is_array()
            .then_some(out)
    })();
    if let Some(cached) = cached {
        return Ok(cached);
    }
    let fresh = remote_ls(ctx)?;
    cache_inventory(ctx, &path, &fresh);
    Ok(fresh)
}

/// Parse JSON the client got from somewhere; a parse error fails like jq.
pub fn parse(ctx: &Ctx, text: &str) -> R<Value> {
    serde_json::from_str(text).map_err(|e| {
        ctx.err(&format!("jq: error (at <stdin>:0): {e}"));
        Fail(1)
    })
}

/// `.[]` of a parsed answer, failing like jq on a scalar.
pub fn items<'a>(ctx: &Ctx, v: &'a Value) -> R<Vec<&'a Value>> {
    jq::items(v).ok_or_else(|| {
        ctx.err(&format!(
            "jq: error (at <stdin>:0): Cannot iterate over {}",
            jq::raw(v)
        ));
        Fail(5)
    })
}

/// `.[] | select(.name == $n)`: the first match.
pub fn find<'a>(ctx: &Ctx, list: &'a Value, name: &str) -> R<Option<&'a Value>> {
    Ok(items(ctx, list)?
        .into_iter()
        .find(|w| jq::get(w, "name").as_str() == Some(name)))
}

fn append_known_host(ctx: &Ctx, line: &str) -> R {
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(ctx.known_hosts())
        .map_err(|e| {
            ctx.err(&format!("{}: {}: {e}", ctx.me, ctx.known_hosts().display()));
            Fail(1)
        })?;
    writeln!(f, "{line}").map_err(|_| Fail(1))
}

fn write_state(ctx: &Ctx, name: &str, v: &Value) -> R {
    fs::write(ctx.state_file(name), format!("{}\n", jq::compact(v))).map_err(|e| {
        ctx.err(&format!(
            "{}: {}: {e}",
            ctx.me,
            ctx.state_file(name).display()
        ));
        Fail(1)
    })
}

/// Remember a worker `up` just created.
pub fn remember_created(ctx: &Ctx, name: &str, info: &Value) -> R {
    let state = jq::merge(info, &[("user", json!("agent")), ("kind", json!("adhoc"))]);
    write_state(ctx, name, &state)?;
    let ip = jq::raw(jq::get(info, "ip"));
    let key = jq::raw(jq::get(info, "host_key"));
    append_known_host(ctx, &format!("{ip} {key}"))
}

pub fn remember_hosted(ctx: &Ctx, name: &str, info: &Value) -> R {
    fs::create_dir_all(&ctx.state_dir).map_err(|_| Fail(1))?;
    write_state(ctx, name, info)
}

pub fn hosted_id(name: &str) -> Option<(crate::github_profiles::Forge, &str)> {
    let (forge, id) = crate::github_profiles::Forge::ALL
        .into_iter()
        .find_map(|forge| name.strip_prefix(forge.prefix()).map(|id| (forge, id)))?;
    crate::github_profiles::valid_worker_suffix(id).then_some((forge, id))
}

/// Fetch a worker this host did not create (another host's, or nixbot's
/// pool) from the control host, pin its host key and remember it.
pub fn adopt(ctx: &Ctx, name: &str) -> R {
    let snapshot = ctx.resolution_snapshot.borrow_mut().take();
    let out = snapshot.map_or_else(|| remote_ls(ctx), Ok)?;
    let list = parse(ctx, &out)?;
    let Some(info) = find(ctx, &list, name)? else {
        ctx.err(&format!("No worker named {}", paint::vm(ctx, name)));
        return Err(Fail(1));
    };
    if jq::or_empty(jq::get(info, "ip")).is_empty()
        || jq::or_empty(jq::get(info, "host_key")).is_empty()
    {
        paint::status_msg(
            ctx,
            &format!(
                "{} has no IP or host key yet (still booting?)",
                paint::vm(ctx, name)
            ),
        );
        return Err(Fail(1));
    }
    if hosted_id(name).is_some() {
        return remember_hosted(ctx, name, info);
    }
    // On ad-hoc workers agent's keys are the creator's, so other owners log
    // in as themselves. Pool workers carry the operator's keys for agent in
    // their image: log in as agent there, as the web console does, so both
    // see the same jobs.
    let kind = jq::get(info, "kind").as_str();
    let pool = !matches!(kind, None | Some("adhoc" | "unknown"));
    let mut user = ctx.user.clone();
    if pool || (kind == Some("adhoc") && jq::raw(jq::get(info, "owner")) == ctx.owner()) {
        user = "agent".to_string();
    }
    fs::create_dir_all(&ctx.state_dir).map_err(|_| Fail(1))?;
    let state = json!({
        "name": jq::get(info, "name"),
        "ip": jq::get(info, "ip"),
        "ips": jq::get(info, "ips"),
        "host_key": jq::get(info, "host_key"),
        "user": user,
        "kind": jq::get(info, "kind"),
    });
    write_state(ctx, name, &state)?;
    append_known_host(
        ctx,
        &format!(
            "{} {}",
            jq::raw(jq::get(info, "ip")),
            jq::raw(jq::get(info, "host_key"))
        ),
    )
}

fn read_state(ctx: &Ctx, name: &str) -> R<Value> {
    let path = ctx.state_file(name);
    let text = fs::read_to_string(&path).map_err(|e| {
        ctx.err(&format!(
            "jq: error: Could not open {}: {e}",
            path.display()
        ));
        Fail(2)
    })?;
    parse(ctx, &text).map_err(|_| Fail(2))
}

/// One field of a worker's state file, adopting the worker first when this
/// host does not know it. A null field fails (`jq -e`).
pub fn worker_field(ctx: &Ctx, name: &str, field: &str) -> R<String> {
    if name.is_empty() {
        ctx.err("Missing worker name");
        return Err(Fail(2));
    }
    if !ctx.state_file(name).is_file() {
        adopt(ctx, name)?;
    }
    let state = read_state(ctx, name)?;
    let v = jq::get(&state, field);
    if !jq::truthy(v) {
        return Err(Fail(1));
    }
    Ok(jq::raw(v))
}

fn probe_timeout() -> Duration {
    let raw = crate::ctx::env_or("RWORKER_PROBE_TIMEOUT", "4");
    let (num, mult) = match raw.chars().last() {
        Some('s') => (&raw[..raw.len() - 1], 1.0),
        Some('m') => (&raw[..raw.len() - 1], 60.0),
        Some('h') => (&raw[..raw.len() - 1], 3600.0),
        Some('d') => (&raw[..raw.len() - 1], 86400.0),
        _ => (raw.as_str(), 1.0),
    };
    Duration::from_secs_f64(num.parse::<f64>().unwrap_or(4.0).max(0.0) * mult)
}

/// Does anything answer on port 22 within the probe timeout?
fn ssh_port_open(ip: &str, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    let Ok(addrs) = (ip, 22).to_socket_addrs() else {
        return false;
    };
    for addr in addrs {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return false;
        }
        if TcpStream::connect_timeout(&addr, left).is_ok() {
            return true;
        }
    }
    false
}

/// The address to reach a worker at. Workers join NetBird and Tailscale;
/// one overlay can be down while the other works, so probe SSH on each
/// known address (the recorded one first) and use the first that answers.
/// Falls back to the recorded address, so ssh reports its own error.
pub fn worker_ip(ctx: &Ctx, name: &str) -> R<String> {
    worker_field(ctx, name, "ip").map_err(|_| Fail(1))?;
    let mut state = read_state(ctx, name).map_err(|_| Fail(1))?;
    if hosted_id(name).is_some() {
        return Ok(name.to_owned());
    }
    // A state file from before the overlays has only the recorded address:
    // ask the control host once.
    if !jq::truthy(jq::get(&state, "ips")) {
        let ips = ctx.muted(|| -> Option<Value> {
            let out = remote_ls(ctx).ok()?;
            let list: Value = serde_json::from_str(&out).ok()?;
            let w = jq::items(&list)?
                .into_iter()
                .find(|w| jq::get(w, "name").as_str() == Some(name))?;
            let ips = jq::get(w, "ips");
            jq::truthy(ips).then(|| ips.clone())
        });
        if let Some(ips) = ips {
            state = jq::merge(&state, &[("ips", ips)]);
            let _ = write_state(ctx, name, &state);
        }
    }
    let mut candidates: Vec<String> = Vec::new();
    for v in [
        jq::get(&state, "ip"),
        jq::alt(jq::path(&state, &["ips", "netbird"]), &jq::NULL),
        jq::alt(jq::path(&state, &["ips", "tailscale"]), &jq::NULL),
    ] {
        if v.is_null() || v.as_str() == Some("") || v == &Value::Bool(false) {
            continue;
        }
        let s = jq::raw(v);
        if !candidates.contains(&s) {
            candidates.push(s);
        }
    }
    if candidates.is_empty() {
        return Err(Fail(1));
    }
    if candidates.len() > 1 {
        let timeout = probe_timeout();
        for ip in &candidates {
            if ssh_port_open(ip, timeout) {
                // Remember the address that works: the next call tries it
                // first and does not wait for the dead overlay again.
                if ip != &candidates[0] {
                    let updated = jq::merge(&state, &[("ip", json!(ip))]);
                    let _ = write_state(ctx, name, &updated);
                }
                return Ok(ip.clone());
            }
        }
    }
    Ok(candidates[0].clone())
}

/// SSH options for a worker. Host keys are pinned per worker *name*
/// (HostKeyAlias): NetBird reuses IPs, and a stale entry must fail loudly
/// rather than reach whichever worker holds that IP now. A failure leaves
/// no options if the host key is unavailable.
pub fn ssh_opts(ctx: &Ctx, name: &str) -> Vec<String> {
    ssh_opts_with_relay(ctx, name, local_relay())
}

/// Commands the web console runs on the control host reach hosted workers
/// through the local relay port, as its terminal does, instead of an SSH
/// proxy back into the control host.
pub fn local_relay() -> bool {
    std::env::var_os("RWORKER_LOCAL_RELAY").is_some_and(|value| !value.is_empty())
}

pub fn hosted_proxy_command(ctx: &Ctx, port: u64) -> String {
    let target = format!("127.0.0.1:{port}");
    proc::quote_words(&[
        "ssh",
        "-q",
        "-o",
        "BatchMode=yes",
        "-o",
        "StrictHostKeyChecking=yes",
        "-W",
        &target,
        "--",
        &ctx.control_host,
    ])
    .replace('%', "%%")
    .trim_end()
    .to_owned()
}

pub fn ssh_opts_with_relay(ctx: &Ctx, name: &str, local_relay: bool) -> Vec<String> {
    let Ok(key) = worker_field(ctx, name, "host_key") else {
        return Vec::new();
    };
    let line = format!("{name} {key}");
    let known = fs::read_to_string(ctx.known_hosts()).unwrap_or_default();
    if !known.lines().any(|l| l == line) {
        let _ = append_known_host(ctx, &line);
    }
    let mut options = vec![
        "-q".to_string(),
        "-o".to_string(),
        format!("UserKnownHostsFile={}", ctx.known_hosts().display()),
        "-o".to_string(),
        format!("HostKeyAlias={name}"),
        "-o".to_string(),
        "StrictHostKeyChecking=yes".to_string(),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        // A build can keep a session silent for minutes. Without probes a NAT
        // or mesh path (a phone on Wi-Fi) forgets the connection; with them it
        // stays alive and a blip of up to two minutes does not end it.
        "-o".to_string(),
        "ServerAliveInterval=15".to_string(),
        "-o".to_string(),
        "ServerAliveCountMax=8".to_string(),
    ];
    if hosted_id(name).is_some() {
        let port = read_state(ctx, name)
            .ok()
            .and_then(|state| state["github_port"].as_u64())
            .filter(|port| (1024..=65535).contains(port));
        if local_relay {
            let Some(port) = port else {
                return Vec::new();
            };
            options.extend([
                "-o".into(),
                "Hostname=127.0.0.1".into(),
                "-o".into(),
                "ProxyCommand=none".into(),
                "-p".into(),
                port.to_string(),
            ]);
        } else if let Some(port) = port {
            let proxy = hosted_proxy_command(ctx, port);
            options.extend(["-o".into(), format!("ProxyCommand={proxy}")]);
        }
    }
    if let Some(identity) =
        std::env::var_os("RWORKER_IDENTITY_FILE").filter(|path| !path.is_empty())
    {
        options.push("-i".into());
        options.push(identity.to_string_lossy().into_owned());
        options.push("-o".into());
        options.push("IdentitiesOnly=yes".into());
        options.push("-o".into());
        options.push("ConnectTimeout=5".into());
    }
    options
}

/// Drop a deleted worker: from the completions' cache right away, and its
/// state file and IP host key.
pub fn forget(ctx: &Ctx, name: &str) {
    let cache = ctx.ls_cache();
    let writable = fs::metadata(&cache)
        .map(|m| !m.permissions().readonly())
        .unwrap_or(false);
    if writable
        && let Ok(text) = fs::read_to_string(&cache)
        && let Ok(Value::Array(list)) = serde_json::from_str::<Value>(&text)
    {
        let kept: Vec<Value> = list
            .into_iter()
            .filter(|w| jq::get(w, "name").as_str() != Some(name))
            .collect();
        let tmp = ctx
            .state_dir
            .join(format!(".ls.json.{}.tmp", std::process::id()));
        if fs::write(&tmp, format!("{}\n", jq::compact(&Value::Array(kept)))).is_ok() {
            let _ = fs::rename(&tmp, &cache);
        }
    }
    let Ok(ip) = ctx.muted(|| worker_field(ctx, name, "ip")) else {
        return;
    };
    if ctx.known_hosts().is_file() {
        let _ = Command::new("ssh-keygen")
            .arg("-R")
            .arg(&ip)
            .arg("-f")
            .arg(ctx.known_hosts())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    let _ = fs::remove_file(ctx.state_file(name));
}

/// `ssh OPTS USER@IP ARGS...` for a worker.
pub fn worker_ssh(opts: &[String], login: &str) -> Command {
    let mut cmd = Command::new("ssh");
    cmd.args(opts).arg(login);
    cmd
}

/// rsync's `-e`: the ssh command line with the pinned options.
pub fn rsync_e(opts: &[String]) -> String {
    let mut s = "ssh".to_string();
    // "ssh ${opts[*]}": a space even when there are no options.
    s.push(' ');
    let args: Vec<_> = opts
        .iter()
        .map(|arg| {
            if arg.contains(char::is_whitespace) || arg.contains(['\'', '"']) {
                format!("\"{}\"", arg.replace('"', "\"\""))
            } else {
                arg.clone()
            }
        })
        .collect();
    s.push_str(&args.join(" "));
    s
}
