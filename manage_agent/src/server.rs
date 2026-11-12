//! HTTP API: POST /jobs, GET /jobs, GET /jobs/{id}, GET /healthz.

use std::sync::Arc;

use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::{Value, json};

use crate::{MAX_BODY, manager::Manager};

type Shared = Arc<Manager>;

pub fn router(manager: Shared) -> Router {
    Router::new()
        .route("/healthz", get(|| async { Json(json!({"ok": true})) }))
        .route("/jobs", get(list).post(submit))
        .route("/jobs/", get(list).post(submit))
        .route("/jobs/{id}", get(show))
        .fallback(|| async { error(StatusCode::NOT_FOUND, "not found") })
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(manager)
}

fn error(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({"error": msg}))).into_response()
}

async fn list(State(m): State<Shared>) -> Response {
    Json(m.list()).into_response()
}

async fn show(State(m): State<Shared>, Path(id): Path<String>) -> Response {
    match m.get(&id) {
        Some(job) => Json(job).into_response(),
        None => error(StatusCode::NOT_FOUND, "not found"),
    }
}

/// A string field, with a missing/null/empty value read as "".
fn optional_str(body: &Value, key: &str) -> String {
    match body.get(key) {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

async fn submit(State(m): State<Shared>, body: Bytes) -> Response {
    let bad = |msg: &str| error(StatusCode::BAD_REQUEST, msg);
    if body.is_empty() {
        return bad("missing or oversized body");
    }
    let Ok(body) = serde_json::from_slice::<Value>(&body) else {
        return bad("body is not valid JSON");
    };
    if !body.is_object() {
        return bad("body must be a JSON object");
    }
    let (Some(source), Some(path)) = (
        body.get("source").and_then(Value::as_str),
        body.get("path").and_then(Value::as_str),
    ) else {
        return bad("source and path are required");
    };
    let (repo_id, revision) = (optional_str(&body, "repo_id"), optional_str(&body, "revision"));
    // resolve_raw touches the filesystem briefly (canonicalize); fine on the async thread.
    match m.submit(source, &repo_id, &revision, path) {
        Ok((job, created)) => {
            let status = if created { StatusCode::ACCEPTED } else { StatusCode::OK };
            (status, Json(job)).into_response()
        }
        Err(msg) => bad(&msg),
    }
}
