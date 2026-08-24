use crate::download::{Manager, NewJob};
use crate::error::{AppError, AppResult};
use crate::hf::{ModelKind, validate_repo_id};
use crate::recipe::{self, Recipe};
use crate::{runtime, store};
use axum::extract::{Path, Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use futures_util::stream::Stream;
use serde::Deserialize;
use serde_json::json;
use std::convert::Infallible;
use std::sync::Arc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use uuid::Uuid;

type Ctx = State<Arc<Manager>>;

pub fn router(state: Arc<Manager>) -> Router {
    Router::new()
        .route("/config", get(get_config))
        .route("/search", get(search))
        .route("/repo/{*repo_id}", get(repo_detail))
        .route("/downloads", get(list_jobs).post(create_job))
        .route("/downloads/{id}", get(get_job).delete(forget_job))
        .route("/downloads/{id}/cancel", post(cancel_job))
        .route("/library", get(library))
        .route("/library/{kind}/{*repo_id}", delete(delete_model))
        .route("/gpus", get(list_gpus))
        .route("/recipes/{kind}/{*repo_id}", get(get_recipe).put(put_recipe))
        .route("/runtime", get(list_running))
        .route("/runtime/launch", post(launch))
        .route("/runtime/{name}/stop", post(stop_container))
        .route("/runtime/{name}/logs", get(container_logs))
        .route("/events", get(events))
        .with_state(state)
}

async fn get_config(State(mgr): Ctx) -> Json<serde_json::Value> {
    let cfg = mgr.cfg();
    Json(json!({
        "data_dir": cfg.data_dir.display().to_string(),
        "models_dir": cfg.models_dir().display().to_string(),
        "parallel_files": cfg.parallel_files,
        "has_hf_token": cfg.hf_token.is_some(),
    }))
}

#[derive(Deserialize)]
struct SearchParams {
    #[serde(default)]
    q: String,
    kind: String,
    #[serde(default)]
    limit: Option<usize>,
}

async fn search(
    State(mgr): Ctx,
    Query(p): Query<SearchParams>,
) -> AppResult<Json<serde_json::Value>> {
    let kind = parse_kind(&p.kind)?;
    let models = mgr.hf().search(&p.q, kind, p.limit.unwrap_or(30)).await?;
    Ok(Json(json!({ "kind": kind, "models": models })))
}

#[derive(Deserialize)]
struct RepoParams {
    kind: String,
    #[serde(default)]
    revision: Option<String>,
}

async fn repo_detail(
    State(mgr): Ctx,
    Path(repo_id): Path<String>,
    Query(p): Query<RepoParams>,
) -> AppResult<Json<serde_json::Value>> {
    let kind = parse_kind(&p.kind)?;
    validate_repo_id(&repo_id)?;
    let revision = revision_or_main(p.revision);
    let (info, files) = tokio::try_join!(
        mgr.hf().model_info(&repo_id, &revision),
        mgr.hf().list_files(&repo_id, &revision, kind),
    )?;
    Ok(Json(json!({
        "model": info,
        "files": files,
        "revision": revision,
        "kind": kind,
        "dest": mgr.cfg().repo_dir(kind, &repo_id).display().to_string(),
    })))
}

#[derive(Deserialize)]
struct FileRequest {
    path: String,
    #[serde(default)]
    size: u64,
}

#[derive(Deserialize)]
struct CreateJob {
    repo_id: String,
    kind: String,
    #[serde(default)]
    revision: Option<String>,
    files: Vec<FileRequest>,
}

async fn create_job(
    State(mgr): Ctx,
    Json(body): Json<CreateJob>,
) -> AppResult<Json<serde_json::Value>> {
    let kind = parse_kind(&body.kind)?;
    validate_repo_id(&body.repo_id)?;
    let job = mgr
        .enqueue(NewJob {
            repo_id: body.repo_id,
            revision: revision_or_main(body.revision),
            kind,
            files: body.files.into_iter().map(|f| (f.path, f.size)).collect(),
        })
        .await?;
    Ok(Json(json!({ "job": job })))
}

async fn list_jobs(State(mgr): Ctx) -> Json<serde_json::Value> {
    Json(json!({ "jobs": mgr.list().await }))
}

async fn get_job(State(mgr): Ctx, Path(id): Path<Uuid>) -> AppResult<Json<serde_json::Value>> {
    let job = mgr.get(id).await.ok_or_else(|| AppError::NotFound(format!("no such job: {id}")))?;
    Ok(Json(json!({ "job": job })))
}

async fn cancel_job(State(mgr): Ctx, Path(id): Path<Uuid>) -> AppResult<Json<serde_json::Value>> {
    Ok(Json(json!({ "job": mgr.cancel(id).await? })))
}

async fn forget_job(State(mgr): Ctx, Path(id): Path<Uuid>) -> AppResult<Json<serde_json::Value>> {
    mgr.forget(id).await?;
    Ok(Json(json!({ "ok": true })))
}

async fn library(State(mgr): Ctx) -> AppResult<Json<serde_json::Value>> {
    Ok(Json(json!({ "models": store::list(mgr.cfg()).await? })))
}

async fn delete_model(
    State(mgr): Ctx,
    Path((kind, repo_id)): Path<(String, String)>,
) -> AppResult<Json<serde_json::Value>> {
    let kind = parse_kind(&kind)?;
    store::delete(mgr.cfg(), kind, &repo_id).await?;
    mgr.emit_library_changed();
    Ok(Json(json!({ "ok": true })))
}

async fn list_gpus(State(mgr): Ctx) -> AppResult<Json<serde_json::Value>> {
    let running = runtime::list_running(mgr.cfg()).await.unwrap_or_default();
    let gpus: Vec<serde_json::Value> = mgr
        .gpus()
        .iter()
        .map(|g| {
            let busy = running.iter().find(|c| c.gpu_index == Some(g.index));
            json!({
                "index": g.index,
                "name": g.name,
                "vram_bytes": g.vram_bytes,
                "busy_with": busy.map(|c| &c.repo_id),
            })
        })
        .collect();
    Ok(Json(json!({ "gpus": gpus })))
}

async fn get_recipe(
    State(mgr): Ctx,
    Path((kind, repo_id)): Path<(String, String)>,
) -> AppResult<Json<serde_json::Value>> {
    let kind = parse_kind(&kind)?;
    let meta = store::get_metadata(mgr.cfg(), kind, &repo_id).await?;
    let dir = mgr.cfg().repo_dir(kind, &repo_id);
    let recipe = recipe::load_or_default(&dir, kind, &repo_id, &meta.files).await;
    let argv = recipe::render_argv(mgr.cfg(), kind, &repo_id, &dir, &recipe)?;
    Ok(Json(json!({
        "recipe": recipe,
        "preview": recipe::render_shell(&argv),
        "files": meta.files,
    })))
}

async fn put_recipe(
    State(mgr): Ctx,
    Path((kind, repo_id)): Path<(String, String)>,
    Json(body): Json<Recipe>,
) -> AppResult<Json<serde_json::Value>> {
    let kind = parse_kind(&kind)?;
    store::get_metadata(mgr.cfg(), kind, &repo_id).await?;
    let dir = mgr.cfg().repo_dir(kind, &repo_id);
    recipe::save(&dir, &body).await?;
    let argv = recipe::render_argv(mgr.cfg(), kind, &repo_id, &dir, &body)?;
    Ok(Json(json!({ "recipe": body, "preview": recipe::render_shell(&argv) })))
}

async fn list_running(State(mgr): Ctx) -> AppResult<Json<serde_json::Value>> {
    Ok(Json(json!({ "containers": runtime::list_running(mgr.cfg()).await? })))
}

#[derive(Deserialize)]
struct LaunchRequest {
    kind: String,
    repo_id: String,
}

async fn launch(
    State(mgr): Ctx,
    Json(body): Json<LaunchRequest>,
) -> AppResult<Json<serde_json::Value>> {
    let kind = parse_kind(&body.kind)?;
    let meta = store::get_metadata(mgr.cfg(), kind, &body.repo_id).await?;
    let dir = mgr.cfg().repo_dir(kind, &body.repo_id);
    let recipe = recipe::load_or_default(&dir, kind, &body.repo_id, &meta.files).await;
    let container = runtime::launch(mgr.cfg(), kind, &body.repo_id, &dir, &recipe).await?;
    Ok(Json(json!({ "container": container })))
}

async fn stop_container(
    State(mgr): Ctx,
    Path(name): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    runtime::stop(mgr.cfg(), &name).await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct LogsParams {
    #[serde(default)]
    tail: Option<u32>,
}

async fn container_logs(
    State(mgr): Ctx,
    Path(name): Path<String>,
    Query(p): Query<LogsParams>,
) -> AppResult<Json<serde_json::Value>> {
    let logs = runtime::logs(mgr.cfg(), &name, p.tail.unwrap_or(200)).await?;
    Ok(Json(json!({ "logs": logs })))
}

/// Server-sent events: job progress and library invalidation.
async fn events(State(mgr): Ctx) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = BroadcastStream::new(mgr.subscribe())
        .filter_map(|msg| msg.ok().map(|data| Ok(Event::default().data(data))));
    Sse::new(stream).keep_alive(KeepAlive::default())
}

fn parse_kind(raw: &str) -> AppResult<ModelKind> {
    ModelKind::parse(raw)
        .ok_or_else(|| AppError::BadRequest(format!("unknown kind {raw:?} (safetensors | gguf)")))
}

fn revision_or_main(revision: Option<String>) -> String {
    revision
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| "main".into())
}
