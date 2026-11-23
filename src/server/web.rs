//! The embedded operator console. Replies use the same commands as the CLI.

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State, ws::WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use tokio::sync::Semaphore;

mod log_stream;
mod terminal;

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
        .route(
            "/assets/agents.woff2",
            get(|| async {
                (
                    [
                        (header::CONTENT_TYPE, "font/woff2"),
                        (header::CACHE_CONTROL, "no-cache"),
                    ],
                    include_bytes!("../web/agents.woff2").as_slice(),
                )
            }),
        )
        .route(
            "/assets/favicon.svg",
            get(|| async { asset("image/svg+xml", include_str!("../web/favicon.svg")) }),
        )
        .route(
            "/assets/logo.svg",
            get(|| async { asset("image/svg+xml", include_str!("../web/logo.svg")) }),
        )
        .merge(log_stream::routes())
        .merge(terminal::routes())
        .route(
            "/assets/xterm.js",
            get(|| async {
                asset(
                    "text/javascript; charset=utf-8",
                    include_str!("../web/xterm.js"),
                )
            }),
        )
        .route(
            "/assets/xterm-fit.js",
            get(|| async {
                asset(
                    "text/javascript; charset=utf-8",
                    include_str!("../web/xterm-fit.js"),
                )
            }),
        )
        .route(
            "/assets/xterm.css",
            get(|| async { asset("text/css; charset=utf-8", include_str!("../web/xterm.css")) }),
        )
        .route("/v1/automation/workers", get(automation_workers))
        .route(
            "/v1/automation/action",
            post(automation_action).layer(DefaultBodyLimit::max(32768)),
        )
        .route("/v1/ui/session", get(session))
        .route("/v1/ui/updates", get(updates))
        .route("/v1/ui/{view}", get(list))
        .route("/v1/ui/{view}/{worker}", get(worker_view))
        .route("/v1/ui/{view}/{worker}/{job}", get(job_view))
        .route(
            "/v1/ui/action",
            post(action).layer(DefaultBodyLimit::max(32768)),
        )
}

async fn updates(
    State(dir): State<PathBuf>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !same_origin(&headers) {
        return failure((StatusCode::FORBIDDEN, "Invalid WebSocket origin."));
    }
    super::inventory_api::upgrade_inventory(dir, false, upgrade)
}

fn automation_auth(headers: &HeaderMap) -> Result<(), Failure> {
    if headers.contains_key(header::ORIGIN) {
        return Err((StatusCode::FORBIDDEN, "Use a non-browser API client."));
    }
    bearer_authorized(headers)
}

async fn automation_workers(
    State(dir): State<PathBuf>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    match automation_auth(&headers) {
        Ok(()) => super::inventory_api::upgrade_inventory(dir, true, upgrade),
        Err(error) => failure(error),
    }
}

async fn automation_action(headers: HeaderMap, body: Bytes) -> Response {
    let result: Result<Value, RunFailure> = async {
        automation_auth(&headers)?;
        let requested: Action = serde_json::from_slice(&body).map_err(|_| BAD_REQUEST)?;
        let (args, seconds) = requested.command()?;
        let output = run(args, seconds).await?;
        Ok(json!({"output": output}))
    }
    .await;
    match result {
        Ok(output) => reply(StatusCode::OK, output),
        Err(error) => run_failure(error),
    }
}

fn same_origin(headers: &HeaderMap) -> bool {
    let origin = std::env::var("RWORKER_WEB_ORIGIN").ok();
    headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .zip(origin.as_deref())
        .is_some_and(|(provided, configured)| provided == configured)
}

