use std::process::{Command, Stdio};

const DEMAND_SQL: &str = "
WITH demand AS (
  SELECT bool_or(a.status = 'building') AS building
  FROM build_attributes a
  JOIN builds b ON b.id = a.build_id
  WHERE b.status IN ('evaluating', 'building')
    AND coalesce(a.system, 'x86_64-linux') = 'x86_64-linux'
    AND a.status IN ('pending', 'building')
  GROUP BY a.drv_path, CASE WHEN a.drv_path IS NULL THEN a.id END
)
SELECT count(*) FILTER (WHERE NOT building),
       count(*) FILTER (WHERE building)
FROM demand
";

pub(super) struct Demand {
    pub pending: u64,
    pub building: u64,
}

impl Demand {
    pub fn read(database_url: &str) -> Result<Self, String> {
        // libpq treats PGDATABASE as a literal name. Parse the local
        // socket URI without putting a credential-bearing URL in argv.
        let (database, socket) = database_url
            .strip_prefix("postgresql:///")
            .and_then(|url| url.split_once("?host="))
            .filter(|(database, socket)| {
                !database.is_empty()
                    && database
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                    && socket.starts_with('/')
                    && socket
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b))
            })
            .ok_or("Expected a PostgreSQL local socket URI (postgresql:///DATABASE?host=/PATH)")?;
        let output = Command::new("psql")
            .args([
                "--no-psqlrc",
                "--no-password",
                "--no-align",
                "--tuples-only",
                "--set=ON_ERROR_STOP=1",
                "--command",
                DEMAND_SQL,
            ])
            .env("PGDATABASE", database)
            .env("PGHOST", socket)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map_err(|error| format!("Read nixbot demand: {error}"))?;
        if !output.status.success() {
            return Err(format!("Read nixbot demand: psql {}", output.status));
        }
        let data =
            std::str::from_utf8(&output.stdout).map_err(|_| "Invalid nixbot demand response")?;
        let (pending, building) = data
            .trim()
            .split_once('|')
            .ok_or("Invalid nixbot demand response")?;
        Ok(Self {
            pending: pending.parse().map_err(|_| "Invalid pending build count")?,
            building: building
                .parse()
                .map_err(|_| "Invalid running build count")?,
        })
    }

    pub fn queued(&self, slots: u64) -> u64 {
        self.pending + self.building.saturating_sub(slots)
    }

    /// Demand of the configured source: nixbot's database (the default) or
    /// a Circus queue-runner (`demand.source = "circus"`).
    pub fn from_settings(cfg: &Value) -> Result<Self, String> {
        match cfg["demand"]["source"].as_str() {
            None | Some("nixbot") => Self::read(setting(cfg, "databaseUrl")?),
            Some("circus") => Self::read_circus(&cfg["demand"]),
            Some(other) => Err(format!("Unknown demand source: {other}")),
        }
    }

    /// Runnable builds Circus has not dispatched yet, from its operator
    /// queue. The API key goes to curl on stdin, never in argv.
    fn read_circus(demand: &Value) -> Result<Self, String> {
        let url = format!(
            "{}/api/v1/operator/queue",
            setting(demand, "url")?.trim_end_matches('/')
        );
        let key = fs::read_to_string(setting(demand, "apiKeyFile")?)
            .map_err(|error| format!("Read Circus API key: {error}"))?;
        let mut child = Command::new("curl")
            .args([
                "--fail",
                "--silent",
                "--show-error",
                "--max-time",
                "20",
                "--header",
                "@-",
                &url,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("Read Circus demand: {error}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            writeln!(stdin, "Authorization: Bearer {}", key.trim())
                .map_err(|error| format!("Read Circus demand: {error}"))?;
        }
        let output = child
            .wait_with_output()
            .map_err(|error| format!("Read Circus demand: {error}"))?;
        if !output.status.success() {
            return Err(format!("Read Circus demand: curl {}", output.status));
        }
        parse_circus_queue(
            &output.stdout,
            demand["system"].as_str().unwrap_or("x86_64-linux"),
            demand["qemuOverflow"].as_bool().unwrap_or(false),
        )
    }
}

/// `GET /api/v1/operator/queue` answers `[{"system": ..., "count": ...}]`:
/// pending builds per system, which is all the pool needs.
pub(super) fn parse_circus_queue(
    body: &[u8],
    system: &str,
    qemu_overflow: bool,
) -> Result<Demand, String> {
    let queue: Vec<Value> =
        serde_json::from_slice(body).map_err(|_| "Invalid Circus queue response")?;
    let pending = queue
        .iter()
        .filter(|entry| entry["system"] == system)
        .map(|entry| {
            if qemu_overflow {
                entry["qemu_overflow_count"].as_u64().unwrap_or(0)
            } else {
                entry["count"].as_u64().unwrap_or(0)
            }
        })
        .sum();
    Ok(Demand {
        pending,
        building: 0,
    })
}

pub(super) struct Capacity {
    pub minimum: usize,
    pub maximum: usize,
    pub pending_threshold: u64,
    pub sustained_seconds: f64,
}

#[derive(Default)]
pub(super) struct ScaleUp {
    high_since: Option<f64>,
}

impl ScaleUp {
    pub fn decide(
        &mut self,
        capacity: &Capacity,
        now: f64,
        queued: u64,
        active: usize,
        booting: bool,
    ) -> bool {
        if queued > capacity.pending_threshold {
            self.high_since.get_or_insert(now);
        } else {
            self.high_since = None;
        }
        let sustained = self
            .high_since
            .is_some_and(|since| now - since >= capacity.sustained_seconds);
        (sustained || active < capacity.minimum) && active < capacity.maximum && !booting
    }

    pub fn attempted(&mut self, now: f64) {
        self.high_since = Some(now);
    }
}

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::openstack::setting;

type Workers = BTreeMap<String, Value>;

pub(super) struct PoolFiles {
    state: PathBuf,
    known_hosts: PathBuf,
    /// Nix machines file of nixbot's pool; a pool without builders (Circus
    /// agents dial in themselves) has none.
    machines: Option<PathBuf>,
    static_machines: Option<PathBuf>,
}

impl PoolFiles {
    pub fn from_settings(cfg: &Value) -> Result<Self, String> {
        let state_dir = Path::new(setting(cfg, "stateDir")?);
        Ok(Self {
            state: state_dir.join("state.json"),
            known_hosts: state_dir.join("known_hosts"),
            machines: cfg["machinesFile"].as_str().map(PathBuf::from),
            static_machines: cfg["staticMachinesFile"].as_str().map(PathBuf::from),
        })
    }

    pub fn load(&self) -> Result<Workers, String> {
        match File::open(&self.state) {
            Ok(file) => {
                serde_json::from_reader(file).map_err(|error| format!("Read pool state: {error}"))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Workers::new()),
            Err(error) => Err(format!("Open pool state: {error}")),
        }
    }

    pub fn save(&self, workers: &Workers) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(workers).map_err(|error| error.to_string())?;
        replace(&self.state, &bytes, 0o600).map_err(|error| format!("Save pool state: {error}"))
    }

    pub fn static_slots(&self, local: u64) -> Result<u64, String> {
        let Some(static_machines) = &self.static_machines else {
            return Ok(local);
        };
        let text = fs::read_to_string(static_machines)
            .map_err(|error| format!("Read static builders: {error}"))?;
        Ok(text.lines().fold(local, |slots, line| {
            slots
                + line
                    .split_whitespace()
                    .nth(3)
                    .and_then(|count| count.parse().ok())
                    .unwrap_or(0)
        }))
    }

    pub fn write_builders(&self, cfg: &Value, workers: &Workers) -> Result<(), String> {
        let Some(machines_file) = &self.machines else {
            return self.write_known_hosts(workers);
        };
        let static_machines = match &self.static_machines {
            Some(path) => fs::read_to_string(path)
                .map_err(|error| format!("Read static builders: {error}"))?,
            None => String::new(),
        };
        let mut machines: String = static_machines
            .split_terminator('\n')
            .filter(|line| !line.trim().is_empty())
            .flat_map(|line| [line, "\n"])
            .collect();
        let mut known = String::new();
        let user = setting(cfg, "sshUser")?;
        let key_file = setting(cfg, "machinesSshKeyFile")?;
        let jobs = cfg["maxJobs"].as_u64().ok_or("Missing setting: maxJobs")?;
        let features = cfg["supportedFeatures"]
            .as_array()
            .ok_or("Missing setting: supportedFeatures")?
            .iter()
            .map(|feature| feature.as_str().ok_or("Invalid supported feature"))
            .collect::<Result<Vec<_>, _>>()?
            .join(",");
        for worker in workers.values().filter(|worker| worker["ready"] == true) {
            let ip = setting(worker, "ip")?;
            let key = setting(worker, "hostKey")?;
            if worker["draining"] != true {
                let encoded = crate::cmd::simple::base64(key.as_bytes());
                machines.push_str(&format!(
                    "ssh-ng://{user}@{ip} x86_64-linux {key_file} {jobs} 1 {features} - {encoded}\n"
                ));
            }
            let mut ips = BTreeSet::from([ip]);
            if let Some(overlays) = worker["ips"].as_object() {
                ips.extend(overlays.values().filter_map(Value::as_str));
            }
            for ip in ips {
                known.push_str(&format!("{ip} {key}\n"));
            }
        }
        replace(&self.known_hosts, known.as_bytes(), 0o644)
            .map_err(|error| format!("Publish pool known_hosts: {error}"))?;
        replace(machines_file, machines.as_bytes(), 0o644)
            .map_err(|error| format!("Publish pool builders: {error}"))
    }

    /// The pool's host keys only, for a pool that publishes no builders.
    fn write_known_hosts(&self, workers: &Workers) -> Result<(), String> {
        let mut known = String::new();
        for worker in workers.values().filter(|worker| worker["ready"] == true) {
            let key = setting(worker, "hostKey")?;
            let mut ips = BTreeSet::from([setting(worker, "ip")?]);
            if let Some(overlays) = worker["ips"].as_object() {
                ips.extend(overlays.values().filter_map(Value::as_str));
            }
            for ip in ips {
                known.push_str(&format!("{ip} {key}\n"));
            }
        }
        replace(&self.known_hosts, known.as_bytes(), 0o644)
            .map_err(|error| format!("Publish pool known_hosts: {error}"))
    }
}

