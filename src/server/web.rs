//! The embedded operator console. Replies use the same commands as the CLI.

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use tokio::sync::Semaphore;

const MAX_OUTPUT: u64 = 2 * 1024 * 1024;
static REQUESTS: Semaphore = Semaphore::const_new(8);

type Failure = (StatusCode, &'static str);
const BAD_REQUEST: Failure = (StatusCode::BAD_REQUEST, "Invalid request.");
const NOT_FOUND: Failure = (StatusCode::NOT_FOUND, "Unknown view.");

pub(super) fn routes(router: Router<PathBuf>) -> Router<PathBuf> {
    if std::env::var_os("RWORKER_WEB_TOKEN_FILE").is_none() {
        return router;
    }
    router
        .route(
            "/",
            get(|| async {
                asset(
                    "text/html; charset=utf-8",
                    include_str!("../web/index.html"),
                )
            }),
        )
        .route(
            "/assets/app.css",
            get(|| async { asset("text/css; charset=utf-8", include_str!("../web/app.css")) }),
        )
        .route(
            "/assets/app.js",
            get(|| async {
                asset(
                    "text/javascript; charset=utf-8",
                    include_str!("../web/app.js"),
                )
            }),
        )
        .route("/v1/ui/updates", get(updates))
        .route("/v1/ui/{view}", get(list))
        .route("/v1/ui/{view}/{worker}", get(worker_view))
        .route("/v1/ui/{view}/{worker}/{job}", get(job_view))
        .route(
            "/v1/ui/action",
            post(action).layer(DefaultBodyLimit::max(32768)),
        )
}

async fn updates(State(dir): State<PathBuf>) -> Response {
    super::inventory_api::inventory_events(dir, false).await
}

fn asset(content_type: &'static str, body: &'static str) -> Response {
    ([
        (header::CONTENT_TYPE, content_type),
        (header::CACHE_CONTROL, "no-cache"),
        (header::CONTENT_SECURITY_POLICY, "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::REFERRER_POLICY, "no-referrer"),
    ], body).into_response()
}

fn reply(status: StatusCode, data: Value) -> Response {
    (
        status,
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        data.to_string(),
    )
        .into_response()
}

fn failure((status, message): Failure) -> Response {
    reply(status, json!({"error": message}))
}

fn authorized(headers: &HeaderMap) -> Result<(), Failure> {
    let Some(path) = std::env::var_os("RWORKER_WEB_TOKEN_FILE") else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Console access is unavailable.",
        ));
    };
    let expected = std::fs::read_to_string(path).map_err(|_| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "Console access is unavailable.",
        )
    })?;
    let expected = expected.trim().as_bytes();
    let given = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.as_bytes().strip_prefix(b"Bearer "))
        .unwrap_or_default();
    if expected.len() < 32 {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Console access is unavailable.",
        ));
    }
    if expected.len() != given.len()
        || expected
            .iter()
            .zip(given)
            .fold(0, |diff, (a, b)| diff | (a ^ b))
            != 0
    {
        return Err((StatusCode::UNAUTHORIZED, "Enter the console access token."));
    }
    Ok(())
}

fn name(value: &str) -> Result<&str, Failure> {
    if value.len() <= 128
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        Ok(value)
    } else {
        Err(BAD_REQUEST)
    }
}

fn words(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_owned()).collect()
}

async fn run(args: Vec<String>, seconds: u16) -> Result<String, Failure> {
    let permit = REQUESTS.try_acquire().map_err(|_| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "Console is busy. Try again shortly.",
        )
    })?;
    let command_error = (
        StatusCode::BAD_GATEWAY,
        "The controller did not complete the request.",
    );
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let exe = std::env::current_exe().map_err(|_| command_error)?;
        let mut child = Command::new("timeout")
            .args(["--kill-after=5s", &format!("{seconds}s")])
            .arg(exe)
            .args(args)
            .env("NO_COLOR", "1")
            .env_remove("SSH_CONNECTION")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| command_error)?;
        let mut output = Vec::new();
        let result = child
            .stdout
            .take()
            .ok_or(command_error)?
            .take(MAX_OUTPUT + 1)
            .read_to_end(&mut output);
        let status = child.wait().map_err(|_| command_error)?;
        if output.len() as u64 > MAX_OUTPUT {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                "Output exceeds the console limit.",
            ));
        }
        if result.is_err() || !status.success() {
            return Err(command_error);
        }
        Ok(String::from_utf8_lossy(&output).into_owned())
    })
    .await
    .map_err(|_| command_error)?
}

async fn read(headers: HeaderMap, command: Result<(Vec<String>, bool), Failure>) -> Response {
    let result = async {
        authorized(&headers)?;
        let (args, structured) = command?;
        let output = run(args, 20).await?;
        if structured {
            serde_json::from_str(&output)
                .map_err(|_| (StatusCode::BAD_GATEWAY, "Invalid controller response."))
        } else {
            Ok(Value::String(output))
        }
    }
    .await;
    match result {
        Ok(data) => reply(StatusCode::OK, json!({"data": data})),
        Err(error) => failure(error),
    }
}

async fn list(Path(view): Path<String>, headers: HeaderMap) -> Response {
    let query = match view.as_str() {
        "workers" => Ok(words(&["machines", "--all", "--json"])),
        "jobs" => Ok(words(&["jobs", "--json"])),
        "events" => Ok(words(&["events", "--json"])),
        "profiles" => Ok(words(&["profiles", "--json"])),
        _ => Err(NOT_FOUND),
    };
    read(headers, query.map(|args| (args, true))).await
}