fn asset(content_type: &'static str, body: &'static str) -> Response {
    ([
        (header::CONTENT_TYPE, content_type),
        (header::CACHE_CONTROL, "no-cache"),
        (header::CONTENT_SECURITY_POLICY, "default-src 'none'; script-src 'self'; style-src 'self'; style-src-elem 'self' 'unsafe-inline'; font-src 'self'; img-src 'self'; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'"),
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

async fn session(headers: HeaderMap) -> Response {
    match authorized(&headers) {
        Ok(()) => reply(
            StatusCode::OK,
            json!({"data": {"signOutUrl": std::env::var("RWORKER_WEB_SIGN_OUT_URL").ok()}}),
        ),
        Err(error) => failure(error),
    }
}

fn authorized(headers: &HeaderMap) -> Result<(), Failure> {
    if let Some((expected, user)) = std::env::var("RWORKER_WEB_PROXY_USER")
        .ok()
        .zip(headers.get("remote-user"))
    {
        return if user.as_bytes() == expected.as_bytes() {
            Ok(())
        } else {
            Err((
                StatusCode::FORBIDDEN,
                "This account cannot access the console.",
            ))
        };
    }
    bearer_authorized(headers)
}

fn bearer_authorized(headers: &HeaderMap) -> Result<(), Failure> {
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

/// A failed controller command, with what it reported on stderr.
#[derive(Debug)]
pub(super) struct RunFailure(StatusCode, String);

impl From<Failure> for RunFailure {
    fn from((status, message): Failure) -> Self {
        Self(status, message.to_owned())
    }
}

fn run_failure(RunFailure(status, message): RunFailure) -> Response {
    reply(status, json!({"error": message}))
}

/// The last lines a failed command wrote to stderr, secrets masked and
/// short enough for a console message.
fn command_detail(stderr: &[u8]) -> String {
    let text = crate::cmd::log_cache::redact_text(&String::from_utf8_lossy(stderr));
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let mut detail = lines[lines.len().saturating_sub(3)..].join(" · ");
    if detail.len() > 400 {
        let mut end = 400;
        while !detail.is_char_boundary(end) {
            end -= 1;
        }
        detail.truncate(end);
        detail.push('…');
    }
    detail
}

async fn run(args: Vec<String>, seconds: u16) -> Result<String, RunFailure> {
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
            .env("RWORKER_LOCAL_RELAY", "1")
            .env_remove("SSH_CONNECTION")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|_| command_error)?;
        let stderr = child.stderr.take().ok_or(command_error)?;
        let errors = std::thread::spawn(move || {
            let mut collected = Vec::new();
            let mut limited = stderr.take(64 * 1024);
            let _ = limited.read_to_end(&mut collected);
            // Keep draining so a chatty command never blocks on a full pipe.
            let _ = std::io::copy(&mut limited.into_inner(), &mut std::io::sink());
            collected
        });
        let mut output = Vec::new();
        let result = child
            .stdout
            .take()
            .ok_or(command_error)?
            .take(MAX_OUTPUT + 1)
            .read_to_end(&mut output);
        let status = child.wait().map_err(|_| command_error)?;
        let stderr = errors.join().unwrap_or_default();
        if output.len() as u64 > MAX_OUTPUT {
            return Err(RunFailure::from((
                StatusCode::PAYLOAD_TOO_LARGE,
                "Output exceeds the console limit.",
            )));
        }
        if result.is_err() || !status.success() {
            let detail = command_detail(&stderr);
            let message = match status.code() {
                Some(124 | 137) => format!("The controller did not answer within {seconds} s."),
                _ if !detail.is_empty() => detail,
                Some(255) => "Could not reach the worker over SSH.".to_owned(),
                Some(code) => format!("The controller did not complete the request (exit {code})."),
                None => command_error.1.to_owned(),
            };
            return Err(RunFailure(StatusCode::BAD_GATEWAY, message));
        }
        Ok(String::from_utf8_lossy(&output).into_owned())
    })
    .await
    .map_err(|_| RunFailure::from(command_error))?
}

async fn read(headers: HeaderMap, command: Result<(Vec<String>, bool), Failure>) -> Response {
    let result: Result<Value, RunFailure> = async {
        authorized(&headers)?;
        let (args, structured) = command?;
        let processes = args.first().is_some_and(|arg| arg == "ps");
        let output = run(args, 20).await.map_err(|error| {
            if processes && error.0 == StatusCode::BAD_GATEWAY {
                RunFailure(
                    error.0,
                    format!(
                        "Could not read job processes over SSH from the controller: {}",
                        error.1
                    ),
                )
            } else {
                error
            }
        })?;
        if structured {
            serde_json::from_str(&output).map_err(|_| {
                RunFailure::from((StatusCode::BAD_GATEWAY, "Invalid controller response."))
            })
        } else {
            Ok(Value::String(output))
        }
    }
    .await;
    match result {
        Ok(data) => reply(StatusCode::OK, json!({"data": data})),
        Err(error) => run_failure(error),
    }
}

async fn list(
    Path(view): Path<String>,
    State(dir): State<PathBuf>,
    headers: HeaderMap,
) -> Response {
    match view.as_str() {
        "workers" => return cached_workers(&headers, dir).await,
        "jobs" => return cached_jobs(&headers, None).await,
        _ => {}
    }
    let query = match view.as_str() {
        "events" => Ok(words(&["events", "--json"])),
        "profiles" => Ok(words(&["profiles", "--json"])),
        _ => Err(NOT_FOUND),
    };
    read(headers, query.map(|args| (args, true))).await
}

async fn cached_workers(headers: &HeaderMap, dir: PathBuf) -> Response {
    if let Err(error) = authorized(headers) {
        return failure(error);
    }
    let snapshot = tokio::task::spawn_blocking(move || {
        let cached = super::inventory_cache::read(&dir, false);
        let stale = cached.is_none();
        let data = cached
            .map(|(workers, _)| workers)
            .or_else(|| super::inventory_cache::read_last(&dir))
            .unwrap_or_else(|| json!([]));
        let ctx = crate::ctx::Ctx::from_env();
        let merged = crate::cmd::history::with_retired(&ctx, &data.to_string()).ok()?;
        let rows = serde_json::from_str::<Value>(&merged).ok()?;
        Some(json!({"data": rows, "stale": stale}))
    })
    .await;
    match snapshot {
        Ok(Some(payload)) => reply(StatusCode::OK, payload),
        _ => failure((StatusCode::BAD_GATEWAY, "Could not read worker history.")),
    }
}

async fn cached_jobs(headers: &HeaderMap, worker: Option<String>) -> Response {
    if let Err(error) = authorized(headers) {
        return failure(error);
    }
    let snapshot = tokio::task::spawn_blocking(move || {
        let mut rows = std::collections::BTreeMap::new();
        let ctx = crate::ctx::Ctx::from_env();
        for (name, job) in crate::cmd::log_cache::archived_jobs(&ctx, worker.as_deref())? {
            let row = json!({"worker": name, "job": job, "state": "archived"});
            rows.insert((name, job), row);
        }
        if let Some(dir) = crate::event_history::directory() {
            for (name, jobs) in crate::event_history::jobs(&dir)? {
                if worker.as_ref().is_some_and(|filter| filter != &name) {
                    continue;
                }
                for (job, state) in jobs {
                    let row = json!({"worker": name, "job": job, "state": state});
                    rows.insert((name.clone(), job), row);
                }
            }
        }
        Ok::<_, std::io::Error>(rows.into_values().collect::<Vec<_>>())
    })
    .await;
    match snapshot {
        Ok(Ok(rows)) => reply(StatusCode::OK, json!({"data": rows})),
        _ => failure((StatusCode::BAD_GATEWAY, "Could not read the job cache.")),
    }
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
        ("processes", None) => Ok((words(&["ps", worker]), false)),
        ("builder", None) => Ok((words(&["builder", worker]), false)),
        _ => Err(NOT_FOUND),
    }
}