fn replace(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let id = super::niks3::token_id()?;
    let temporary = path.with_extension(format!("{id}.tmp"));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(mode)
        .open(&temporary)?;
    let result = file.write_all(bytes).and_then(|()| {
        drop(file);
        fs::rename(&temporary, path)
    });
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

use super::{
    epoch,
    mutations::{self, Compute},
    netbird::NetBird,
    openstack::OpenStack,
    pools, provision,
    up::{self, Boot},
    worker_ssh,
};
use serde_json::json;

pub(super) fn run(cfg: &Value, once: bool) -> Result<(), String> {
    let seconds = count(cfg, "pollSeconds")?;
    let files = PoolFiles::from_settings(cfg)?;
    let mut pool = Pool {
        workers: files.load()?,
        files,
        cfg,
        compute: OpenStack::from_settings(cfg)?,
        netbird: NetBird::from_settings(cfg)?,
        scale: ScaleUp::default(),
        last_membership_check: 0.0,
    };
    pool.files.write_builders(cfg, &pool.workers)?;
    if once {
        return pool.reconcile();
    }
    loop {
        if let Err(error) = pool.reconcile() {
            eprintln!("reconcile failed: {error}");
        }
        std::thread::sleep(std::time::Duration::from_secs(seconds));
    }
}

fn count(cfg: &Value, key: &str) -> Result<u64, String> {
    cfg[key]
        .as_u64()
        .ok_or_else(|| format!("Missing setting: {key}"))
}

struct Pool<'a> {
    workers: Workers,
    files: PoolFiles,
    cfg: &'a Value,
    compute: OpenStack,
    netbird: NetBird,
    scale: ScaleUp,
    last_membership_check: f64,
}

