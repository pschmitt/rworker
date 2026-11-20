use std::collections::HashMap;
use std::fs::File;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{http, openstack::setting};

pub(super) struct Tailscale {
    api: String,
    credentials: Value,
    tag: String,
    token: Option<(String, Instant)>,
}

impl Tailscale {
    pub fn from_settings(cfg: &Value) -> Result<Self, String> {
        let file = File::open(setting(cfg, "oauthFile")?).map_err(|err| err.to_string())?;
        let credentials = serde_json::from_reader(file)
            .map_err(|_| "Invalid Tailscale OAuth credentials JSON")?;
        setting(&credentials, "client_id")?;
        setting(&credentials, "client_secret")?;
        Ok(Self {
            api: cfg["api"]
                .as_str()
                .unwrap_or("https://api.tailscale.com/api/v2")
                .trim_end_matches('/')
                .into(),
            credentials,
            tag: setting(cfg, "tag")?.into(),
            token: None,
        })
    }

    fn access_token(&mut self) -> Result<&str, String> {
        if !self
            .token
            .as_ref()
            .is_some_and(|(_, until)| Instant::now() < *until)
        {
            let body = format!(
                "client_id={}&client_secret={}&grant_type=client_credentials",
                form(setting(&self.credentials, "client_id")?),
                form(setting(&self.credentials, "client_secret")?),
            );
            let response = http::json_request(
                "POST",
                &format!("{}/oauth/token", self.api),
                &["Content-Type: application/x-www-form-urlencoded"],
                Some(&body),
            )?;
            let lifetime = response["expires_in"]
                .as_u64()
                .or_else(|| response["expires_in"].as_str()?.parse().ok())
                .unwrap_or(3600);
            let now = Instant::now();
            let until = now
                .checked_add(Duration::from_secs(lifetime.saturating_sub(60)))
                .unwrap_or(now);
            self.token = Some((setting(&response, "access_token")?.into(), until));
        }
        self.token
            .as_ref()
            .map(|(token, _)| token.as_str())
            .ok_or_else(|| "Missing Tailscale OAuth token".into())
    }

    fn call(&mut self, method: &str, path: &str, body: Option<Value>) -> Result<Value, String> {
        let url = format!("{}/{path}", self.api);
        let authorization = format!("Authorization: Bearer {}", self.access_token()?);
        let body = body.map(|body| body.to_string());
        http::json_request(
            method,
            &url,
            &[
                &authorization,
                "Content-Type: application/json",
                "Accept: application/json",
            ],
            body.as_deref(),
        )
        .map_err(|err| format!("Tailscale: {err}"))
    }

    pub fn create_key(&mut self, name: &str) -> Result<Value, String> {
        let description: String = format!("rworker {name}").chars().take(50).collect();
        self.call(
            "POST",
            "tailnet/-/keys",
            Some(json!({
                "description": description,
                "expirySeconds": 86400,
                "capabilities": {"devices": {"create": {
                    "reusable": false,
                    "ephemeral": true,
                    "preauthorized": true,
                    "tags": [&self.tag],
                }}},
            })),
        )
    }

    pub fn devices(&mut self) -> Result<HashMap<String, Value>, String> {
        let mut response = self.call("GET", "tailnet/-/devices", None)?;
        let mut devices = HashMap::new();
        if let Value::Array(entries) = response["devices"].take() {
            for device in entries {
                if !device["tags"]
                    .as_array()
                    .is_some_and(|tags| tags.iter().any(|tag| tag.as_str() == Some(&self.tag)))
                {
                    continue;
                }
                let name = device["hostname"]
                    .as_str()
                    .filter(|name| !name.is_empty())
                    .or_else(|| device["name"].as_str()?.split('.').next())
                    .ok_or_else(|| "Missing Tailscale device name".to_string())?;
                devices.insert(name.into(), device);
            }
        }
        Ok(devices)
    }

    pub fn delete_key(&mut self, id: &str) {
        if let Err(err) = self.call("DELETE", &format!("tailnet/-/keys/{id}"), None) {
            eprintln!("deleting Tailscale key {id}: {err}");
        }
    }

    pub fn delete_device(&mut self, name: &str) -> Result<(), String> {
        if let Some(device) = self.devices()?.get(name) {
            let id = setting(device, "id")?;
            eprintln!("deleting Tailscale device {name}");
            self.call("DELETE", &format!("device/{id}"), None)?;
        }
        Ok(())
    }
}

fn form(value: &str) -> String {
    let mut output = String::new();
    for byte in value.bytes() {
        match byte {
            b' ' => output.push('+'),
            b'-' | b'_' | b'.' | b'~' => output.push(char::from(byte)),
            byte if byte.is_ascii_alphanumeric() => output.push(char::from(byte)),
            _ => output.push_str(&format!("%{byte:02X}")),
        }
    }
    output
}
