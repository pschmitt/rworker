use std::hash::{DefaultHasher, Hash, Hasher};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event, KeepAlive};
use axum::response::{IntoResponse, Response, Sse};
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
                .route("/v1/workers/events/stream", get(events))
                .route("/v1/workers/{name}", get(worker));
            #[cfg(feature = "web")]
            let app = super::web::routes(app);
            axum::serve(listener, app.with_state(dir)).await
        })
}

async fn events(State(dir): State<PathBuf>) -> Response {
    let stream =
        futures_util::stream::unfold(inventory_cache::Events::new(dir), |mut source| async move {
            loop {
                let result = tokio::task::spawn_blocking(move || {
                    let snapshot = source
                        .next_snapshot()
                        .map(|snapshot| snapshot.map(str::to_owned));
                    (source, snapshot)
                })
                .await
                .ok()?;
                source = result.0;
                match result.1 {
                    Ok(Some(snapshot)) => {
                        return Some((
                            Ok::<_, std::convert::Infallible>(Event::default().data(snapshot)),
                            source,
                        ));
                    }
                    Ok(None) => tokio::time::sleep(Duration::from_millis(250)).await,
                    Err(_) => return None,
                }
            }
        });
    let mut response = Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(5)))
        .into_response();
    response
        .headers_mut()
        .insert("x-accel-buffering", header::HeaderValue::from_static("no"));
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    response
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
