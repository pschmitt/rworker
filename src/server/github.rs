//! GitHub Actions operations for workers in the configured repository.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const WORKFLOW: &str = "rworker-job.yaml";

#[derive(Deserialize, PartialEq, Serialize)]
pub(super) struct RunStatus {
    pub(super) status: String,
    pub(super) conclusion: Option<String>,
}

pub(super) struct GitHub<'a> {
    repository: &'a str,
    token: &'a str,
}

impl<'a> GitHub<'a> {
    pub(super) fn new(repository: &'a str, token: &'a str) -> Result<Self, String> {
        let valid_component = |part: &str| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
        };
        let valid_repository = repository
            .split_once('/')
            .is_some_and(|(owner, name)| valid_component(owner) && valid_component(name));
        if !valid_repository
            || token.is_empty()
            || !token.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err("Invalid GitHub Actions credentials or repository".into());
        }
        Ok(Self { repository, token })
    }

    pub(super) fn dispatch(&self, job_id: &str, profile: &str) -> Result<u64, String> {
        let payload = json!({
            "ref": "main",
            "return_run_details": true,
            "inputs": {"job_id": job_id, "profile": profile}
        });
        self.request(
            "POST",
            &format!("/actions/workflows/{WORKFLOW}/dispatches"),
            Some(payload),
        )?["workflow_run_id"]
            .as_u64()
            .filter(|id| *id != 0)
            .ok_or_else(|| "GitHub dispatch did not return a workflow run ID".into())
    }

    pub(super) fn status(&self, run_id: u64) -> Result<RunStatus, String> {
        serde_json::from_value(self.request("GET", &format!("/actions/runs/{run_id}"), None)?)
            .map_err(|_| "Invalid GitHub workflow run status".into())
    }

    pub(super) fn job_id(&self, run_id: u64) -> Option<u64> {
        let response = self
            .request(
                "GET",
                &format!("/actions/runs/{run_id}/attempts/1/jobs"),
                None,
            )
            .ok()?;
        response["jobs"]
            .as_array()?
            .iter()
            .find(|job| job["name"] == "build")?["id"]
            .as_u64()
    }

    pub(super) fn cancel(&self, run_id: u64) -> Result<(), String> {
        self.request("POST", &format!("/actions/runs/{run_id}/cancel"), None)?;
        Ok(())
    }

    pub(super) fn public_repository(&self) -> Option<bool> {
        self.request("GET", "", None).ok()?["private"]
            .as_bool()
            .map(|private| !private)
    }

    fn request(&self, method: &str, endpoint: &str, body: Option<Value>) -> Result<Value, String> {
        let url = format!("https://api.github.com/repos/{}{endpoint}", self.repository);
        let authorization = format!("Authorization: Bearer {}", self.token);
        let body = body.map(|value| value.to_string());
        super::http::json_request(
            method,
            &url,
            &[
                &authorization,
                "Accept: application/vnd.github+json",
                "Content-Type: application/json",
                "X-GitHub-Api-Version: 2026-03-10",
            ],
            body.as_deref(),
        )
    }
}
