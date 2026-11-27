use super::{Failure, authorized, failure, run, same_origin, worker_command};
use axum::Router;
use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Child;
use tokio::sync::Semaphore;
use tokio::time::timeout;

static STREAMS: Semaphore = Semaphore::const_new(8);

pub(super) fn routes() -> Router<PathBuf> {
    Router::new()
        .route("/v1/ui/log-filter-check", get(validate_filters))
        .route("/v1/ui/follow/{view}", get(all))
        .route("/v1/ui/follow/{view}/{worker}", get(worker))
        .route("/v1/ui/follow/{view}/{worker}/{job}", get(job))
}

async fn validate_filters(State(dir): State<PathBuf>, uri: Uri, headers: HeaderMap) -> Response {
    if let Err(error) = authorized(&headers) {
        return failure(error);
    }
    if let Err(error) = all_args("logs", uri.query()) {
        return failure(error);
    }
    let Ok(filters) = decode_filters(uri.query()) else {
        return failure(super::BAD_REQUEST);
    };
    let snapshot = tokio::task::spawn_blocking(move || {
        let workers = super::super::inventory_cache::read(&dir, false)
            .map(|(workers, _)| workers)
            .or_else(|| super::super::inventory_cache::read_last(&dir))?;
        matching_workers(&filters, &workers)
    })
    .await;
    match snapshot {
        Ok(Some(names)) => super::reply(StatusCode::OK, matching_workers_response(names)),
        _ => failure((StatusCode::BAD_GATEWAY, "Worker inventory unavailable.")),
    }
}

fn matching_workers_response(names: Vec<String>) -> serde_json::Value {
    serde_json::json!({"data": {"matching_workers": names}})
}

fn matching_workers(filters: &LogFilters, workers: &serde_json::Value) -> Option<Vec<String>> {
    let ctx = crate::ctx::Ctx::from_env();
    ctx.filter_agent_id.replace(filters.agent_id.clone());
    let filtered =
        crate::cmd::ls::filter_workers(&ctx, &workers.to_string(), &filters.topic, &filters.agent)
            .ok()?;
    let rows: Vec<serde_json::Value> = serde_json::from_str(&filtered).ok()?;
    Some(
        rows.into_iter()
            .filter_map(|row| row["name"].as_str().map(str::to_owned))
            .collect(),
    )
}

