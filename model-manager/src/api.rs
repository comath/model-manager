use crate::download::{Manager, NewJob};
use crate::error::{AppError, AppResult};
use crate::hf::{ModelKind, validate_repo_id};
use crate::recipe::{self, Recipe};
use crate::source::{Source, parse_repo_ref};
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
        .route("/clone", post(clone_repo))
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
    let stores: Vec<serde_json::Value> = cfg
        .stores
        .iter()
        .map(|s| json!({ "name": s.name, "root": s.root.display().to_string() }))
        .collect();
    Json(json!({
        "data_dir": cfg.data_dir.display().to_string(),
        "models_dir": cfg.stores[0].root.display().to_string(),
        "datasets_dir": cfg.datasets_store.root.display().to_string(),
        "stores": stores,
        "parallel_files": cfg.parallel_files,
        "has_hf_token": cfg.hf_token.is_some(),
        "has_kaggle_credentials": cfg.kaggle.is_some(),
        "has_github_token": cfg.github_token.is_some(),
    }))
}

#[derive(Deserialize)]
struct SearchParams {
    #[serde(default)]
    q: String,
    kind: String,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

async fn search(
    State(mgr): Ctx,
    Query(p): Query<SearchParams>,
) -> AppResult<Json<serde_json::Value>> {
    let kind = parse_kind(&p.kind)?;
    let limit = p.limit.unwrap_or(30);
    let models = if kind == ModelKind::Dataset {
        let source = parse_source(p.source.as_deref(), kind)?;
        mgr.hub().search(source, &p.q, limit).await?
    } else {
        mgr.hf().search(&p.q, kind, limit).await?
    };
    Ok(Json(json!({ "kind": kind, "models": models })))
}

#[derive(Deserialize)]
struct RepoParams {
    kind: String,
    #[serde(default)]
    source: Option<String>,
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
    let source = parse_source(p.source.as_deref(), kind)?;
    let requested = p.revision.map(|r| r.trim().to_string()).filter(|r| !r.is_empty());
    let (info, revision, files) =
        fetch_repo(&mgr, kind, source, &repo_id, requested.as_deref()).await?;
    Ok(Json(json!({
        "model": info,
        "files": files,
        "revision": revision,
        "kind": kind,
        "source": source,
        "dest": mgr.cfg().stores_for(kind)[0].repo_dir(kind, source, &repo_id).display().to_string(),
        // Where it would land in each store, so the UI can show the chosen one.
        "stores": mgr.cfg().stores_for(kind).iter().map(|s| json!({
            "name": s.name,
            "dest": s.repo_dir(kind, source, &repo_id).display().to_string(),
        })).collect::<Vec<_>>(),
    })))
}

/// Summary, resolved revision and file list of a repo, whatever its source.
async fn fetch_repo(
    mgr: &Manager,
    kind: ModelKind,
    source: Source,
    repo_id: &str,
    requested: Option<&str>,
) -> AppResult<(crate::hf::ModelSummary, String, Vec<crate::hf::RepoFile>)> {
    if kind == ModelKind::Dataset {
        let (info, revision) = mgr.hub().info(source, repo_id, requested).await?;
        let files = mgr.hub().list_files(source, repo_id, &revision).await?;
        Ok((info, revision, files))
    } else {
        let revision = requested.unwrap_or("main").to_string();
        let (info, files) = tokio::try_join!(
            mgr.hf().model_info(repo_id, &revision, kind),
            mgr.hf().list_files(repo_id, &revision, kind),
        )?;
        Ok((info, revision, files))
    }
}

#[derive(Deserialize)]
struct CloneRequest {
    /// `owner/name`, or a Hugging Face / Kaggle / GitHub URL.
    repo: String,
    /// `safetensors` or `dataset`; defaults to what the URL implies, else `dataset`.
    #[serde(default)]
    kind: Option<String>,
    /// Required for a bare `owner/name` (huggingface | kaggle | github); a URL implies it.
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    revision: Option<String>,
    #[serde(default)]
    store: Option<String>,
}

/// One-shot "give me this repo": resolve it, select every relevant file, start the job.
///
/// The provider must be explicit: either a URL that names it, or `source` in the request. A bare
/// `owner/name` is ambiguous (the same id can exist on several sites), so we never guess.
async fn clone_repo(
    State(mgr): Ctx,
    Json(body): Json<CloneRequest>,
) -> AppResult<Json<serde_json::Value>> {
    let parsed = parse_repo_ref(&body.repo)?;
    validate_repo_id(&parsed.repo_id)?;

    let source = match (given(body.source.as_deref()), parsed.source) {
        (Some(raw), url_source) => {
            let source = Source::parse(raw).ok_or_else(|| {
                AppError::BadRequest(format!(
                    "unknown source {raw:?} (huggingface | kaggle | github)"
                ))
            })?;
            if url_source.is_some_and(|u| u != source) {
                return Err(AppError::BadRequest(format!(
                    "the URL is on {}, but source is {}",
                    url_source.map_or("?", Source::as_str),
                    source.as_str()
                )));
            }
            source
        }
        (None, Some(url_source)) => url_source,
        (None, None) => {
            return Err(AppError::BadRequest(
                "say where it lives: set source (huggingface | kaggle | github) or paste a URL"
                    .into(),
            ));
        }
    };
    // Hugging Face holds models and datasets: an explicit kind wins, then the URL, then dataset.
    let kind = match given(body.kind.as_deref()) {
        Some(k) => parse_kind(k)?,
        None if parsed.dataset == Some(false) => ModelKind::Safetensors,
        None => ModelKind::Dataset,
    };
    if kind == ModelKind::Gguf {
        return Err(AppError::BadRequest(
            "a GGUF repo holds many quantisations — pick one on the model page instead".into(),
        ));
    }
    if kind != ModelKind::Dataset && source != Source::Huggingface {
        return Err(AppError::BadRequest("models can only come from Hugging Face".into()));
    }

    let requested =
        body.revision.or(parsed.revision).map(|r| r.trim().to_string()).filter(|r| !r.is_empty());
    let (_, revision, files) =
        fetch_repo(&mgr, kind, source, &parsed.repo_id, requested.as_deref()).await?;
    let files: Vec<(String, u64)> =
        files.into_iter().filter(|f| f.relevant).map(|f| (f.path, f.size)).collect();
    let job = mgr
        .enqueue(NewJob {
            repo_id: parsed.repo_id,
            revision,
            kind,
            source,
            store: body.store,
            files,
        })
        .await?;
    Ok(Json(json!({ "job": job })))
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
    /// Dataset source (huggingface | kaggle | github); models are always HuggingFace.
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    revision: Option<String>,
    /// Store to download into (see `MODEL_MANAGER_STORES`); default store if omitted.
    #[serde(default)]
    store: Option<String>,
    files: Vec<FileRequest>,
}

/// `?store=` on library/recipe routes; omitted means the first store holding the repo.
#[derive(Deserialize, Default)]
struct StoreParam {
    #[serde(default)]
    store: Option<String>,
    #[serde(default)]
    source: Option<String>,
}

async fn create_job(
    State(mgr): Ctx,
    Json(body): Json<CreateJob>,
) -> AppResult<Json<serde_json::Value>> {
    let kind = parse_kind(&body.kind)?;
    validate_repo_id(&body.repo_id)?;
    let source = parse_source(body.source.as_deref(), kind)?;
    let job = mgr
        .enqueue(NewJob {
            repo_id: body.repo_id,
            revision: revision_or_default(body.revision, source),
            kind,
            source,
            store: body.store,
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
    Query(p): Query<StoreParam>,
) -> AppResult<Json<serde_json::Value>> {
    let kind = parse_kind(&kind)?;
    store::delete(
        mgr.cfg(),
        p.store.as_deref(),
        kind,
        parse_source(p.source.as_deref(), kind)?,
        &repo_id,
    )
    .await?;
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
    Query(p): Query<StoreParam>,
) -> AppResult<Json<serde_json::Value>> {
    let kind = parse_runnable_kind(&kind)?;
    let (_, dir, meta) =
        store::locate(mgr.cfg(), p.store.as_deref(), kind, Source::default(), &repo_id).await?;
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
    Query(p): Query<StoreParam>,
    Json(body): Json<Recipe>,
) -> AppResult<Json<serde_json::Value>> {
    let kind = parse_runnable_kind(&kind)?;
    let (_, dir, _) =
        store::locate(mgr.cfg(), p.store.as_deref(), kind, Source::default(), &repo_id).await?;
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
    #[serde(default)]
    store: Option<String>,
}

async fn launch(
    State(mgr): Ctx,
    Json(body): Json<LaunchRequest>,
) -> AppResult<Json<serde_json::Value>> {
    let kind = parse_runnable_kind(&body.kind)?;
    let (_, dir, meta) =
        store::locate(mgr.cfg(), body.store.as_deref(), kind, Source::default(), &body.repo_id)
            .await?;
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
    ModelKind::parse(raw).ok_or_else(|| {
        AppError::BadRequest(format!("unknown kind {raw:?} (safetensors | gguf | dataset)"))
    })
}

/// Like `parse_kind`, but for routes that run a model in a container.
fn parse_runnable_kind(raw: &str) -> AppResult<ModelKind> {
    match parse_kind(raw)? {
        ModelKind::Dataset => Err(AppError::BadRequest("datasets cannot be run".into())),
        kind => Ok(kind),
    }
}

fn given(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

/// Only datasets have a choice of source; everything else is HuggingFace.
fn parse_source(raw: Option<&str>, kind: ModelKind) -> AppResult<Source> {
    if kind != ModelKind::Dataset {
        return Ok(Source::Huggingface);
    }
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(Source::Huggingface),
        Some(raw) => Source::parse(raw).ok_or_else(|| {
            AppError::BadRequest(format!("unknown source {raw:?} (huggingface | kaggle | github)"))
        }),
    }
}

fn revision_or_default(revision: Option<String>, source: Source) -> String {
    revision.map(|r| r.trim().to_string()).filter(|r| !r.is_empty()).unwrap_or_else(|| {
        match source {
            Source::Huggingface => "main",
            Source::Github => "HEAD",
            Source::Kaggle => "latest",
        }
        .into()
    })
}
