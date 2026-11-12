//! Per-model launch recipes: how to run a downloaded model as a container.
//!
//! Stored as `docker-recipe.json` next to `model-manager.json`. The struct is
//! the source of truth; the actual `podman run` argv is derived from it on
//! demand, so the GUI edits fields instead of a shell string.

use crate::config::Config;
use crate::error::{AppError, AppResult};
use crate::hf::ModelKind;
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const RECIPE_FILE: &str = "docker-recipe.json";

/// The container-internal port each runtime's server listens on.
const VLLM_CONTAINER_PORT: u16 = 8000;
const LLAMACPP_CONTAINER_PORT: u16 = 8080;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "runtime", rename_all = "snake_case")]
pub enum RecipeParams {
    Vllm {
        served_model_name: String,
        tensor_parallel_size: u32,
        gpu_memory_utilization: f32,
        max_model_len: Option<u32>,
    },
    LlamaCpp {
        /// Which `.gguf` in the repo to serve (repos often ship several quants).
        gguf_file: String,
        ctx_size: u32,
        /// Layers offloaded to GPU; llama.cpp's own default-everything sentinel.
        n_gpu_layers: i32,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recipe {
    pub image: String,
    /// Host GPU to bind via CDI (`nvidia.com/gpu=<index>`); `None` runs on CPU.
    pub gpu_index: Option<u32>,
    pub host_port: u16,
    /// Extra flags appended verbatim (shell-quoted) to the server command.
    #[serde(default)]
    pub extra_args: String,
    pub params: RecipeParams,
}

impl Recipe {
    fn container_port(&self) -> u16 {
        match &self.params {
            RecipeParams::Vllm { .. } => VLLM_CONTAINER_PORT,
            RecipeParams::LlamaCpp { .. } => LLAMACPP_CONTAINER_PORT,
        }
    }
}

/// A sensible starting recipe for a freshly downloaded repo.
pub fn default_recipe(kind: ModelKind, repo_id: &str, files: &[String]) -> Recipe {
    match kind {
        ModelKind::Safetensors => Recipe {
            image: "vllm/vllm-openai:latest".into(),
            gpu_index: None,
            host_port: VLLM_CONTAINER_PORT,
            extra_args: String::new(),
            params: RecipeParams::Vllm {
                served_model_name: repo_id.to_string(),
                tensor_parallel_size: 1,
                gpu_memory_utilization: 0.90,
                max_model_len: None,
            },
        },
        // Datasets are rejected at the API boundary; fall back to the llama.cpp shape.
        ModelKind::Gguf | ModelKind::Dataset => {
            let gguf_file = files
                .iter()
                .filter(|f| f.to_ascii_lowercase().ends_with(".gguf"))
                .min()
                .cloned()
                .unwrap_or_default();
            Recipe {
                image: "ghcr.io/ggml-org/llama.cpp:server".into(),
                gpu_index: None,
                host_port: LLAMACPP_CONTAINER_PORT,
                extra_args: String::new(),
                params: RecipeParams::LlamaCpp { gguf_file, ctx_size: 8192, n_gpu_layers: 999 },
            }
        }
    }
}

pub async fn load_or_default(
    dir: &Path,
    kind: ModelKind,
    repo_id: &str,
    files: &[String],
) -> Recipe {
    match tokio::fs::read(dir.join(RECIPE_FILE)).await {
        Ok(body) => match serde_json::from_slice(&body) {
            Ok(recipe) => recipe,
            Err(err) => {
                tracing::warn!(%err, "stored recipe is unreadable — using the default instead");
                default_recipe(kind, repo_id, files)
            }
        },
        Err(_) => default_recipe(kind, repo_id, files),
    }
}

pub async fn save(dir: &Path, recipe: &Recipe) -> AppResult<()> {
    validate(recipe)?;
    let body = serde_json::to_vec_pretty(recipe).expect("recipe is serializable");
    tokio::fs::create_dir_all(dir).await?;
    tokio::fs::write(dir.join(RECIPE_FILE), body).await?;
    Ok(())
}

fn validate(recipe: &Recipe) -> AppResult<()> {
    if recipe.image.trim().is_empty() {
        return Err(AppError::BadRequest("recipe image must not be empty".into()));
    }
    if recipe.host_port == 0 {
        return Err(AppError::BadRequest("host_port must be non-zero".into()));
    }
    shell_words::split(&recipe.extra_args)
        .map_err(|err| AppError::BadRequest(format!("extra_args: {err}")))?;
    match &recipe.params {
        RecipeParams::Vllm { served_model_name, gpu_memory_utilization, .. } => {
            if served_model_name.trim().is_empty() {
                return Err(AppError::BadRequest("served_model_name must not be empty".into()));
            }
            if !(0.0..=1.0).contains(gpu_memory_utilization) {
                return Err(AppError::BadRequest("gpu_memory_utilization must be in 0..1".into()));
            }
        }
        RecipeParams::LlamaCpp { gguf_file, .. } => {
            if gguf_file.trim().is_empty() {
                return Err(AppError::BadRequest("gguf_file must not be empty".into()));
            }
        }
    }
    Ok(())
}

/// Container name derived from the repo id, so `podman ps` doubles as our registry.
pub fn container_name(kind: ModelKind, repo_id: &str) -> String {
    let slug: String =
        repo_id.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    format!("mm-{}-{}", kind.as_str(), slug.to_ascii_lowercase())
}

pub const LABEL_MARKER: &str = "model-manager";
pub const LABEL_REPO: &str = "model-manager.repo";
pub const LABEL_KIND: &str = "model-manager.kind";
pub const LABEL_GPU: &str = "model-manager.gpu";

/// Build the full `podman run` argv for this recipe. Never touches a shell,
/// so nothing in `extra_args` or the repo id can break out of its argument.
pub fn render_argv(
    cfg: &Config,
    kind: ModelKind,
    repo_id: &str,
    model_dir: &Path,
    recipe: &Recipe,
) -> AppResult<Vec<String>> {
    let extra: Vec<String> = shell_words::split(&recipe.extra_args)
        .map_err(|err| AppError::BadRequest(format!("extra_args: {err}")))?;
    let container_port = recipe.container_port();

    let mut argv = vec![
        cfg.container_runtime.clone(),
        "run".into(),
        "-d".into(),
        "--rm".into(),
        "--name".into(),
        container_name(kind, repo_id),
        "--label".into(),
        format!("{LABEL_MARKER}=1"),
        "--label".into(),
        format!("{LABEL_REPO}={repo_id}"),
        "--label".into(),
        format!("{LABEL_KIND}={}", kind.as_str()),
    ];
    if let Some(gpu) = recipe.gpu_index {
        argv.push("--device".into());
        argv.push(format!("nvidia.com/gpu={gpu}"));
        argv.push("--label".into());
        argv.push(format!("{LABEL_GPU}={gpu}"));
    }
    argv.push("-p".into());
    argv.push(format!("{}:{container_port}", recipe.host_port));
    argv.push("-v".into());
    argv.push(format!("{}:/model:ro", model_dir.display()));
    argv.push(recipe.image.clone());

    match &recipe.params {
        RecipeParams::Vllm {
            served_model_name,
            tensor_parallel_size,
            gpu_memory_utilization,
            max_model_len,
        } => {
            // The vllm-openai image's ENTRYPOINT is already `["vllm", "serve"]`.
            argv.push("/model".into());
            argv.push("--served-model-name".into());
            argv.push(served_model_name.clone());
            argv.push("--port".into());
            argv.push(container_port.to_string());
            argv.push("--tensor-parallel-size".into());
            argv.push(tensor_parallel_size.to_string());
            argv.push("--gpu-memory-utilization".into());
            argv.push(gpu_memory_utilization.to_string());
            if let Some(len) = max_model_len {
                argv.push("--max-model-len".into());
                argv.push(len.to_string());
            }
        }
        RecipeParams::LlamaCpp { gguf_file, ctx_size, n_gpu_layers } => {
            // The image's ENTRYPOINT is already `/app/llama-server`.
            argv.push("-m".into());
            argv.push(format!("/model/{gguf_file}"));
            argv.push("--host".into());
            argv.push("0.0.0.0".into());
            argv.push("--port".into());
            argv.push(container_port.to_string());
            argv.push("-c".into());
            argv.push(ctx_size.to_string());
            argv.push("-ngl".into());
            argv.push(n_gpu_layers.to_string());
        }
    }
    argv.extend(extra);
    Ok(argv)
}

/// Render as a copy-pasteable shell command (for display only — launching
/// itself always goes through `render_argv` + `Command`, never a shell).
pub fn render_shell(argv: &[String]) -> String {
    shell_words::join(argv)
}