/// Every job on every worker, like `rworker logs -f`.
async fn all(
    Path(view): Path<String>,
    uri: Uri,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    match all_args(&view, uri.query()) {
        Ok(args) => serve(&headers, upgrade, args, None),
        Err(error) => failure(error),
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct LogFilters {
    topic: String,
    agent: String,
    agent_id: String,
}

fn all_args(view: &str, query: Option<&str>) -> Result<Vec<String>, Failure> {
    if !matches!(view, "logs" | "raw-logs") {
        return Err(super::NOT_FOUND);
    }
    let filters = decode_filters(query)?;
    let mut args = super::words(&["logs", "--follow", "--no-events", "--lines", "50"]);
    for (flag, pattern) in [
        ("--topic", filters.topic),
        ("--agent", filters.agent),
        ("--agent-id", filters.agent_id),
    ] {
        if !pattern.is_empty() {
            if pattern.len() > 256
                || regex::Regex::new(pattern.strip_prefix('!').unwrap_or(&pattern)).is_err()
            {
                return Err(super::BAD_REQUEST);
            }
            args.extend([flag.to_string(), pattern]);
        }
    }
    if view == "logs" {
        args.push("--redact".into());
    }
    Ok(args)
}

fn decode_filters(query: Option<&str>) -> Result<LogFilters, Failure> {
    match query {
        None => Ok(LogFilters::default()),
        Some(query) => {
            let encoded = query
                .strip_prefix("filters=")
                .filter(|s| s.len() <= 4096)
                .ok_or(super::BAD_REQUEST)?;
            let data = URL_SAFE_NO_PAD
                .decode(encoded)
                .map_err(|_| super::BAD_REQUEST)?;
            serde_json::from_slice(&data).map_err(|_| super::BAD_REQUEST)
        }
    }
}

async fn worker(
    Path((view, worker)): Path<(String, String)>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    follow(&headers, upgrade, &view, &worker, None)
}

async fn job(
    Path((view, worker, job)): Path<(String, String, String)>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    follow(&headers, upgrade, &view, &worker, Some(&job))
}

fn follow(
    headers: &HeaderMap,
    upgrade: WebSocketUpgrade,
    view: &str,
    worker: &str,
    job: Option<&str>,
) -> Response {
    let args = match log_args(view, worker, job) {
        Ok(args) => args,
        Err(error) => return failure(error),
    };
    let archived_view = if view == "logs" {
        "archived-logs"
    } else {
        "raw-archive"
    };
    let archive = match worker_command(archived_view, worker, job) {
        Ok((args, _)) => args,
        Err(error) => return failure(error),
    };
    serve(headers, upgrade, args, Some(archive))
}

/// Stream ARGS' output to the socket; without output, fall back to polling
/// the ARCHIVE command when there is one.
fn serve(
    headers: &HeaderMap,
    upgrade: WebSocketUpgrade,
    args: Vec<String>,
    archive: Option<Vec<String>>,
) -> Response {
    if !same_origin(headers) {
        return failure((StatusCode::FORBIDDEN, "Invalid WebSocket origin."));
    }
    let check = authorized(headers);
    if let Err(error) = check
        && error.0 != StatusCode::UNAUTHORIZED
    {
        return failure(error);
    }
    let Ok(permit) = STREAMS.try_acquire() else {
        return failure((StatusCode::SERVICE_UNAVAILABLE, "Too many log streams."));
    };
    upgrade
        .max_message_size(1024)
        .max_frame_size(1024)
        .on_upgrade(move |mut socket| async move {
            let _permit = permit;
            if check.is_err() && !authenticate(&mut socket).await {
                return;
            }
            if matches!(stream(&mut socket, &args).await, End::NoOutput)
                && let Some(archive) = archive
            {
                cached(&mut socket, archive).await;
            }
        })
        .into_response()
}

fn log_args(view: &str, worker: &str, job: Option<&str>) -> Result<Vec<String>, Failure> {
    if !matches!(view, "logs" | "raw-logs") {
        return Err(super::NOT_FOUND);
    }
    let (mut args, _) = worker_command(view, worker, job)?;
    args.push("--follow".into());
    Ok(args)
}

async fn authenticate(socket: &mut WebSocket) -> bool {
    let Ok(Some(Ok(Message::Text(token)))) = timeout(Duration::from_secs(5), socket.recv()).await
    else {
        return false;
    };
    authorize_token(&token)
}

pub(super) fn authorize_token(token: &str) -> bool {
    let Ok(value) = HeaderValue::from_str(&format!("Bearer {token}")) else {
        return false;
    };
    let mut headers = HeaderMap::new();
    headers.insert(header::AUTHORIZATION, value);
    authorized(&headers).is_ok()
}

enum End {
    NoOutput,
    Closed,
}

struct LogProcess(Child);

impl Drop for LogProcess {
    fn drop(&mut self) {
        if let Some(pid) = self.0.id() {
            // SAFETY: the unreaped child holds this PID, and starts in a new
            // group whose ID is the child's PID.
            unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
        }
    }
}

fn spawn(args: &[String]) -> std::io::Result<LogProcess> {
    use std::os::unix::process::CommandExt;

    let mut cmd = Command::new(std::env::current_exe()?);
    // No NO_COLOR: the console's viewer renders the colors already in the
    // logs, such as the worker/job prefixes.
    cmd.args(args)
        .env("RWORKER_LOCAL_RELAY", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    tokio::process::Command::from(cmd)
        .kill_on_drop(true)
        .spawn()
        .map(LogProcess)
}

async fn stream(socket: &mut WebSocket, args: &[String]) -> End {
    let Ok(mut child) = spawn(args) else {
        return End::NoOutput;
    };
    let Some(mut stdout) = child.0.stdout.take() else {
        return End::NoOutput;
    };
    let mut buf = [0; 8192];
    let mut sent = false;
    let mut ping = tokio::time::interval(Duration::from_secs(20));
    loop {
        let frame = tokio::select! {
            data = stdout.read(&mut buf) => {
                match data {
                    Ok(n) if n > 0 => {
                        sent = true;
                        Message::Binary(buf[..n].to_vec().into())
                    }
                    _ => return if sent { End::Closed } else { End::NoOutput },
                }
            }
            message = socket.recv() => {
                match message {
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                    _ => return End::Closed,
                }
            }
            _ = ping.tick() => Message::Ping(Bytes::new()),
        };
        if !matches!(
            timeout(Duration::from_secs(10), socket.send(frame)).await,
            Ok(Ok(()))
        ) {
            return End::Closed;
        }
    }
}

async fn cached(socket: &mut WebSocket, args: Vec<String>) {
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    let mut previous = String::new();
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let snapshot = run(args.clone(), 4).await.ok().filter(|text| text != &previous);
                let frame = if let Some(snapshot) = snapshot {
                    previous = snapshot;
                    let mut start = previous.len().saturating_sub(262_144);
                    while !previous.is_char_boundary(start) {
                        start += 1;
                    }
                    let data = serde_json::json!({"snapshot": &previous[start..]}).to_string();
                    Message::Text(data.into())
                } else {
                    Message::Ping(Bytes::new())
                };
                if !matches!(
                    timeout(Duration::from_secs(10), socket.send(frame)).await,
                    Ok(Ok(()))
                ) {
                    return;
                }
            }
            message = socket.recv() => {
                match message {
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                    _ => return,
                }
            }
        }
    }
}

