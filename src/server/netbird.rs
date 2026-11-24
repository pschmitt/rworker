use std::fs;

use serde_json::{Value, json};

use super::{http, openstack::setting};

/// The NetBird management API. It is optional: without `netbirdApi` there are
/// no peers and nothing to clean up, and only creating a key fails. An
/// unreadable token fails each call like an outage, not the whole command.
pub(super) struct NetBird(Option<Result<Api, String>>);

struct Api {
    url: String,
    token: String,
}

impl NetBird {
    pub fn from_settings(config: &Value) -> Self {
        let Some(url) = config["netbirdApi"].as_str().filter(|url| !url.is_empty()) else {
            return Self(None);
        };
        Self(Some(setting(config, "netbirdTokenFile").and_then(|file| {
            let token = fs::read_to_string(file)
                .map_err(|error| format!("Read NetBird token: {error}"))?;
            Ok(Api {
                url: url.trim_end_matches('/').into(),
                token: token.trim().into(),
            })
        })))
    }

    pub fn configured(&self) -> bool {
        self.0.is_some()
    }

    fn call(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value, String> {
        let api = self
            .0
            .as_ref()
            .ok_or("NetBird is not configured")?
            .as_ref()
            .map_err(Clone::clone)?;
        let authorization = format!("Authorization: Token {}", api.token);
        let body = body.map(|body| body.to_string());
        http::json_request(
            method,
            &format!("{}/{path}", api.url),
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
        if !self.configured() {
            return Ok(Vec::new());
        }
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
        if !self.configured() {
            return;
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unconfigured_netbird_has_no_peers_and_cannot_enroll() {
        let netbird = NetBird::from_settings(&json!({"netbirdApi": null}));
        assert!(!netbird.configured());
        assert!(netbird.peers().unwrap().is_empty());
        netbird.delete_peer("adhoc-1").unwrap();
        assert!(netbird.create_key("adhoc-1", "group").is_err());
    }

    #[test]
    fn an_unreadable_token_fails_calls_only() {
        let netbird = NetBird::from_settings(&json!({
            "netbirdApi": "http://127.0.0.1:1", "netbirdTokenFile": "/nonexistent/token",
        }));
        assert!(netbird.configured());
        assert!(netbird.peers().unwrap_err().contains("Read NetBird token"));
    }
}
