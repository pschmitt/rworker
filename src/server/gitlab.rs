//! GitLab API operations for worker pipelines in a configured project.

use serde_json::{Value, json};

use super::github::RunStatus;

pub(super) struct GitLab<'a> {
    instance: &'a str,
    project_id: u64,
    token: &'a str,
}

impl<'a> GitLab<'a> {
    pub(super) fn new(instance: &'a str, project_id: u64, token: &'a str) -> Result<Self, String> {
        if instance_host(instance).is_none()
            || project_id == 0
            || token.is_empty()
            || !token.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err("Invalid GitLab instance, project ID or credentials".into());
        }
        Ok(Self {
            instance,
            project_id,
            token,
        })
    }

    pub(super) fn dispatch(&self, job_id: &str, profile: &str) -> Result<u64, String> {
        let payload = json!({
            "ref": "main",
            "inputs": {"job_id": job_id, "profile": profile}
        });
        self.request("POST", "/pipeline", Some(payload))?["id"]
            .as_u64()
            .filter(|id| *id != 0)
            .ok_or_else(|| "GitLab did not return a pipeline ID".into())
    }

    pub(super) fn status(&self, pipeline_id: u64) -> Result<RunStatus, String> {
        let pipeline = self.request("GET", &format!("/pipelines/{pipeline_id}"), None)?;
        pipeline_status(&pipeline)
    }

    pub(super) fn cancel(&self, pipeline_id: u64) -> Result<(), String> {
        self.request("POST", &format!("/pipelines/{pipeline_id}/cancel"), None)?;
        Ok(())
    }

    fn request(&self, method: &str, endpoint: &str, body: Option<Value>) -> Result<Value, String> {
        let url = format!(
            "{}/api/v4/projects/{}{endpoint}",
            self.instance, self.project_id
        );
        let authorization = format!("PRIVATE-TOKEN: {}", self.token);
        let body = body.map(|value| value.to_string());
        super::http::json_request(
            method,
            &url,
            &[&authorization, "Content-Type: application/json"],
            body.as_deref(),
        )
    }
}

pub(super) fn instance_host(instance: &str) -> Option<&str> {
    instance.strip_prefix("https://").filter(|host| {
        !host.is_empty()
            && host
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b".-:".contains(&byte))
    })
}

fn pipeline_status(pipeline: &Value) -> Result<RunStatus, String> {
    let status = pipeline["status"]
        .as_str()
        .ok_or("Invalid GitLab pipeline status")?;
    let (status, conclusion) = match status {
        "success" | "failed" | "canceled" | "skipped" => ("completed", Some(status)),
        "running" => ("in_progress", None),
        "canceling" => ("canceling", None),
        _ => ("queued", None),
    };
    Ok(RunStatus {
        status: status.into(),
        conclusion: conclusion.map(str::to_owned),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipeline_status_distinguishes_queued_running_and_terminal_states() {
        for (gitlab, status, conclusion) in [
            ("pending", "queued", None),
            ("running", "in_progress", None),
            ("canceling", "canceling", None),
            ("success", "completed", Some("success")),
            ("failed", "completed", Some("failed")),
            ("canceled", "completed", Some("canceled")),
            ("skipped", "completed", Some("skipped")),
        ] {
            let run = pipeline_status(&json!({"status": gitlab})).unwrap();
            assert_eq!(run.status, status);
            assert_eq!(run.conclusion.as_deref(), conclusion);
        }
        assert!(pipeline_status(&json!({})).is_err());
    }
}
