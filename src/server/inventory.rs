use std::collections::{BTreeMap, HashMap};

use serde_json::{Value, json};

use super::{
    metadata::CREATOR_KEYS,
    netbird::NetBird,
    openstack::{OpenStack, setting},
    pools::{Kind, Pools},
    tailscale::Tailscale,
};

pub(super) fn list(cfg: &Value) -> Result<Value, String> {
    let compute = OpenStack::from_settings(cfg)?;
    let netbird = NetBird::from_settings(cfg);
    let pools = Pools::from_settings(cfg)?;
    let (list, peers, devices) = std::thread::scope(|scope| {
        let peers = scope.spawn(|| netbird.peers());
        let devices = scope.spawn(|| tailscale_devices(cfg));
        let list = compute.command(&[
            "server",
            "list",
            "-f",
            "json",
            "-c",
            "Name",
            "-c",
            "Status",
            "-c",
            "Properties",
            "-c",
            "Created At",
            "-c",
            "Task State",
            "-c",
            "Availability Zone",
            "-c",
            "Flavor",
        ]);
        let peers = peers.join().map_err(|_| "NetBird lookup thread failed")?;
        let devices = devices
            .join()
            .map_err(|_| "Tailscale lookup thread failed")?;
        Ok::<_, String>((list?, peers, devices))
    })?;
    // Either overlay is optional: while its API fails, the workers keep the
    // addresses they were last listed with.
    let mut failed = Vec::new();
    let peers = peers.unwrap_or_else(|error| {
        eprintln!("NetBird peers unavailable: {error}");
        failed.push("netbird");
        Vec::new()
    });
    let devices = devices.unwrap_or_else(|error| {
        eprintln!("Tailscale device list: {error}");
        failed.push("tailscale");
        HashMap::new()
    });
    let list: Vec<Value> =
        serde_json::from_slice(&list).map_err(|_| "Invalid OpenStack server listing")?;
    let peers: HashMap<_, _> = peers
        .into_iter()
        .filter_map(|peer| Some((peer["name"].as_str()?.to_owned(), peer)))
        .collect();
    let mut servers = BTreeMap::new();
    for server in &list {
        let name = setting(server, "Name")?;
        let properties = server["Properties"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        if let Some(kind) = pools.classify(name, &properties) {
            servers.insert(name, (server, kind));
        }
    }
    let images = image_labels(&compute, servers.values().map(|(server, _)| *server));
    let mut workers = Vec::new();
    for (name, (server, kind)) in servers {
        let mut worker = info(cfg, name, server, &kind, peers.get(name), devices.get(name))?;
        let image = worker["image_id"].as_str().and_then(|id| images.get(id));
        // From the worker's Glance image (set at publish time): `icon` is a
        // Nerd Font glyph for the consoles only, `ls --json` drops it.
        for key in ["icon", "label"] {
            worker[key] = image
                .and_then(|props| props.get(key))
                .cloned()
                .unwrap_or_default();
        }
        workers.push(worker);
    }
    if !failed.is_empty()
        && let Some(Value::Array(previous)) = super::inventory_cache::directory()
            .and_then(|dir| super::inventory_cache::read_last(&dir))
    {
        keep_addresses(&mut workers, &previous, &failed);
    }
    workers.extend(super::github_leases::inventory(cfg)?);
    Ok(Value::Array(workers))
}

/// `rworker_icon` and `rworker_label` of each distinct worker image,
/// looked up once per image (workers boot from volume, so their image is
/// only known from the `image_id` metadata the creator records).
fn image_labels<'a>(
    compute: &OpenStack,
    servers: impl Iterator<Item = &'a Value>,
) -> HashMap<String, serde_json::Map<String, Value>> {
    let mut ids: Vec<String> = servers
        .filter_map(|server| server["Properties"]["image_id"].as_str())
        .map(str::to_owned)
        .collect();
    ids.sort();
    ids.dedup();
    std::thread::scope(|scope| {
        let lookups: Vec<_> = ids
            .into_iter()
            .map(|id| {
                scope.spawn(move || {
                    let output = compute
                        .command(&["image", "show", "-f", "json", "-c", "properties", "--", &id])
                        .ok()?;
                    let detail: Value = serde_json::from_slice(&output).ok()?;
                    let props = &detail["properties"];
                    let mut labels = serde_json::Map::new();
                    if let Some(icon) = props["rworker_icon"].as_str().and_then(icon_glyph) {
                        labels.insert("icon".into(), json!(icon));
                    }
                    if let Some(label) = props["rworker_label"].as_str().filter(|v| !v.is_empty()) {
                        labels.insert("label".into(), json!(label));
                    }
                    Some((id, labels))
                })
            })
            .collect();
        lookups
            .into_iter()
            .filter_map(|lookup| lookup.join().ok().flatten())
            .collect()
    })
}

