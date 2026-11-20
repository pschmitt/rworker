pub(super) fn reap(cfg: &serde_json::Value, dry_run: bool) -> Result<(), String> {
    use super::reaper::{AfterProbe, BeforeProbe, Policy, Worker};
    use super::{
        mutations::{self, Compute},
        netbird::NetBird,
        openstack::{OpenStack, setting},
        pools::{Kind, Pools},
        worker_ssh,
    };
    use serde_json::{Value, json};

    let mut compute = OpenStack::from_settings(cfg)?;
    let netbird = NetBird::from_settings(cfg)?;
    let pools = Pools::from_settings(cfg)?;
    let number = |key: &str| {
        cfg[key]
            .as_f64()
            .ok_or_else(|| format!("Missing setting: {key}"))
    };
    let policy = Policy {
        boot_timeout: number("bootTimeout")?,
        grace_period: number("adhocGracePeriod")?,
        max_ttl: number("adhocMaxTtl")?,
    };
    let now = super::epoch();
    let output = compute.command(&[
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
    let servers: Vec<Value> =
        serde_json::from_slice(&output).map_err(|_| "Invalid OpenStack server listing")?;
    for server in servers {
        let name = setting(&server, "Name")?;
        // Only ever ad-hoc workers: pool workers belong to their autoscaler,
        // whatever their name says.
        let listed = server["Properties"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        if pools.classify(name, &listed) != Some(Kind::Adhoc) {
            continue;
        }
        let id = setting(&server, "ID")?.to_owned();
        let metadata = compute.metadata(&id)?;
        let number = |key| {
            metadata
                .get(key)
                .and_then(|value| value.as_f64().or_else(|| value.as_str()?.parse().ok()))
        };
        let worker = Worker {
            created_at: number("created_at"),
            expires_at: number("expires_at").unwrap_or_default(),
            max_lifetime: number("max_lifetime"),
            host_key_present: metadata.contains_key("host_key"),
            last_active: number("last_active").unwrap_or_default(),
            token_expires_at: number("niks3_token_exp").unwrap_or_default(),
        };
        let decision = policy.before_probe(&worker, now);
        if decision == BeforeProbe::Keep {
            continue;
        }
        let protect_in_use = cfg["reapProtectInUse"] == true;
        let active = if decision == BeforeProbe::Probe || protect_in_use {
            let key = metadata
                .get("host_key")
                .and_then(Value::as_str)
                .filter(|key| !key.is_empty());
            match key {
                Some(key) => worker_ssh::activity(cfg, name, key, protect_in_use)?,
                None => None,
            }
        } else {
            None
        };
        let reason = match decision {
            BeforeProbe::Keep => continue,
            BeforeProbe::DeleteUnready => "never became ready",
            BeforeProbe::DeleteAtMaximumLifetime => "maximum lifetime reached",
            BeforeProbe::Probe => match policy.after_probe(&worker, now, active) {
                AfterProbe::Keep => continue,
                AfterProbe::DeleteExpired => "expired and idle",
                AfterProbe::Busy {
                    record_activity,
                    report_expired,
                    renew_token_until,
                } => {
                    if record_activity && !dry_run {
                        compute.set_metadata(
                            &id,
                            [("last_active".into(), json!((now as i64).to_string()))]
                                .into_iter()
                                .collect(),
                        )?;
                    }
                    if report_expired {
                        eprintln!("{name} expired but is in use; keeping it");
                    }
                    if let Some(end) = renew_token_until.filter(|_| !dry_run) {
                        super::niks3::install(cfg, &mut compute, &id, name, &metadata, end as i64)?;
                    }
                    continue;
                }
            },
        };
        if protect_in_use && active != Some(false) {
            let activity = if active == Some(true) {
                "worker is active"
            } else {
                "activity could not be ruled out"
            };
            eprintln!("keeping {name} ({activity})");
            continue;
        }
        if dry_run {
            eprintln!("would delete {name} ({reason})");
        } else {
            let current = compute.metadata(&id)?;
            if current != metadata {
                eprintln!("keeping {name} (metadata changed during reaping)");
                continue;
            }
            eprintln!("deleting {name} ({reason})");
            mutations::delete(cfg, &compute, &netbird, &id, name, &current)?;
        }
    }
    Ok(())
}