async fn worker_view(Path((view, worker)): Path<(String, String)>, headers: HeaderMap) -> Response {
    if view == "profiles" {
        return read(
            headers,
            name(&worker).map(|profile| (words(&["profiles", profile, "--json"]), true)),
        )
        .await;
    }
    if view == "jobs" {
        return match name(&worker) {
            Ok(_) => cached_jobs(&headers, Some(worker)).await,
            Err(error) => failure(error),
        };
    }
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
        #[serde(default)]
        flavor: Option<String>,
        #[serde(default)]
        disk: Option<u32>,
        #[serde(default)]
        scratch: bool,
        #[serde(default)]
        no_niks3_token: bool,
        #[serde(default)]
        agent_id: Option<String>,
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
    Exec {
        profile: String,
        ttl: String,
        topic: String,
        command: String,
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
                flavor,
                disk,
                scratch,
                no_niks3_token,
                agent_id,
            } => {
                let ci = crate::github_profiles::from_name(profile).is_some();
                if ci
                    && (flavor.is_some()
                        || disk.is_some()
                        || *scratch
                        || *no_niks3_token
                        || agent_id.is_some())
                {
                    return Err(BAD_REQUEST);
                }
                let mut args = words(&[
                    "up",
                    "--no-nix-cache",
                    "--no-nix-auth",
                    "--profile",
                    name(profile)?,
                    "--ttl",
                    ttl(duration)?,
                    "--topic",
                    topic(title)?,
                    "--json",
                ]);
                if let Some(flavor) = flavor {
                    args.extend(words(&["--flavor", name(flavor)?]));
                }
                if let Some(disk) = disk {
                    if *disk == 0 {
                        return Err(BAD_REQUEST);
                    }
                    args.extend(["--disk".into(), disk.to_string()]);
                }
                if *scratch {
                    args.push("--scratch".into());
                }
                if *no_niks3_token {
                    args.push("--no-niks3-token".into());
                }
                if let Some(id) = agent_id {
                    if id.is_empty()
                        || id.len() > 128
                        || !id
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
                        || id.starts_with('-')
                    {
                        return Err(BAD_REQUEST);
                    }
                    args.extend(words(&["--agent-id", id]));
                }
                return Ok((args, 900));
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
            Self::Exec {
                profile,
                ttl: duration,
                topic: title,
                command,
            } if !command.trim().is_empty() && !command.contains('\0') => {
                return Ok((
                    words(&[
                        "exec",
                        "--no-nix-cache",
                        "--no-nix-auth",
                        "--profile",
                        name(profile)?,
                        "--ttl",
                        ttl(duration)?,
                        "--topic",
                        topic(title)?,
                        "--",
                        command,
                    ]),
                    3600,
                ));
            }
            Self::RenewToken { worker } => words(&["niks3-token", name(worker)?]),
            _ => return Err(BAD_REQUEST),
        };
        Ok((args, 45))
    }
}