impl Pool<'_> {
    fn publish(&self) -> Result<(), String> {
        self.files.save(&self.workers)?;
        self.files.write_builders(self.cfg, &self.workers)
    }

    fn reconcile(&mut self) -> Result<(), String> {
        let cfg = self.cfg;
        let now = epoch();
        let own = pools::own_pool(cfg);
        let classification = legacy_prefixes(cfg);
        let pools = pools::Pools::from_settings(&classification)?;
        if now - self.last_membership_check > 600.0 {
            self.netbird.ensure_member(
                setting(cfg, "netbirdNixbotGroup")?,
                setting(cfg, "netbirdSelf")?,
            )?;
            self.last_membership_check = now;
        }
        if cfg["reapAdhoc"] == true {
            super::reap_command::reap(cfg, false)?;
        }
        let output = self.compute.command(&[
            "server",
            "list",
            "-f",
            "json",
            "-c",
            "ID",
            "-c",
            "Name",
            "-c",
            "Properties",
        ])?;
        let list: Vec<Value> =
            serde_json::from_slice(&output).map_err(|_| "Invalid OpenStack server listing")?;
        let mut servers = BTreeMap::new();
        for server in &list {
            let name = setting(server, "Name")?;
            let listed = server["Properties"]
                .as_object()
                .cloned()
                .unwrap_or_default();
            // This pool's workers by their metadata; `namePrefix` only for
            // workers created before `rworker-pool` existed.
            if pools.classify(name, &listed) == Some(pools::Kind::Pool(own.clone())) {
                servers.insert(name, setting(server, "ID")?);
            }
        }
        for (name, id) in &servers {
            self.workers.entry((*name).into()).or_insert_with(|| {
                eprintln!("recovering unrecognized pool worker {name}");
                json!({"id": id, "created": now, "ready": false, "recovering": true})
            });
        }
        let vanished: Vec<_> = self
            .workers
            .iter()
            .filter(|(name, worker)| {
                !servers.contains_key(name.as_str()) && worker["ready"] == true
            })
            .map(|(name, _)| name.clone())
            .collect();
        for name in vanished {
            self.netbird.delete_peer(&name)?;
            self.workers.remove(&name);
            self.publish()?;
        }
        for (name, worker) in &self.workers {
            if worker["ready"] != true {
                continue;
            }
            let Some(&id) = servers.get(name.as_str()) else {
                continue;
            };
            let id = id.to_owned();
            let metadata = self.compute.metadata(&id)?;
            let labels = PoolLabels::from_settings(cfg);
            let wanted = [
                ("host_key", json!(setting(worker, "hostKey")?)),
                (
                    "owner",
                    json!(format!("{}@{}", labels.agent, setting(cfg, "netbirdSelf")?)),
                ),
                ("nixbot-worker", json!(labels.kind)),
                (pools::POOL_KEY, json!(pools::own_pool(cfg))),
                ("agent", json!(labels.agent)),
                ("topic", json!(labels.topic)),
            ]
            .into_iter()
            .filter(|(key, value)| metadata.get(*key) != Some(value))
            .map(|(key, value)| (key.to_owned(), value))
            .collect::<serde_json::Map<_, _>>();
            if !wanted.is_empty() {
                self.compute.set_metadata(&id, wanted)?;
            }
        }
        let demand = Demand::from_settings(cfg)?;
        let active = self
            .workers
            .values()
            .filter(|worker| worker["draining"] != true)
            .count();
        let ready = self
            .workers
            .values()
            .filter(|worker| worker["ready"] == true && worker["draining"] != true)
            .count();
        let booting = self.workers.values().any(|worker| worker["ready"] != true);
        let slots = self.files.static_slots(count(cfg, "localMaxJobs")?)?
            + count(cfg, "maxJobs")? * ready as u64;
        let queued = demand.queued(slots);
        let minimum = count(cfg, "minWorkers")? as usize;
        let names: Vec<_> = self.workers.keys().cloned().collect();
        for name in names {
            let mut worker = self.workers[&name].clone();
            if self.reconcile_worker(&name, &mut worker, now, queued == 0 && active > minimum)? {
                self.workers.insert(name, worker);
            } else {
                self.workers.remove(&name);
            }
            self.publish()?;
        }
        self.files.save(&self.workers)?;
        let capacity = Capacity {
            minimum,
            maximum: count(cfg, "maxWorkers")? as usize,
            pending_threshold: count(cfg, "scaleUpPending")?,
            sustained_seconds: count(cfg, "scaleUpAfter")? as f64,
        };
        if self.scale.decide(&capacity, now, queued, active, booting) {
            eprintln!(
                "demand: {queued} queued ({} pending, {} building, {slots} slots)",
                demand.pending, demand.building
            );
            if let Err(error) = self.create_worker(setting(cfg, "namePrefix")?) {
                eprintln!("not scaling up: {error}");
            }
            self.scale.attempted(now);
        }
        Ok(())
    }

    fn create_worker(&mut self, prefix: &str) -> Result<(), String> {
        let cfg = self.cfg;
        let suffix = super::niks3::token_id().map_err(|error| error.to_string())?;
        let name = format!("{prefix}{}", &suffix[..8]);
        let labels = PoolLabels::from_settings(cfg);
        let metadata = [
            ("nixbot-worker".into(), json!(labels.kind)),
            (pools::POOL_KEY.into(), json!(pools::own_pool(cfg))),
            (
                "owner".into(),
                json!(format!("{}@{}", labels.agent, setting(cfg, "netbirdSelf")?)),
            ),
            ("agent".into(), json!(labels.agent)),
            ("topic".into(), json!(labels.topic)),
        ]
        .into_iter()
        .collect();
        let mut user_data = user_data(cfg)?;
        if !user_data.contains_key("agent_authorized_keys")
            && let Some(key) = super::worker_ssh::web_pool_key()?
        {
            user_data.insert("agent_authorized_keys".into(), json!([key]));
        }
        let (id, metadata) = provision::create(
            cfg,
            &self.compute,
            provision::Request {
                name: &name,
                group: setting(cfg, "netbirdWorkersGroup")?,
                flavor: setting(cfg, "flavor")?,
                disk: count(cfg, "volumeSize")? as i64,
                profile: cfg["profile"].as_str().unwrap_or("standard"),
                metadata,
                user_data,
            },
        )?;
        self.workers.insert(name, json!({
            "id": id, "created": epoch(), "ready": false, "setupKeyId": metadata["setup_key_id"],
        }));
        self.files.save(&self.workers)
    }

    fn reconcile_worker(
        &mut self,
        name: &str,
        worker: &mut Value,
        now: f64,
        may_drain: bool,
    ) -> Result<bool, String> {
        let cfg = self.cfg;
        let id = setting(worker, "id")?.to_owned();
        if worker["ready"] != true {
            let recovering = worker["recovering"] == true;
            let timed_out = now
                - worker["created"]
                    .as_f64()
                    .ok_or("Missing worker creation time")?
                > count(cfg, "bootTimeout")? as f64;
            if !recovering && timed_out {
                eprintln!("{}", up::boot_diagnostics(cfg, &self.compute, &id, name));
                self.remove(name, &id, worker, "did not come up in time")?;
                return Ok(false);
            }
            match up::boot_status(cfg, &self.compute, &id, name)?.1 {
                Boot::Waiting => {}
                Boot::Failed if recovering => {
                    eprintln!("cannot recover {name}: server is in ERROR state")
                }
                Boot::Failed => {
                    eprintln!("{}", up::boot_diagnostics(cfg, &self.compute, &id, name));
                    self.remove(name, &id, worker, "server in ERROR state")?;
                    return Ok(false);
                }
                Boot::Ready((key, ip, ips)) => {
                    self.compute.set_metadata(
                        &id,
                        [("host_key".into(), json!(key))].into_iter().collect(),
                    )?;
                    if let Some(key) = worker["setupKeyId"].as_str() {
                        self.netbird.delete_key(key);
                    }
                    let _ = self.compute.delete_metadata(&id, &["setup_key_id"]);
                    if cfg["tailscale"].is_object() {
                        let result = (|| -> Result<(), String> {
                            let Some((_, metadata)) = self.compute.worker(name)? else {
                                return Ok(());
                            };
                            if let Some(key) = metadata
                                .get("ts_key_id")
                                .and_then(Value::as_str)
                                .filter(|key| !key.is_empty())
                            {
                                let mut client =
                                    super::tailscale::Tailscale::from_settings(&cfg["tailscale"])?;
                                client.delete_key(key);
                                self.compute.delete_metadata(&id, &["ts_key_id"])?;
                            }
                            Ok(())
                        })();
                        if let Err(error) = result {
                            eprintln!("releasing the Tailscale key of {name}: {error}");
                        }
                    }
                    worker["ready"] = json!(true);
                    worker["hostKey"] = json!(key);
                    worker["ip"] = json!(ip);
                    worker["ips"] = json!(ips);
                    worker["lastBusy"] = json!(epoch());
                    if let Some(fields) = worker.as_object_mut() {
                        fields.remove("recovering");
                        fields.remove("setupKeyId");
                    }
                    eprintln!("{name} is ready at {ip}");
                }
            }
            return Ok(true);
        }
        let mut ip = setting(worker, "ip")?
            .parse()
            .map_err(|_| "Invalid pool worker IP")?;
        if worker["draining"] != true
            && now - worker["routeCheck"].as_f64().unwrap_or_default() >= 60.0
        {
            worker["routeCheck"] = json!(now);
            if !worker_ssh::answers(ip) {
                let (route, ips) = worker_ssh::routes(cfg, name)?;
                if let Some(route) = route {
                    ip = route;
                    worker["ip"] = json!(ip);
                    worker["ips"] = json!(ips);
                }
            }
        }
        let busy = worker_ssh::run(
            cfg,
            ip,
            setting(worker, "hostKey")?,
            "find /nix/.rw-store/builds -mindepth 1 -maxdepth 1 -print -quit",
            None,
            30,
        )
        .map_or(true, |output| {
            !output.status.success() || !String::from_utf8_lossy(&output.stdout).trim().is_empty()
        });
        if worker["draining"] == true {
            if busy || !worker["drainQuietSince"].is_number() {
                worker["drainQuietSince"] = json!(now);
            } else if now - worker["drainQuietSince"].as_f64().unwrap_or(now) >= 120.0 {
                self.remove(name, &id, worker, "drained")?;
                return Ok(false);
            }
            return Ok(true);
        }
        let state = if busy {
            worker["lastBusy"] = json!(now);
            "busy"
        } else if may_drain
            && now - worker["lastBusy"].as_f64().unwrap_or(now) > count(cfg, "idleSeconds")? as f64
        {
            worker["draining"] = json!(true);
            worker["drainQuietSince"] = json!(now);
            "draining"
        } else {
            "idle"
        };
        if worker["state"] != state {
            worker["state"] = json!(state);
            let last = worker["lastBusy"].as_f64().unwrap_or(now) as i64;
            if let Err(error) = self.compute.set_metadata(
                &id,
                [
                    ("pool_state".into(), json!(state)),
                    ("last_busy".into(), json!(last.to_string())),
                ]
                .into_iter()
                .collect(),
            ) {
                eprintln!("recording the state of {name}: {error}");
            }
        }
        Ok(true)
    }

    fn remove(&mut self, name: &str, id: &str, worker: &Value, reason: &str) -> Result<(), String> {
        eprintln!("deleting {name} ({reason})");
        if let Some((current_id, mut metadata)) = self.compute.worker(name)?
            && current_id == id
        {
            if let Some(key) = worker["setupKeyId"].as_str() {
                metadata.insert("setup_key_id".into(), json!(key));
            }
            mutations::delete(self.cfg, &self.compute, &self.netbird, id, name, &metadata)?;
        }
        Ok(())
    }
}

