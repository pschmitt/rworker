use std::fs::{self, File};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde::Deserialize;
use serde_json::Value;

use super::github_leases::{self, Disk, Endpoint, MeshResult};

#[derive(Deserialize)]
struct Claim {
    profile: String,
    architecture: String,
    #[serde(default)]
    capabilities: Vec<String>,
    disk: Option<Disk>,
}

pub(super) fn serve(config: &str, socket: &str) -> io::Result<()> {
    let settings: Value = serde_json::from_reader(File::open(config)?)?;
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?
        .block_on(async {
            let listener = tokio::net::UnixListener::bind(socket)?;
            fs::set_permissions(socket, fs::Permissions::from_mode(0o660))?;
            let app = Router::new()
                .route("/v1/agent/jobs/{id}/claim", post(claim))
                .route("/v1/agent/jobs/{id}/ready", post(ready))
                .route("/v1/agent/jobs/{id}/heartbeat", post(heartbeat))
                .layer(DefaultBodyLimit::max(4096))
                .with_state(Arc::new(settings));
            axum::serve(listener, app).await
        })
}

async fn claim(
    State(settings): State<Arc<Value>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(token) = bearer(&headers) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let Ok(request) = serde_json::from_slice::<Claim>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    static CLAIMS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(8);
    let Ok(permit) = CLAIMS.try_acquire() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let token = token.to_owned();
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        github_leases::claim(
            &settings,
            &id,
            &token,
            &request.profile,
            &request.architecture,
            &request.capabilities,
            request.disk,
        )
    })
    .await
    {
        Ok(Ok(response)) => (
            [(header::CONTENT_TYPE, "application/json")],
            response.to_string(),
        )
            .into_response(),
        Ok(Err(_)) => StatusCode::FORBIDDEN.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

async fn ready(
    State(settings): State<Arc<Value>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Ok(endpoint) = serde_json::from_slice(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    update(settings, id, headers, Some(endpoint), None).await
}

#[derive(Deserialize)]
struct Heartbeat {
    mesh_result: Option<MeshResult>,
}

async fn heartbeat(
    State(settings): State<Arc<Value>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let result = if body.is_empty() {
        None
    } else {
        let Ok(heartbeat) = serde_json::from_slice::<Heartbeat>(&body) else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        heartbeat.mesh_result
    };
    update(settings, id, headers, None, result).await
}

async fn update(
    settings: Arc<Value>,
    id: String,
    headers: HeaderMap,
    endpoint: Option<Endpoint>,
    mesh_result: Option<MeshResult>,
) -> Response {
    let Some(token) = bearer(&headers) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    static UPDATES: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(16);
    let Ok(permit) = UPDATES.try_acquire() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let token = token.to_owned();
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        github_leases::agent_update(&settings, &id, &token, endpoint, mesh_result)
    })
    .await
    {
        Ok(Ok(None)) => StatusCode::NO_CONTENT.into_response(),
        Ok(Ok(Some(assignment))) => (
            [
                (header::CONTENT_TYPE, "application/json"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            assignment.to_string(),
        )
            .into_response(),
        Ok(Err(_)) => (StatusCode::FORBIDDEN, [("x-rworker-lease", "closed")]).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
