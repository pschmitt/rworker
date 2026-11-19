pub(super) fn reap(cfg: &serde_json::Value) -> Result<(), String> {
    use super::reaper::{AfterProbe, BeforeProbe, Policy, Worker};
    use super::{
        mutations::{self, Compute},
        netbird::NetBird,
        openstack::{OpenStack, setting},
        worker_ssh,
    };
    use serde_json::{Value, json};

    let mut compute = OpenStack::from_settings(cfg)?;
    let netbird = NetBird::from_settings(cfg)?;
    let prefix = setting(cfg, "adhocPrefix")?;
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
    let output = compute.command(&["server", "list", "-f", "json", "-c", "ID", "-c", "Name"])?;
    let servers: Vec<Value> =
        serde_json::from_slice(&output).map_err(|_| "Invalid OpenStack server listing")?;
    for server in servers {
        let name = setting(&server, "Name")?;
        if !name.starts_with(prefix) {
            continue;
        }
        let Some((id, metadata)) = compute.worker(prefix, name)? else {
            continue;
        };
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
        let reason = match policy.before_probe(&worker, now) {
            BeforeProbe::Keep => continue,
            BeforeProbe::DeleteUnready => "never became ready",
            BeforeProbe::DeleteAtMaximumLifetime => "maximum lifetime reached",
            BeforeProbe::Probe => {
                let ip = worker_ssh::route(cfg, name)?;
                let key = metadata
                    .get("host_key")
                    .and_then(Value::as_str)
                    .filter(|key| !key.is_empty());
                let active = ip
                    .zip(key)
                    .and_then(|(ip, key)| worker_ssh::active(cfg, ip, key));
                match policy.after_probe(&worker, now, active) {
                    AfterProbe::Keep => continue,
                    AfterProbe::DeleteExpired => "expired and idle",
                    AfterProbe::Busy {
                        record_activity,
                        report_expired,
                        renew_token_until,
                    } => {
                        if record_activity {
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
                        if let Some(end) = renew_token_until {
                            super::niks3::install(
                                cfg,
                                &mut compute,
                                &id,
                                name,
                                &metadata,
                                end as i64,
                            )?;
                        }
                        continue;
                    }
                }
            }
        };
        eprintln!("deleting {name} ({reason})");
        mutations::delete(cfg, &compute, &netbird, &id, name, &metadata)?;
    }
    Ok(())
}
