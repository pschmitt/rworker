use std::hash::{DefaultHasher, Hash, Hasher};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Router, routing::get};
use serde_json::{Value, json};
use std::time::Duration;

use super::inventory_cache;

pub(super) fn serve(dir: PathBuf, socket: &str) -> io::Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?
        .block_on(async move {
            match std::fs::remove_file(socket) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            let listener = tokio::net::UnixListener::bind(socket)?;
            std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o660))?;
            let app = Router::new()
                .route("/v1/workers", get(workers))
                .route("/v1/workers/events/ws", get(events))
                .route("/v1/workers/{name}", get(worker));
            #[cfg(feature = "web")]
            let app = super::web::routes(app);
            axum::serve(listener, app.with_state(dir)).await
        })
}

async fn events(
    State(dir): State<PathBuf>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if headers.contains_key(header::ORIGIN) {
        return StatusCode::FORBIDDEN.into_response();
    }
    upgrade_inventory(dir, true, upgrade)
}

pub(super) fn upgrade_inventory(
    dir: PathBuf,
    send_snapshot: bool,
    upgrade: WebSocketUpgrade,
) -> Response {
    upgrade
        .max_message_size(125)
        .max_frame_size(125)
        .on_upgrade(move |socket| stream_inventory(dir, send_snapshot, socket))
}

async fn stream_inventory(dir: PathBuf, send_snapshot: bool, mut socket: WebSocket) {
    let mut source = inventory_cache::Events::new(dir);
    let mut ticks = tokio::time::interval(Duration::from_millis(250));
    let mut heartbeats = tokio::time::interval(Duration::from_secs(5));
    loop {
        let text = tokio::select! {
            _ = ticks.tick() => {
                let Ok((next, result)) = tokio::task::spawn_blocking(move || {
                    let update = source.next_snapshot().map(|snapshot| snapshot.map(|body| {
                        if send_snapshot { format!("{body}\n") } else { "updated".to_owned() }
                    }));
                    (source, update)
                }).await else { return };
                source = next;
                match result {
                    Ok(Some(text)) => text,
                    Ok(None) => continue,
                    Err(_) => return,
                }
            }
            _ = heartbeats.tick() => "\n".into(),
            message = socket.recv() => {
                match message {
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                    _ => return,
                }
            }
        };
        if !matches!(
            tokio::time::timeout(
                Duration::from_secs(3),
                socket.send(Message::Text(text.into()))
            )
            .await,
            Ok(Ok(()))
        ) {
            return;
        }
    }
}

async fn workers(State(dir): State<PathBuf>, headers: HeaderMap) -> Response {
    inventory(dir, headers, None).await
}

async fn worker(
    State(dir): State<PathBuf>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    inventory(dir, headers, Some(name)).await
}

async fn inventory(dir: PathBuf, headers: HeaderMap, name: Option<String>) -> Response {
    let Ok(Some((mut workers, age))) =
        tokio::task::spawn_blocking(move || inventory_cache::read(&dir, false)).await
    else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "inventory unavailable");
    };
    if let Some(name) = name {
        let Some(worker) = workers.as_array_mut().and_then(|items| {
            items
                .iter_mut()
                .find(|worker| worker["name"].as_str() == Some(&name))
        }) else {
            return error(StatusCode::NOT_FOUND, "worker not found");
        };
        workers = worker.take();
    }
    let body = workers.to_string();
    let mut hash = DefaultHasher::new();
    body.hash(&mut hash);
    let tag = format!("W/\"{:016x}\"", hash.finish());
    let unchanged = headers.get_all(header::IF_NONE_MATCH).iter().any(|value| {
        value.to_str().is_ok_and(|value| {
            value.split(',').map(str::trim).any(|candidate| {
                candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate) == &tag[2..]
            })
        })
    });
    let mut response = if unchanged {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        ([(header::CONTENT_TYPE, "application/json")], body).into_response()
    };
    let headers = response.headers_mut();
    headers.insert(
        header::ETAG,
        header::HeaderValue::from_str(&tag).expect("hex ETag"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("private, no-cache"),
    );
    headers.insert(
        "x-rworker-inventory-age-ms",
        header::HeaderValue::from_str(&age.as_millis().to_string()).expect("decimal age"),
    );
    response
}

fn error(status: StatusCode, message: &str) -> Response {
    let body: Value = json!({"error": message});
    (
        status,
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body.to_string(),
    )
        .into_response()
}
