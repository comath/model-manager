//! The on-disk library: metadata sidecars, scanning, deletion.

use crate::config::Config;
use crate::error::{AppError, AppResult};
use crate::hf::{ModelKind, validate_repo_id};
use crate::recipe;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const META_FILE: &str = "model-manager.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metadata {
    pub repo_id: String,
    pub revision: String,
    pub kind: ModelKind,
    pub files: Vec<String>,
    pub downloaded_at: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LibraryEntry {
    pub repo_id: String,
    pub revision: String,
    pub kind: ModelKind,
    pub path: String,
    pub size_bytes: u64,
    pub file_count: usize,
    pub downloaded_at: u64,
    /// Ready-to-paste command for the matching runtime.
    pub serve_command: String,
    /// Weight-file bytes only (`.safetensors` / `.gguf`) — a floor on the VRAM
    /// needed to load the model, excluding KV-cache/activation overhead.
    pub estimated_vram_bytes: u64,
    pub files: Vec<String>,
}

pub async fn write_metadata(
    dir: &Path,
    repo_id: &str,
    revision: &str,
    kind: ModelKind,
    files: &[String],
) -> std::io::Result<()> {
    // A repo can be filled in over several jobs (another quant, a missing config),
    // so union the new file list with whatever the sidecar already records.
    let mut all: Vec<String> = match read_metadata(dir).await {
        Some(previous) => previous.files,
        None => Vec::new(),
    };
    all.extend(files.iter().cloned());
    all.sort();
    all.dedup();

    let meta = Metadata {
        repo_id: repo_id.to_string(),
        revision: revision.to_string(),
        kind,
        files: all,
        downloaded_at: crate::download::now_ms(),
    };
    let body = serde_json::to_vec_pretty(&meta).expect("metadata is serializable");
    tokio::fs::create_dir_all(dir).await?;
    tokio::fs::write(dir.join(META_FILE), body).await
}

pub async fn list(cfg: &Config) -> AppResult<Vec<LibraryEntry>> {
    let mut entries = Vec::new();
    for kind in [ModelKind::Safetensors, ModelKind::Gguf] {
        let root = cfg.models_dir().join(kind.as_str());
        collect(&root, 2, &mut entries).await?;
    }
    entries.sort_by_key(|e| std::cmp::Reverse(e.downloaded_at));
    Ok(entries)
}

/// Walk at most `depth` levels below `dir` looking for metadata sidecars.
async fn collect(dir: &Path, depth: usize, out: &mut Vec<LibraryEntry>) -> AppResult<()> {
    let mut stack = vec![(dir.to_path_buf(), depth)];
    while let Some((current, remaining)) = stack.pop() {
        let Ok(mut rd) = tokio::fs::read_dir(&current).await else { continue };
        while let Some(child) = rd.next_entry().await? {
            if !child.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let path = child.path();
            match read_metadata(&path).await {
                Some(meta) => out.push(entry_for(&path, meta).await),
                None if remaining > 0 => stack.push((path, remaining - 1)),
                None => {}
            }
        }
    }
    Ok(())
}

async fn read_metadata(dir: &Path) -> Option<Metadata> {
    let body = tokio::fs::read(dir.join(META_FILE)).await.ok()?;
    serde_json::from_slice(&body).ok()
}

/// Metadata for one repo already in the library, keyed by kind + repo id.
pub async fn get_metadata(cfg: &Config, kind: ModelKind, repo_id: &str) -> AppResult<Metadata> {
    validate_repo_id(repo_id)?;
    let dir = cfg.repo_dir(kind, repo_id);
    read_metadata(&dir)
        .await
        .ok_or_else(|| AppError::NotFound(format!("{repo_id} is not in the library")))
}

async fn entry_for(path: &Path, meta: Metadata) -> LibraryEntry {
    let (size_bytes, file_count, estimated_vram_bytes) = dir_stats(path).await;
    LibraryEntry {
        serve_command: serve_command(path, &meta).await,
        repo_id: meta.repo_id,
        revision: meta.revision,
        kind: meta.kind,
        path: path.display().to_string(),
        size_bytes,
        file_count,
        downloaded_at: meta.downloaded_at,
        estimated_vram_bytes,
        files: meta.files,
    }
}

async fn serve_command(path: &Path, meta: &Metadata) -> String {
    match meta.kind {
        ModelKind::Safetensors => {
            format!("vllm serve {} --served-model-name {}", path.display(), meta.repo_id)
        }
        ModelKind::Gguf => {
            // Point llama.cpp at the first shard; it picks up the rest itself.
            let first = meta
                .files
                .iter()
                .filter(|f| f.to_ascii_lowercase().ends_with(".gguf"))
                .min()
                .cloned()
                .unwrap_or_default();
            format!("llama-server -m {} -c 8192", path.join(first).display())
        }
    }
}

/// Recursive `(total bytes, file count, weight-file bytes)`, ignoring
/// in-flight `.part` downloads and our own sidecars.
async fn dir_stats(root: &Path) -> (u64, usize, u64) {
    let (mut bytes, mut files, mut weight_bytes) = (0u64, 0usize, 0u64);
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(mut rd) = tokio::fs::read_dir(&dir).await else { continue };
        while let Ok(Some(child)) = rd.next_entry().await {
            let Ok(ft) = child.file_type().await else { continue };
            if ft.is_dir() {
                stack.push(child.path());
            } else if ft.is_file() {
                let name = child.file_name();
                let name = name.to_string_lossy();
                if name == META_FILE || name == recipe::RECIPE_FILE || name.ends_with(".part") {
                    continue;
                }
                if let Ok(meta) = child.metadata().await {
                    bytes += meta.len();
                    files += 1;
                    let lower = name.to_ascii_lowercase();
                    if lower.ends_with(".safetensors") || lower.ends_with(".gguf") {
                        weight_bytes += meta.len();
                    }
                }
            }
        }
    }
    (bytes, files, weight_bytes)
}

pub async fn delete(cfg: &Config, kind: ModelKind, repo_id: &str) -> AppResult<()> {
    validate_repo_id(repo_id)?;
    let dir = cfg.repo_dir(kind, repo_id);
    let models_dir = cfg.models_dir();
    // Belt and braces: never remove anything outside the models directory.
    if !dir.starts_with(&models_dir) || dir == models_dir {
        return Err(AppError::BadRequest("refusing to delete outside the models dir".into()));
    }
    if !dir.join(META_FILE).exists() {
        return Err(AppError::NotFound(format!("{repo_id} is not in the library")));
    }
    tokio::fs::remove_dir_all(&dir).await?;
    // Prune empty `<owner>` dirs, but keep the per-kind roots in place.
    prune_empty_parents(&dir, &models_dir.join(kind.as_str())).await;
    Ok(())
}

/// Remove now-empty `<owner>` directories left behind by a deletion.
async fn prune_empty_parents(from: &Path, stop: &Path) {
    let mut current: PathBuf = from.to_path_buf();
    while let Some(parent) = current.parent().map(Path::to_path_buf) {
        if !parent.starts_with(stop) || parent == stop {
            break;
        }
        match tokio::fs::read_dir(&parent).await {
            Ok(mut rd) => match rd.next_entry().await {
                Ok(None) => {
                    if tokio::fs::remove_dir(&parent).await.is_err() {
                        break;
                    }
                }
                _ => break,
            },
            Err(_) => break,
        }
        current = parent;
    }
}
