use std::collections::{BTreeMap, HashMap};

use serde_json::{Value, json};

use super::{
    metadata::CREATOR_KEYS,
    netbird::NetBird,
    openstack::{OpenStack, setting},
    tailscale::Tailscale,
};

pub(super) fn list(cfg: &Value) -> Result<Value, String> {
    let compute = OpenStack::from_settings(cfg)?;
    let netbird = NetBird::from_settings(cfg)?;
    let prefixes = [setting(cfg, "adhocPrefix")?, setting(cfg, "poolPrefix")?];
    let list = compute.command(&["server", "list", "-f", "json", "-c", "ID", "-c", "Name"])?;
    let list: Vec<Value> =
        serde_json::from_slice(&list).map_err(|_| "Invalid OpenStack server listing")?;
    let mut ids = BTreeMap::new();
    for server in &list {
        let name = setting(server, "Name")?;
        if prefixes.iter().any(|prefix| name.starts_with(prefix)) {
            ids.insert(name, setting(server, "ID")?);
        }
    }
    let peers: HashMap<_, _> = netbird
        .peers()?
        .into_iter()
        .filter_map(|peer| Some((peer["name"].as_str()?.to_owned(), peer)))
        .collect();
    let devices = if cfg["tailscale"].is_object() {
        Tailscale::from_settings(&cfg["tailscale"])
            .and_then(|mut client| client.devices())
            .unwrap_or_else(|err| {
                eprintln!("Tailscale device list: {err}");
                HashMap::new()
            })
    } else {
        HashMap::new()
    };
    let mut workers = Vec::new();
    for (name, id) in ids {
        let output = compute.command(&["server", "show", "-f", "json", "--", id])?;
        let detail: Value =
            serde_json::from_slice(&output).map_err(|_| "Invalid OpenStack server detail")?;
        workers.push(info(
            cfg,
            name,
            &detail,
            peers.get(name),
            devices.get(name),
        )?);
    }
    Ok(Value::Array(workers))
}

fn info(
    cfg: &Value,
    name: &str,
    detail: &Value,
    peer: Option<&Value>,
    device: Option<&Value>,
) -> Result<Value, String> {
    let mut meta = match &detail["properties"] {
        Value::Object(value) => value.clone(),
        Value::Null => Default::default(),
        _ => return Err("Invalid OpenStack server properties".into()),
    };
    let number = |key| {
        meta.get(key)
            .and_then(|value| value.as_f64().or_else(|| value.as_str()?.parse().ok()))
    };
    let expires = number("expires_at").unwrap_or_default();
    let last = number("last_active").unwrap_or_default();
    let max = number("max_lifetime");
    let created = number("created_at");
    let grace = if expires != 0.0 && last >= expires {
        (last + cfg["adhocGracePeriod"].as_f64().unwrap_or_default()).min(
            created.unwrap_or_else(super::epoch)
                + max.unwrap_or_else(|| cfg["adhocMaxTtl"].as_f64().unwrap_or_default()),
        )
    } else {
        0.0
    };
    let netbird = peer
        .and_then(|peer| peer["ip"].as_str())
        .filter(|ip| !ip.is_empty());
    let tailscale = device
        .and_then(|device| device["addresses"].as_array())
        .and_then(|addresses| {
            addresses
                .iter()
                .filter_map(Value::as_str)
                .find(|ip| ip.contains('.'))
        });
    let mut ips = serde_json::Map::new();
    for (overlay, ip) in [("netbird", netbird), ("tailscale", tailscale)] {
        if let Some(ip) = ip {
            ips.insert(overlay.into(), json!(ip));
        }
    }
    let status = if detail["OS-EXT-STS:task_state"] == "deleting" {
        json!("DELETING")
    } else {
        detail["status"].clone()
    };
    let mut worker = json!({
        "name": name,
        "kind": match meta.get("nixbot-worker") {
            Some(value) if value == "1" => json!("pool"),
            Some(value) => value.clone(),
            None => json!("unknown"),
        },
        "host_key": meta.get("host_key"),
        "created_at": detail["created"],
        "status": status,
        "flavor": detail["flavor"]["original_name"],
        "profile": meta.get("worker_profile").cloned().unwrap_or_else(|| json!("standard")),
        "owner": meta.get("owner"),
        "last_active": number("last_active").map(|value| value as i64),
        "pool_state": meta.get("pool_state"),
        "last_busy": number("last_busy").map(|value| value as i64),
        "expires_at": expires as i64,
        "niks3_token_expires_at": number("niks3_token_exp").unwrap_or_default() as i64,
        "grace_until": grace as i64,
        "max_lifetime_end": created.map_or(0, |value| (value + max.unwrap_or_default()) as i64),
        "ip": netbird.or(tailscale),
        "ips": ips,
        "connected": peer.is_some_and(|p| p["connected"].as_bool() == Some(true))
            || device.is_some_and(|d| d["connectedToControl"].as_bool() == Some(true)),
        "availability_zone": detail["OS-EXT-AZ:availability_zone"],
    });
    for key in CREATOR_KEYS {
        worker[key] = meta.get(key).cloned().unwrap_or_default();
    }
    meta.remove("setup_key_id");
    meta.remove("ts_key_id");
    worker["metadata"] = Value::Object(meta);
    Ok(worker)
}