fn tailscale_devices(cfg: &Value) -> Result<HashMap<String, Value>, String> {
    if !cfg["tailscale"].is_object() {
        return Ok(HashMap::new());
    }
    Tailscale::from_settings(&cfg["tailscale"]).and_then(|mut client| client.devices())
}

/// Fill in the addresses of overlays that could not be asked from the
/// previous listing.
fn keep_addresses(workers: &mut [Value], previous: &[Value], failed: &[&str]) {
    for worker in workers {
        let Some(old) = previous
            .iter()
            .find(|old| old["name"] == worker["name"] && old["created_at"] == worker["created_at"])
        else {
            continue;
        };
        let mut kept = false;
        for overlay in failed {
            if let Some(ip) = old["ips"].get(overlay) {
                worker["ips"][*overlay] = ip.clone();
                kept = true;
            }
        }
        if kept {
            worker["ip"] = ["netbird", "tailscale"]
                .iter()
                .find_map(|overlay| worker["ips"].get(overlay).cloned())
                .unwrap_or_default();
            worker["connected"] = json!(worker["connected"] == true || old["connected"] == true);
        }
    }
}

fn info(
    cfg: &Value,
    name: &str,
    detail: &Value,
    kind: &Kind,
    peer: Option<&Value>,
    device: Option<&Value>,
) -> Result<Value, String> {
    let mut meta = match &detail["Properties"] {
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
    let status = if detail["Task State"] == "deleting" {
        json!("DELETING")
    } else {
        detail["Status"].clone()
    };
    let mut worker = json!({
        "name": name,
        // `adhoc` or the pool's name; `pool` is null for ad-hoc workers.
        "kind": kind.label(),
        "pool": match kind {
            Kind::Adhoc => Value::Null,
            Kind::Pool(pool) => json!(pool),
        },
        "host_key": meta.get("host_key"),
        "created_at": detail["Created At"],
        "status": status,
        "flavor": detail.get("Flavor"),
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
        "availability_zone": detail["Availability Zone"],
    });
    for key in CREATOR_KEYS {
        worker[key] = meta.get(key).cloned().unwrap_or_default();
    }
    for key in ["image_name", "image_id"] {
        if let Some(value) = meta.get(key) {
            worker[key] = value.clone();
        }
    }
    meta.remove("setup_key_id");
    meta.remove("ts_key_id");
    worker["metadata"] = Value::Object(meta);
    Ok(worker)
}

/// `rworker_icon` as a glyph: Glance only stores 3-byte UTF-8, so images
/// carry the codepoint (`U+F0508`); a literal glyph works too.
fn icon_glyph(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    match value
        .strip_prefix("U+")
        .or_else(|| value.strip_prefix("u+"))
    {
        Some(hex) => u32::from_str_radix(hex, 16)
            .ok()
            .and_then(char::from_u32)
            .map(String::from),
        None => Some(value.to_owned()),
    }
}

#[cfg(test)]
mod icon_tests {
    use super::icon_glyph;

    #[test]
    fn codepoints_and_literals() {
        assert_eq!(icon_glyph("U+F0508").as_deref(), Some("\u{f0508}"));
        assert_eq!(icon_glyph("u+f1064").as_deref(), Some("\u{f1064}"));
        assert_eq!(icon_glyph("\u{f0ad}").as_deref(), Some("\u{f0ad}"));
        assert_eq!(icon_glyph("U+ZZZZ"), None);
        assert_eq!(icon_glyph(" "), None);
    }
}

#[cfg(test)]
mod address_tests {
    use super::*;

    #[test]
    fn a_failed_overlay_keeps_the_last_known_addresses() {
        let previous = [
            json!({"name": "a", "created_at": "1", "connected": true, "ips": {"netbird": "100.64.0.1", "tailscale": "100.64.1.1"}}),
            json!({"name": "b", "created_at": "1", "connected": true, "ips": {"netbird": "100.64.0.2"}}),
        ];
        let mut workers = [
            json!({"name": "a", "created_at": "1", "connected": false, "ip": "100.64.1.9", "ips": {"tailscale": "100.64.1.9"}}),
            // Same name, but another worker.
            json!({"name": "b", "created_at": "2", "connected": false, "ip": null, "ips": {}}),
        ];
        keep_addresses(&mut workers, &previous, &["netbird"]);
        assert_eq!(workers[0]["ip"], "100.64.0.1");
        assert_eq!(
            workers[0]["ips"],
            json!({"netbird": "100.64.0.1", "tailscale": "100.64.1.9"})
        );
        assert_eq!(workers[0]["connected"], true);
        assert!(workers[1]["ip"].is_null());
    }
}
