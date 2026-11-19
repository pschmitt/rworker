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
            .env("PGDATABASE", database_url)
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
    machines: PathBuf,
    static_machines: PathBuf,
}

impl PoolFiles {
    pub fn from_settings(cfg: &Value) -> Result<Self, String> {
        let state_dir = Path::new(setting(cfg, "stateDir")?);
        Ok(Self {
            state: state_dir.join("state.json"),
            known_hosts: state_dir.join("known_hosts"),
            machines: PathBuf::from(setting(cfg, "machinesFile")?),
            static_machines: PathBuf::from(setting(cfg, "staticMachinesFile")?),
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
        let text = fs::read_to_string(&self.static_machines)
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
        let static_machines = fs::read_to_string(&self.static_machines)
            .map_err(|error| format!("Read static builders: {error}"))?;
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
        replace(&self.machines, machines.as_bytes(), 0o644)
            .map_err(|error| format!("Publish pool builders: {error}"))
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
    provision,
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
        let prefix = setting(cfg, "namePrefix")?;
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
        let output = self
            .compute
            .command(&["server", "list", "-f", "json", "-c", "ID", "-c", "Name"])?;
        let list: Vec<Value> =
            serde_json::from_slice(&output).map_err(|_| "Invalid OpenStack server listing")?;
        let mut servers = BTreeMap::new();
        for server in &list {
            let name = setting(server, "Name")?;
            if name.starts_with(prefix) {
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
            if worker["ready"] != true || !servers.contains_key(name.as_str()) {
                continue;
            }
            let Some((id, metadata)) = self.compute.worker(prefix, name)? else {
                continue;
            };
            let wanted = [
                ("host_key", json!(setting(worker, "hostKey")?)),
                (
                    "owner",
                    json!(format!("nixbot@{}", setting(cfg, "netbirdSelf")?)),
                ),
                ("nixbot-worker", json!("pool")),
                ("agent", json!("nixbot")),
                ("topic", json!("build pool")),
            ]
            .into_iter()
            .filter(|(key, value)| metadata.get(*key) != Some(value))
            .map(|(key, value)| (key.to_owned(), value))
            .collect::<serde_json::Map<_, _>>();
            if !wanted.is_empty() {
                self.compute.set_metadata(&id, wanted)?;
            }
        }
        let demand = Demand::read(setting(cfg, "databaseUrl")?)?;
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
            if let Err(error) = self.create_worker(prefix) {
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
        let metadata = [
            ("nixbot-worker".into(), json!("pool")),
            (
                "owner".into(),
                json!(format!("nixbot@{}", setting(cfg, "netbirdSelf")?)),
            ),
            ("agent".into(), json!("nixbot")),
            ("topic".into(), json!("build pool")),
        ]
        .into_iter()
        .collect();
        let (id, metadata) = provision::create(
            cfg,
            &self.compute,
            provision::Request {
                name: &name,
                group: setting(cfg, "netbirdWorkersGroup")?,
                flavor: setting(cfg, "flavor")?,
                disk: count(cfg, "volumeSize")? as i64,
                profile: "standard",
                metadata,
                user_data: Default::default(),
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
                self.remove(name, &id, worker, "did not come up in time")?;
                return Ok(false);
            }
            match up::boot_status(cfg, &self.compute, &id, name)?.1 {
                Boot::Waiting => {}
                Boot::Failed if recovering => {
                    eprintln!("cannot recover {name}: server is in ERROR state")
                }
                Boot::Failed => {
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
                            let Some((_, metadata)) =
                                self.compute.worker(setting(cfg, "namePrefix")?, name)?
                            else {
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
        let prefix = setting(self.cfg, "namePrefix")?;
        eprintln!("deleting {name} ({reason})");
        if let Some((current_id, mut metadata)) = self.compute.worker(prefix, name)?
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