fn worker_command(
    view: &str,
    worker: &str,
    job: Option<&str>,
) -> Result<(Vec<String>, bool), Failure> {
    let worker = name(worker)?;
    if matches!(view, "logs" | "archived-logs" | "raw-logs" | "raw-archive") {
        let mut args = words(&["logs", worker, "--lines", "200"]);
        if let Some(job) = job {
            args.push(name(job)?.into());
        }
        if matches!(view, "archived-logs" | "raw-archive") {
            args.push("--cached".into());
        }
        if matches!(view, "logs" | "archived-logs") {
            args.push("--redact".into());
        }
        return Ok((args, false));
    }
    match (view, job) {
        ("inspect", None) => Ok((words(&["inspect", worker, "--json"]), true)),
        ("boot", None) => Ok((words(&["inspect", worker, "--boot", "--json"]), true)),
        ("jobs", None) => Ok((words(&["jobs", worker, "--json"]), true)),
        ("processes", None) => Ok((words(&["ps", worker]), false)),
        ("builder", None) => Ok((words(&["builder", worker]), false)),
        _ => Err(NOT_FOUND),
    }
}

async fn worker_view(Path((view, worker)): Path<(String, String)>, headers: HeaderMap) -> Response {
    read(headers, worker_command(&view, &worker, None)).await
}

async fn job_view(
    Path((view, worker, job)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    read(headers, worker_command(&view, &worker, Some(&job))).await
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "kebab-case", deny_unknown_fields)]
enum Action {
    Create {
        profile: String,
        ttl: String,
        topic: String,
    },
    Extend {
        worker: String,
        ttl: String,
    },
    Topic {
        worker: String,
        topic: String,
    },
    Claim {
        worker: String,
        topic: String,
    },
    Delete {
        worker: String,
    },
    Start {
        worker: String,
        job: String,
        command: String,
    },
    Register {
        worker: String,
        job: String,
        path: String,
    },
    RenewToken {
        worker: String,
    },
}

fn ttl(value: &str) -> Result<&str, Failure> {
    if !value.is_empty()
        && value.len() <= 16
        && value.as_bytes()[0].is_ascii_digit()
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || b"hm".contains(&b))
    {
        Ok(value)
    } else {
        Err(BAD_REQUEST)
    }
}

fn topic(value: &str) -> Result<&str, Failure> {
    if value.len() <= 255 && !value.starts_with('-') && !value.contains(['\0', '\r', '\n']) {
        Ok(value)
    } else {
        Err(BAD_REQUEST)
    }
}

impl Action {
    fn command(&self) -> Result<(Vec<String>, u16), Failure> {
        let args = match self {
            Self::Create {
                profile,
                ttl: duration,
                topic: title,
            } => {
                return Ok((
                    words(&[
                        "up",
                        "--no-nix-cache",
                        "--profile",
                        name(profile)?,
                        "--ttl",
                        ttl(duration)?,
                        "--topic",
                        topic(title)?,
                        "--json",
                    ]),
                    900,
                ));
            }
            Self::Extend {
                worker,
                ttl: duration,
            } => words(&["extend", name(worker)?, "--ttl", ttl(duration)?]),
            Self::Topic {
                worker,
                topic: title,
            } => words(&["topic", name(worker)?, topic(title)?]),
            Self::Claim {
                worker,
                topic: title,
            } => words(&["claim", name(worker)?, "--topic", topic(title)?]),
            Self::Delete { worker } => words(&["down", name(worker)?]),
            Self::Start {
                worker,
                job,
                command,
            } if !command.trim().is_empty() && !command.contains('\0') => {
                words(&["start", name(worker)?, "--name", name(job)?, "--", command])
            }
            Self::Register { worker, job, path }
                if path.starts_with('/') && !path.contains('\0') =>
            {
                words(&["register", name(worker)?, name(job)?, "--log", path])
            }
            Self::RenewToken { worker } => words(&["niks3-token", name(worker)?]),
            _ => return Err(BAD_REQUEST),
        };
        Ok((args, 45))
    }
}

async fn action(headers: HeaderMap, body: Bytes) -> Response {
    let result = async {
        authorized(&headers)?;
        let origin = std::env::var("RWORKER_WEB_ORIGIN").ok();
        if !headers
            .get(header::ORIGIN)
            .and_then(|header| header.to_str().ok())
            .zip(origin.as_deref())
            .is_some_and(|(provided, configured)| provided == configured)
        {
            return Err((
                StatusCode::FORBIDDEN,
                "Open the console on its configured hostname.",
            ));
        }
        if headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map(str::trim)
            != Some("application/json")
        {
            return Err(BAD_REQUEST);
        }
        let requested: Action = serde_json::from_slice(&body).map_err(|_| BAD_REQUEST)?;
        let (args, seconds) = requested.command()?;
        run(args, seconds).await?;
        Ok(())
    }
    .await;
    match result {
        Ok(()) => reply(StatusCode::OK, json!({"ok": true})),
        Err(error) => failure(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_control_fields_and_preserves_job_command() {
        assert!(worker_command("logs", "../secret", None).is_err());
        assert!(
            Action::Topic {
                worker: "example".into(),
                topic: "--force".into()
            }
            .command()
            .is_err()
        );
        let action = Action::Start {
            worker: "example".into(),
            job: "build".into(),
            command: "printf 'hello world'".into(),
        };
        assert_eq!(
            action.command().unwrap().0,
            [
                "start",
                "example",
                "--name",
                "build",
                "--",
                "printf 'hello world'"
            ]
        );
    }
}
