use std::fs;

use serde_json::{Value, json};

use super::{http, openstack::setting};

pub(super) struct NetBird {
    api: String,
    token: String,
}

impl NetBird {
    pub fn from_settings(config: &Value) -> Result<Self, String> {
        let api = setting(config, "netbirdApi")?.trim_end_matches('/').into();
        let token = fs::read_to_string(setting(config, "netbirdTokenFile")?)
            .map_err(|error| format!("Read NetBird token: {error}"))?;
        Ok(Self {
            api,
            token: token.trim().into(),
        })
    }

    fn call(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value, String> {
        let authorization = format!("Authorization: Token {}", self.token);
        let body = body.map(|body| body.to_string());
        http::json_request(
            method,
            &format!("{}/{path}", self.api),
            &[
                &authorization,
                "Content-Type: application/json",
                "Accept: application/json",
            ],
            body.as_deref(),
        )
        .map_err(|error| format!("NetBird: {error}"))
    }

    pub fn peers(&self) -> Result<Vec<Value>, String> {
        serde_json::from_value(self.call("GET", "peers", None)?)
            .map_err(|_| "Invalid NetBird peers response".into())
    }

    pub fn group(&self, name: &str) -> Result<Value, String> {
        let groups: Vec<Value> = serde_json::from_value(self.call("GET", "groups", None)?)
            .map_err(|_| "Invalid NetBird groups response")?;
        groups
            .into_iter()
            .find(|group| group["name"].as_str() == Some(name))
            .ok_or_else(|| format!("NetBird group {name:?} not found"))
    }

    pub fn ensure_member(&self, group_name: &str, peer_name: &str) -> Result<(), String> {
        let group = self.group(group_name)?;
        let Some(peer) = self
            .peers()?
            .into_iter()
            .find(|peer| peer["name"] == peer_name)
        else {
            return Ok(());
        };
        let peer_id = setting(&peer, "id")?;
        let mut ids = group["peers"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|peer| setting(peer, "id"))
            .collect::<Result<Vec<_>, _>>()?;
        if ids.contains(&peer_id) {
            return Ok(());
        }
        ids.push(peer_id);
        let resources: Vec<_> = group["resources"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|resource| json!({"id": resource["id"], "type": resource["type"]}))
            .collect();
        eprintln!("adding {peer_name} to NetBird group {group_name}");
        self.call(
            "PUT",
            &format!("groups/{}", setting(&group, "id")?),
            Some(json!({
                "name": group["name"], "peers": ids, "resources": resources,
            })),
        )?;
        Ok(())
    }

    pub fn create_key(&self, name: &str, group: &str) -> Result<Value, String> {
        self.create_ephemeral_key(name, group, 86400)
    }

    pub fn create_ephemeral_key(&self, name: &str, group: &str, ttl: u64) -> Result<Value, String> {
        self.call(
            "POST",
            "setup-keys",
            Some(json!({
                "name": name, "type": "one-off", "expires_in": ttl,
                "auto_groups": [group], "usage_limit": 1, "ephemeral": true,
            })),
        )
    }

    pub fn delete_key(&self, id: &str) {
        if let Err(error) = self.call("DELETE", &format!("setup-keys/{id}"), None) {
            eprintln!("deleting setup key {id}: {error}");
        }
    }

    pub fn delete_peer(&self, name: &str) -> Result<(), String> {
        if let Some(peer) = self
            .peers()?
            .into_iter()
            .find(|peer| peer["name"].as_str() == Some(name))
        {
            let id = setting(&peer, "id")?;
            self.call("DELETE", &format!("peers/{id}"), None)?;
        }
        Ok(())
    }
}
