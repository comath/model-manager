use crate::download::Manager;
use crate::error::{AppError, AppResult};
use crate::runtime;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;

pub fn router(state: Arc<Manager>) -> Router {
    Router::new()
        .route("/models", get(list_models))
        .route("/chat/completions", post(chat_completions))
        .route("/completions", post(completions))
        .with_state(state)
}

#[derive(Serialize)]
struct OpenAiModel {
    id: String,
    object: &'static str,
    created: u64,
    owned_by: &'static str,
}

async fn list_models(State(mgr): State<Arc<Manager>>) -> AppResult<Json<Value>> {
    let running = runtime::list_running(mgr.cfg()).await.unwrap_or_default();

    let mut models = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for container in &running {
        if seen.insert(container.repo_id.clone()) {
            models.push(OpenAiModel {
                id: container.repo_id.clone(),
                object: "model",
                created: 0,
                owned_by: "model-manager",
            });
        }
    }

    Ok(Json(json!({
        "object": "list",
        "data": models
    })))
}

#[derive(Deserialize)]
struct ModelPayload {
    #[serde(default)]
    model: String,
    #[serde(default)]
    stream: Option<bool>,
    #[serde(flatten)]
    _extra: Value,
}

async fn find_running_port(mgr: &Manager, target_model: &str) -> AppResult<u16> {
    let running = runtime::list_running(mgr.cfg()).await?;
    let target = target_model.to_lowercase();

    for container in running {
        let repo_lower = container.repo_id.to_lowercase();
        let name_lower = container.name.to_lowercase();
        if repo_lower == target
            || name_lower == target
            || repo_lower.ends_with(&format!("/{target}"))
        {
            if let Some(port) = container.host_port {
                return Ok(port);
            }
        }
    }

    Err(AppError::NotFound(format!(
        "Model '{target_model}' is not currently running. Please launch it first."
    )))
}

fn openai_error(status: StatusCode, message: &str, code: &str) -> Response {
    (
        status,
        Json(json!({
            "error": {
                "message": message,
                "type": "invalid_request_error",
                "param": "model",
                "code": code
            }
        })),
    )
        .into_response()
}

async fn proxy_openai_request(
    mgr: &Manager,
    path: &str,
    _headers: HeaderMap,
    body_bytes: axum::body::Bytes,
) -> Response {
    let payload: ModelPayload = match serde_json::from_slice(&body_bytes) {
        Ok(p) => p,
        Err(e) => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                &format!("Invalid JSON payload: {e}"),
                "invalid_json",
            );
        }
    };

    if payload.model.trim().is_empty() {
        return openai_error(
            StatusCode::BAD_REQUEST,
            "The 'model' field is required.",
            "missing_model",
        );
    }

    let port = match find_running_port(mgr, &payload.model).await {
        Ok(p) => p,
        Err(AppError::NotFound(msg)) => {
            return openai_error(StatusCode::NOT_FOUND, &msg, "model_not_found");
        }
        Err(e) => {
            return openai_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &e.to_string(),
                "internal_error",
            );
        }
    };

    let target_url = format!("http://127.0.0.1:{port}/v1{path}");
    let client = reqwest::Client::new();

    let is_stream = payload.stream.unwrap_or(false);

    let res = match client
        .post(&target_url)
        .header("Content-Type", "application/json")
        .body(body_bytes)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return openai_error(
                StatusCode::BAD_GATEWAY,
                &format!("Failed to reach container backend: {e}"),
                "upstream_error",
            );
        }
    };

    let status = res.status();

    if is_stream && status.is_success() {
        let stream = res
            .bytes_stream()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e));
        let body = Body::from_stream(stream);
        Response::builder()
            .status(status)
            .header("Content-Type", "text/event-stream")
            .header("Cache-Control", "no-cache")
            .header("Connection", "keep-alive")
            .body(body)
            .unwrap_or_else(|_| {
                openai_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to create stream response",
                    "stream_error",
                )
            })
    } else {
        let bytes = match res.bytes().await {
            Ok(b) => b,
            Err(e) => {
                return openai_error(
                    StatusCode::BAD_GATEWAY,
                    &format!("Failed to read container response: {e}"),
                    "upstream_error",
                );
            }
        };
        Response::builder()
            .status(status)
            .header("Content-Type", "application/json")
            .body(Body::from(bytes))
            .unwrap_or_else(|_| {
                openai_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to build response",
                    "response_error",
                )
            })
    }
}

async fn chat_completions(
    State(mgr): State<Arc<Manager>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    proxy_openai_request(&mgr, "/chat/completions", headers, body).await
}

async fn completions(
    State(mgr): State<Arc<Manager>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    proxy_openai_request(&mgr, "/completions", headers, body).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_openai_model_serialization() {
        let model = OpenAiModel {
            id: "Qwen/Qwen2.5-Coder-7B-Instruct-GGUF".into(),
            object: "model",
            created: 1700000000,
            owned_by: "model-manager",
        };
        let val = serde_json::to_value(&model).unwrap();
        assert_eq!(val["id"], "Qwen/Qwen2.5-Coder-7B-Instruct-GGUF");
        assert_eq!(val["object"], "model");
        assert_eq!(val["owned_by"], "model-manager");
    }

    #[test]
    fn test_model_payload_parsing() {
        let json_data = json!({
            "model": "qwen2.5-coder",
            "messages": [{"role": "user", "content": "hello"}],
            "stream": true
        });
        let payload: ModelPayload = serde_json::from_value(json_data).unwrap();
        assert_eq!(payload.model, "qwen2.5-coder");
        assert_eq!(payload.stream, Some(true));
    }
}