/// Retain other pools' more specific prefixes when classifying legacy workers.
fn legacy_prefixes(cfg: &Value) -> Value {
    let mut prefixes = cfg["poolPrefixes"].as_object().cloned().unwrap_or_default();
    if let Some(prefix) = cfg["namePrefix"].as_str() {
        prefixes.insert(prefix.into(), json!(pools::own_pool(cfg)));
    }
    json!({ "adhocPrefix": "", "poolPrefixes": prefixes })
}

/// How the pool's workers are labelled in their OpenStack metadata (`ls`,
/// the console): nixbot's pool by default, another pool (Circus) by setting
/// `poolLabels`.
pub(super) struct PoolLabels<'a> {
    pub kind: &'a str,
    pub agent: &'a str,
    pub topic: &'a str,
}

impl<'a> PoolLabels<'a> {
    pub fn from_settings(cfg: &'a Value) -> Self {
        let labels = &cfg["poolLabels"];
        Self {
            kind: labels["kind"].as_str().unwrap_or("pool"),
            agent: labels["agent"].as_str().unwrap_or("nixbot"),
            topic: labels["topic"].as_str().unwrap_or("build pool"),
        }
    }
}

/// User-data for a new worker from `userDataFiles` (key -> secret file),
/// read at creation so the values never sit in the settings file. The
/// worker reads them from its metadata service at boot.
pub(super) fn user_data(cfg: &Value) -> Result<serde_json::Map<String, Value>, String> {
    let mut data = serde_json::Map::new();
    if let Some(files) = cfg["userDataFiles"].as_object() {
        for (key, path) in files {
            let path = path
                .as_str()
                .ok_or_else(|| format!("Invalid userDataFiles entry: {key}"))?;
            let value = fs::read_to_string(path)
                .map_err(|error| format!("Read user-data {key}: {error}"))?;
            let value = value.trim();
            if value.is_empty() {
                return Err(format!("Empty user-data {key}"));
            }
            data.insert(key.clone(), json!(value));
        }
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regular_circus_pool_does_not_adopt_legacy_qemu_workers() {
        let cfg = json!({
            "namePrefix": "circus-",
            "poolName": "circus",
            "poolPrefixes": {"circus-qemu-": "circus-qemu"}
        });
        let settings = legacy_prefixes(&cfg);
        let classifier = pools::Pools::from_settings(&settings).unwrap();
        let metadata = serde_json::Map::new();
        assert_eq!(
            classifier.classify("circus-qemu-1234", &metadata),
            Some(pools::Kind::Pool("circus-qemu".into()))
        );
    }

    #[test]
    fn circus_queue_counts_only_the_pool_system() {
        let body = br#"[{"system":"x86_64-linux","count":7},{"system":"aarch64-linux","count":3,"qemu_overflow_count":1}]"#;
        let demand = parse_circus_queue(body, "x86_64-linux", false).unwrap();
        assert_eq!(demand.pending, 7);
        assert_eq!(demand.building, 0);
        assert_eq!(demand.queued(16), 7);

        let overflow = parse_circus_queue(body, "aarch64-linux", true).unwrap();
        assert_eq!(overflow.pending, 1);
        assert_eq!(overflow.queued(16), 1);

        let stale_or_unconfigured = parse_circus_queue(
            br#"[{"system":"aarch64-linux","count":8}]"#,
            "aarch64-linux",
            true,
        )
        .unwrap();
        assert_eq!(stale_or_unconfigured.pending, 0);
    }

    #[test]
    fn circus_queue_without_the_system_is_idle() {
        let demand = parse_circus_queue(b"[]", "x86_64-linux", false).unwrap();
        assert_eq!(demand.pending, 0);
        assert!(parse_circus_queue(b"{}", "x86_64-linux", false).is_err());
    }

    #[test]
    fn unknown_demand_source_is_an_error() {
        let cfg = json!({"demand": {"source": "jenkins"}});
        assert!(Demand::from_settings(&cfg).is_err());
    }

    #[test]
    fn labels_default_to_nixbots_pool() {
        let cfg = json!({});
        let labels = PoolLabels::from_settings(&cfg);
        assert_eq!(
            (labels.kind, labels.agent, labels.topic),
            ("pool", "nixbot", "build pool")
        );
        let cfg = json!({"poolLabels": {"kind": "circus-pool", "agent": "circus", "topic": "circus pool"}});
        let labels = PoolLabels::from_settings(&cfg);
        assert_eq!(
            (labels.kind, labels.agent, labels.topic),
            ("circus-pool", "circus", "circus pool")
        );
    }

    #[test]
    fn user_data_reads_and_trims_files() {
        let dir = std::env::temp_dir().join(format!("rworker-ud-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("token");
        fs::write(&file, "secret-value\n").unwrap();
        let cfg = json!({"userDataFiles": {"circus_join_token": file.to_str().unwrap()}});
        let data = user_data(&cfg).unwrap();
        assert_eq!(data["circus_join_token"], "secret-value");
        fs::write(&file, "\n").unwrap();
        assert!(user_data(&cfg).is_err());
        assert!(user_data(&json!({})).unwrap().is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pool_without_machines_counts_local_slots_only() {
        let files = PoolFiles::from_settings(&json!({"stateDir": "/nonexistent"})).unwrap();
        assert_eq!(files.static_slots(4).unwrap(), 4);
    }
}
