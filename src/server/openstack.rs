use std::fs::File;
use std::process::{Command, Stdio};

use serde_json::{Map, Value};

use super::mutations::{Compute, WorkerRecord};

pub(super) struct OpenStack {
    credentials: Value,
}

impl OpenStack {
    pub fn from_settings(settings: &Value) -> Result<Self, String> {
        let path = setting(settings, "openstackSecretFile")?;
        let name = setting(settings, "openstackSecretName")?;
        let file = File::open(path).map_err(|err| format!("OpenStack credentials: {err}"))?;
        let document: Value = serde_json::from_reader(file)
            .map_err(|_| "Invalid JSON in OpenStack credentials file".to_string())?;
        let credentials = document[name]["data"].clone();
        for key in [
            "auth_url",
            "application_credential_id",
            "application_credential_secret",
            "region",
        ] {
            setting(&credentials, key)?;
        }
        Ok(Self { credentials })
    }

    pub(super) fn command(&self, args: &[&str]) -> Result<Vec<u8>, String> {
        let mut command = Command::new("openstack");
        command
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .env_remove("OS_CLOUD")
            .env_remove("OS_TOKEN")
            .env("OS_AUTH_TYPE", "v3applicationcredential");
        for (env, key) in [
            ("OS_AUTH_URL", "auth_url"),
            ("OS_APPLICATION_CREDENTIAL_ID", "application_credential_id"),
            (
                "OS_APPLICATION_CREDENTIAL_SECRET",
                "application_credential_secret",
            ),
            ("OS_REGION_NAME", "region"),
        ] {
            command.env(env, setting(&self.credentials, key)?);
        }
        let output = command
            .output()
            .map_err(|err| format!("OpenStack CLI: {err}"))?;
        if !output.status.success() {
            return Err(format!(
                "OpenStack {}: {}",
                args[..2].join(" "),
                output.status
            ));
        }
        Ok(output.stdout)
    }
}

impl OpenStack {
    pub(super) fn flavor(&self, name_or_id: &str) -> Result<Value, String> {
        let output = self.command(&["flavor", "show", "-f", "json", "--", name_or_id])?;
        serde_json::from_slice(&output).map_err(|_| "Invalid OpenStack flavor".to_string())
    }

    pub(super) fn metadata(&self, id: &str) -> Result<Map<String, Value>, String> {
        let output =
            self.command(&["server", "show", "-f", "json", "-c", "properties", "--", id])?;
        let mut detail: Value = serde_json::from_slice(&output)
            .map_err(|_| "Invalid OpenStack server detail".to_string())?;
        let metadata = match detail["properties"].take() {
            Value::Object(metadata) => metadata,
            Value::Null => Map::new(),
            _ => return Err("Invalid OpenStack server properties".into()),
        };
        Ok(metadata)
    }
}

pub(super) fn setting<'a>(settings: &'a Value, key: &str) -> Result<&'a str, String> {
    settings[key]
        .as_str()
        .ok_or_else(|| format!("Missing setting: {key}"))
}

impl Compute for OpenStack {
    type WorkerId = String;

    fn worker(&mut self, name: &str) -> Result<Option<WorkerRecord<String>>, String> {
        let output = self.command(&["server", "list", "-f", "json", "-c", "ID", "-c", "Name"])?;
        let servers: Vec<Value> = serde_json::from_slice(&output)
            .map_err(|_| "Invalid OpenStack server listing".to_string())?;
        let Some(server) = servers
            .iter()
            .rev()
            .find(|server| server["Name"].as_str() == Some(name))
        else {
            return Ok(None);
        };
        let id = setting(server, "ID")?;
        Ok(Some((id.into(), self.metadata(id)?)))
    }

    fn set_metadata(
        &mut self,
        worker: &String,
        metadata: Map<String, Value>,
    ) -> Result<(), String> {
        let mut args = vec!["server".into(), "set".into()];
        for (key, value) in metadata {
            let value = value
                .as_str()
                .ok_or_else(|| format!("Invalid metadata: {key}"))?;
            args.extend(["--property".into(), format!("{key}={value}")]);
        }
        args.extend(["--".into(), worker.clone()]);
        self.command(&args.iter().map(String::as_str).collect::<Vec<_>>())?;
        Ok(())
    }

    fn delete_metadata(&mut self, worker: &String, keys: &[&str]) -> Result<(), String> {
        let mut args = vec!["server", "unset"];
        for key in keys {
            args.extend(["--property", key]);
        }
        args.extend(["--", worker]);
        self.command(&args)?;
        Ok(())
    }
}