#[cfg(test)]
mod filter_tests {
    use super::*;
    #[test]
    fn matching_uses_the_same_worker_filters_as_the_cli() {
        let filters = LogFilters {
            topic: "(?i)nixpp".into(),
            agent: "!circus".into(),
            agent_id: String::new(),
        };
        let workers = serde_json::json!([
            {"name":"yes", "topic":"NIXPP upgrade", "agent":"codex"},
            {"name":"wrong-topic", "topic":"build", "agent":"codex"},
            {"name":"wrong-agent", "topic":"nixpp", "agent":"circus"}
        ]);
        assert_eq!(
            matching_workers(&filters, &workers),
            Some(vec!["yes".into()])
        );
    }

    #[test]
    fn filter_check_uses_the_console_response_envelope() {
        let response = matching_workers_response(vec!["worker-a".into()]);
        assert_eq!(
            response["data"]["matching_workers"],
            serde_json::json!(["worker-a"])
        );
    }

    #[test]
    fn global_log_filters_become_cli_arguments() {
        let query = format!(
            "filters={}",
            URL_SAFE_NO_PAD.encode(r#"{"topic":"nixpp","agent":"!other","agent_id":"codex:.*"}"#)
        );
        let args = all_args("logs", Some(&query)).unwrap();
        assert!(args.windows(2).any(|pair| pair == ["--topic", "nixpp"]));
        assert!(args.windows(2).any(|pair| pair == ["--agent", "!other"]));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--agent-id", "codex:.*"])
        );
        assert!(args.contains(&"--redact".to_string()));
        let invalid = format!("filters={}", URL_SAFE_NO_PAD.encode(r#"{"topic":"["}"#));
        assert!(all_args("logs", Some(&invalid)).is_err());
        assert!(all_args("logs", Some("filters=???")).is_err());
    }
}