async fn action(headers: HeaderMap, body: Bytes) -> Response {
    let result: Result<Value, RunFailure> = async {
        authorized(&headers)?;
        if !same_origin(&headers) {
            return Err(RunFailure::from((
                StatusCode::FORBIDDEN,
                "Open the console on its configured hostname.",
            )));
        }
        if headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map(str::trim)
            != Some("application/json")
        {
            return Err(BAD_REQUEST.into());
        }
        let requested: Action = serde_json::from_slice(&body).map_err(|_| BAD_REQUEST)?;
        if matches!(requested, Action::Exec { .. }) {
            return Err(BAD_REQUEST.into());
        }
        let (args, seconds) = requested.command()?;
        let output = run(args, seconds).await?;
        let data = if matches!(requested, Action::Create { .. }) {
            let created: Value = serde_json::from_str(&output)
                .map_err(|_| (StatusCode::BAD_GATEWAY, "Invalid controller response."))?;
            let worker = created["name"]
                .as_str()
                .ok_or((StatusCode::BAD_GATEWAY, "Missing worker name."))?;
            json!({"worker": name(worker)?})
        } else {
            Value::Null
        };
        Ok(data)
    }
    .await;
    match result {
        Ok(data) => reply(StatusCode::OK, json!({"ok": true, "data": data})),
        Err(error) => run_failure(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_detail_keeps_the_last_lines_and_masks_secrets() {
        let detail = command_detail(b"starting\n\nfirst\nPASSWORD=hunter2\nssh: connect to host 100.64.0.1 port 22: Connection timed out\n");
        assert_eq!(
            detail,
            "first \u{b7} PASSWORD=[REDACTED] \u{b7} ssh: connect to host 100.64.0.1 port 22: Connection timed out"
        );
        assert!(command_detail(&b"x".repeat(1000)).ends_with('\u{2026}'));
        assert_eq!(command_detail(b"\n \n"), "");
    }

    #[test]
    fn create_accepts_openstack_overrides_and_keeps_controller_credentials_local() {
        let input = json!({
            "action": "create", "profile": "standard", "ttl": "2h", "topic": "test",
            "flavor": "s1.large", "disk": 250, "scratch": true,
            "no_niks3_token": true, "agent_id": "codex:fixture",
        });
        let args = serde_json::from_value::<Action>(input.clone())
            .unwrap()
            .command()
            .unwrap()
            .0;
        assert_eq!(
            args,
            [
                "up",
                "--no-nix-cache",
                "--no-nix-auth",
                "--profile",
                "standard",
                "--ttl",
                "2h",
                "--topic",
                "test",
                "--json",
                "--flavor",
                "s1.large",
                "--disk",
                "250",
                "--scratch",
                "--no-niks3-token",
                "--agent-id",
                "codex:fixture",
            ]
        );
        for (field, bad) in [
            ("profile", json!("gh-aarch64")),
            ("flavor", json!("--ssh")),
            ("disk", json!(0)),
            ("agent_id", json!("--json")),
        ] {
            let mut copy = input.clone();
            copy[field] = bad;
            assert!(
                serde_json::from_value::<Action>(copy)
                    .unwrap()
                    .command()
                    .is_err()
            );
        }
        let minimal = json!({
            "action": "create", "profile": "gh-aarch64", "ttl": "2h", "topic": "",
        });
        assert!(
            serde_json::from_value::<Action>(minimal)
                .unwrap()
                .command()
                .is_ok()
        );
    }

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
