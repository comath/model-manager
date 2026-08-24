//! Launching, stopping, and listing the containers a recipe describes.
//!
//! Podman itself is the registry: every container we start carries our
//! labels, so `podman ps` after a restart still finds them — nothing about
//! running state needs to be persisted separately.

use crate::config::Config;
use crate::error::{AppError, AppResult};
use crate::hf::ModelKind;
use crate::recipe::{self, Recipe};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use tokio::process::Command;

#[derive(Debug, Clone, Serialize)]
pub struct RunningContainer {
    pub id: String,
    pub name: String,
    pub repo_id: String,
    pub kind: ModelKind,
    pub gpu_index: Option<u32>,
    pub state: String,
    pub host_port: Option<u16>,
}

#[derive(Debug, Deserialize)]
struct RawContainer {
    #[serde(alias = "ID", alias = "Id")]
    id: String,
    #[serde(alias = "Names")]
    names: NamesField,
    #[serde(alias = "State")]
    state: String,
    #[serde(alias = "Labels", default)]
    labels: HashMap<String, String>,
    #[serde(alias = "Ports", default)]
    ports: Vec<RawPort>,
}

#[derive(Debug, Deserialize)]
struct RawPort {
    #[serde(default)]
    host_port: u16,
}

/// Podman reports `Names` as an array; Docker (for anyone swapping the
/// runtime binary) reports it as a single string — accept either.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum NamesField {
    Many(Vec<String>),
    One(String),
}

impl NamesField {
    fn first(&self) -> String {
        match self {
            NamesField::Many(v) => v.first().cloned().unwrap_or_default(),
            NamesField::One(s) => s.clone(),
        }
    }
}

async fn run(cfg: &Config, args: &[&str]) -> AppResult<std::process::Output> {
    Command::new(&cfg.container_runtime).args(args).output().await.map_err(|err| {
        AppError::BadRequest(format!("could not run `{}`: {err}", cfg.container_runtime))
    })
}

pub async fn list_running(cfg: &Config) -> AppResult<Vec<RunningContainer>> {
    let output = run(
        cfg,
        &["ps", "--filter", &format!("label={}", recipe::LABEL_MARKER), "--format", "json"],
    )
    .await?;
    if !output.status.success() {
        return Err(AppError::Upstream(format!(
            "{} ps failed: {}",
            cfg.container_runtime,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let raw: Vec<RawContainer> = serde_json::from_slice(&output.stdout).map_err(|err| {
        AppError::Upstream(format!("could not parse `{} ps` output: {err}", cfg.container_runtime))
    })?;
    Ok(raw
        .into_iter()
        .filter_map(|c| {
            let repo_id = c.labels.get(recipe::LABEL_REPO)?.clone();
            let kind = ModelKind::parse(c.labels.get(recipe::LABEL_KIND)?)?;
            Some(RunningContainer {
                id: c.id,
                name: c.names.first(),
                repo_id,
                kind,
                gpu_index: c.labels.get(recipe::LABEL_GPU).and_then(|g| g.parse().ok()),
                state: c.state,
                host_port: c.ports.first().map(|p| p.host_port).filter(|p| *p != 0),
            })
        })
        .collect())
}

/// Refuse to double-book a GPU: at most one of our containers per index.
async fn assert_gpu_free(cfg: &Config, gpu: u32, except_repo: &str) -> AppResult<()> {
    let busy = list_running(cfg)
        .await?
        .into_iter()
        .find(|c| c.gpu_index == Some(gpu) && c.repo_id != except_repo);
    if let Some(c) = busy {
        return Err(AppError::BadRequest(format!(
            "GPU {gpu} is already in use by {} ({})",
            c.repo_id, c.name
        )));
    }
    Ok(())
}

pub async fn launch(
    cfg: &Config,
    kind: ModelKind,
    repo_id: &str,
    model_dir: &Path,
    recipe: &Recipe,
) -> AppResult<RunningContainer> {
    if let Some(gpu) = recipe.gpu_index {
        assert_gpu_free(cfg, gpu, repo_id).await?;
    }
    let name = recipe::container_name(kind, repo_id);
    if list_running(cfg).await?.iter().any(|c| c.name == name) {
        return Err(AppError::BadRequest(format!("{repo_id} is already running")));
    }

    let argv = recipe::render_argv(cfg, kind, repo_id, model_dir, recipe)?;
    let output = Command::new(&argv[0]).args(&argv[1..]).output().await?;
    if !output.status.success() {
        return Err(AppError::Upstream(format!(
            "failed to launch {repo_id}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(RunningContainer {
        id,
        name,
        repo_id: repo_id.to_string(),
        kind,
        gpu_index: recipe.gpu_index,
        host_port: Some(recipe.host_port),
        state: "running".into(),
    })
}

pub async fn stop(cfg: &Config, container: &str) -> AppResult<()> {
    let output = run(cfg, &["stop", container]).await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.contains("no such container") {
            return Err(AppError::NotFound(format!("no such container: {container}")));
        }
        return Err(AppError::Upstream(format!("failed to stop {container}: {stderr}")));
    }
    Ok(())
}

pub async fn logs(cfg: &Config, container: &str, tail: u32) -> AppResult<String> {
    let output = run(cfg, &["logs", "--tail", &tail.to_string(), container]).await?;
    if !output.status.success() {
        return Err(AppError::NotFound(format!(
            "no such container: {container} ({})",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    // llama.cpp/vllm both log to stderr; merge so the GUI sees everything in order.
    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&output.stderr));
    Ok(combined)
}
