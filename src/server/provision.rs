use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use serde_json::{Map, Value};

use super::{
    netbird::NetBird,
    openstack::{OpenStack, setting},
    tailscale::Tailscale,
};

pub(super) struct Request<'a> {
    pub name: &'a str,
    pub group: &'a str,
    pub flavor: &'a str,
    pub disk: i64,
    pub profile: &'a str,
    pub metadata: Map<String, Value>,
    pub user_data: Map<String, Value>,
}

struct UserData(PathBuf);

impl Drop for UserData {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

// Shares image discovery across profiles within a single request.
#[derive(Default)]
pub(super) struct Images {
    ids: HashMap<String, Vec<String>>,
    details: HashMap<String, Value>,
}

impl Images {
    pub(super) fn select(
        &mut self,
        compute: &OpenStack,
        profile_name: &str,
        profile: &Value,
    ) -> Result<Option<(String, String, String)>, String> {
        let tags = profile["imageTags"]
            .as_array()
            .filter(|tags| !tags.is_empty())
            .ok_or_else(|| format!("unknown worker profile: {}", profile_name))?;
        let image_name_pattern = profile
            .get("imageNamePattern")
            .map(|pattern| {
                let pattern = pattern
                    .as_str()
                    .ok_or("Expected imageNamePattern to be a string")?;
                regex::Regex::new(pattern)
                    .map_err(|error| format!("Invalid imageNamePattern: {error}"))
            })
            .transpose()?;
        let mut newest = None;
        for tag in tags {
            let tag = tag.as_str().ok_or("Invalid image tag")?;
            if !self.ids.contains_key(tag) {
                let output = compute.command(&[
                    "image",
                    "list",
                    "--private",
                    "--status",
                    "active",
                    "--tag",
                    tag,
                    "-f",
                    "json",
                ])?;
                let images: Vec<Value> =
                    serde_json::from_slice(&output).map_err(|_| "Invalid image listing")?;
                let ids = images
                    .iter()
                    .map(|image| setting(image, "ID").map(str::to_owned))
                    .collect::<Result<Vec<_>, _>>()?;
                self.ids.insert(tag.to_owned(), ids);
            }
            for id in &self.ids[tag] {
                if !self.details.contains_key(id) {
                    let output = compute.command(&["image", "show", "-f", "json", "--", id])?;
                    let detail =
                        serde_json::from_slice(&output).map_err(|_| "Invalid image detail")?;
                    self.details.insert(id.clone(), detail);
                }
                let detail = &self.details[id];
                let name = setting(detail, "name")?;
                if image_name_pattern
                    .as_ref()
                    .is_some_and(|pattern| !pattern.is_match(name))
                {
                    continue;
                }
                let created = setting(detail, "created_at")?;
                if newest
                    .as_ref()
                    .is_none_or(|(date, _, _): &(String, String, String)| created > date.as_str())
                {
                    newest = Some((created.into(), id.clone(), name.into()));
                }
            }
        }
        Ok(newest)
    }
}

pub(super) fn create(
    cfg: &Value,
    compute: &OpenStack,
    request: Request<'_>,
) -> Result<(String, Map<String, Value>), String> {
    let network = setting(cfg, "networkId")?;
    let security = setting(cfg, "securityGroup")?;
    let flavor = compute.flavor(request.flavor)?;
    let flavor_id = setting(&flavor, "id")?;
    let output = compute.command(&["limits", "show", "--absolute", "-f", "json"])?;
    let limits: Vec<Value> =
        serde_json::from_slice(&output).map_err(|_| "Invalid OpenStack limits")?;
    let limit = |name: &str| {
        limits
            .iter()
            .find(|row| row["Name"] == name)
            .and_then(|row| {
                row["Value"]
                    .as_i64()
                    .or_else(|| row["Value"].as_str()?.parse().ok())
            })
            .ok_or_else(|| format!("Missing OpenStack limit: {name}"))
    };
    for (total, used, needed) in [
        (
            "max_total_cores",
            "total_cores_used",
            flavor["vcpus"].as_i64().ok_or("Invalid flavor cores")?,
        ),
        (
            "max_total_ram_size",
            "total_ram_used",
            flavor["ram"].as_i64().ok_or("Invalid flavor RAM")?,
        ),
        ("max_total_instances", "total_instances_used", 1),
    ] {
        if limit(total)?.saturating_sub(limit(used)?) < needed {
            return Err(format!("no quota left for a {} worker", request.flavor));
        }
    }

    let mut images = Images::default();
    let Some((_, image_id, image_name)) =
        images.select(compute, request.profile, &cfg["profiles"][request.profile])?
    else {
        return Err(format!(
            "no active image for worker profile {}",
            request.profile
        ));
    };

    let netbird = NetBird::from_settings(cfg)?;
    // NetBird is preferred, not required: when its API fails the worker is
    // enrolled with Tailscale alone, and the lease only fails if no overlay
    // can enroll it.
    let netbird_key = netbird
        .group(request.group)
        .and_then(|group| {
            netbird.create_key(
                &format!("nixbot-worker {}", request.name),
                setting(&group, "id")?,
            )
        })
        .and_then(|key| {
            Ok((
                setting(&key, "id")?.to_owned(),
                setting(&key, "key")?.to_owned(),
            ))
        });
    if let Err(error) = &netbird_key {
        eprintln!("no NetBird key for {}: {error}", request.name);
    }
    let mut tailscale = if cfg["tailscale"].is_object() {
        Tailscale::from_settings(&cfg["tailscale"])
            .map_err(|error| eprintln!("Tailscale disabled: {error}"))
            .ok()
    } else {
        None
    };
    let mut ts_id = None;
    let result = (|| {
        let mut data = request.user_data;
        let mut metadata = request.metadata;
        metadata.insert("image_name".into(), Value::String(image_name.clone()));
        metadata.insert("image_id".into(), Value::String(image_id.clone()));
        if let Ok((key_id, key)) = &netbird_key {
            data.insert("netbird_setup_key".into(), Value::String(key.clone()));
            metadata.insert("setup_key_id".into(), Value::String(key_id.clone()));
        }
        if let Some(client) = &mut tailscale {
            match client.create_key(request.name) {
                Ok(key) => {
                    ts_id = key["id"].as_str().map(str::to_owned);
                    if let Some(id) = &ts_id {
                        metadata.insert("ts_key_id".into(), Value::String(id.clone()));
                    }
                    if let Some(key) = key["key"].as_str() {
                        data.insert("tailscale_auth_key".into(), Value::String(key.into()));
                    }
                }
                Err(error) => eprintln!("no Tailscale key for {}: {error}", request.name),
            }
        }
        if let Err(error) = &netbird_key {
            if !data.contains_key("tailscale_auth_key") {
                return Err(format!(
                    "no overlay network can enroll {}: NetBird: {error}; Tailscale: not available",
                    request.name
                ));
            }
        }
        let suffix = super::niks3::token_id().map_err(|err| err.to_string())?;
        let path = std::env::temp_dir().join(format!("rworker-user-data-{suffix}"));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|err| err.to_string())?;
        let input = UserData(path);
        file.write_all(Value::Object(data).to_string().as_bytes())
            .map_err(|err| err.to_string())?;
        drop(file);
        let path = input.0.to_str().ok_or("Invalid user data path")?;
        let block_device = format!(
            "uuid={image_id},source_type=image,destination_type=volume,volume_size={},boot_index=0,delete_on_termination=true",
            request.disk
        );
        let mut args = vec![
            "server",
            "create",
            "-f",
            "json",
            "--flavor",
            flavor_id,
            "--block-device",
            &block_device,
            "--network",
            network,
            "--security-group",
            security,
            "--user-data",
            path,
        ];
        let properties: Vec<String> = metadata
            .iter()
            .map(|(key, value)| {
                let value = value
                    .as_str()
                    .ok_or_else(|| format!("Invalid metadata: {key}"))?;
                Ok(format!("{key}={value}"))
            })
            .collect::<Result<_, String>>()?;
        for property in &properties {
            args.extend(["--property", property]);
        }
        args.extend(["--", request.name]);
        eprintln!(
            "creating {} ({}) from image {image_name}",
            request.name, request.flavor
        );
        let output = compute.command(&args)?;
        let server: Value =
            serde_json::from_slice(&output).map_err(|_| "Invalid created server response")?;
        Ok((setting(&server, "id")?.to_owned(), metadata))
    })();
    if result.is_err() {
        if let Ok((key_id, _)) = &netbird_key {
            netbird.delete_key(key_id);
        }
        if let (Some(client), Some(id)) = (&mut tailscale, ts_id) {
            client.delete_key(&id);
        }
    }
    result
}
