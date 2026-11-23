use super::{Failure, authorized, failure, run, same_origin, worker_command};
use axum::Router;
use axum::body::Bytes;
use axum::extract::Path;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
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
        .route("/v1/ui/follow/{view}/{worker}", get(worker))
        .route("/v1/ui/follow/{view}/{worker}/{job}", get(job))
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
    if !same_origin(headers) {
        return failure((StatusCode::FORBIDDEN, "Invalid WebSocket origin."));
    }
    let check = authorized(headers);
    if let Err(error) = check
        && error.0 != StatusCode::UNAUTHORIZED
    {
        return failure(error);
    }
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
            if matches!(stream(&mut socket, &args).await, End::NoOutput) {
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
    cmd.args(args)
        .env("NO_COLOR", "1")
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
