//! The on-disk library: metadata sidecars, scanning, deletion.

use crate::config::{Config, Store};
use crate::error::{AppError, AppResult};
use crate::hf::{ModelKind, validate_repo_id};
use crate::recipe;
use crate::source::Source;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const META_FILE: &str = "model-manager.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metadata {
    pub repo_id: String,
    pub revision: String,
    pub kind: ModelKind,
    /// Where it was fetched from; only datasets vary. Older sidecars have none.
    #[serde(default)]
    pub source: Source,
    pub files: Vec<String>,
    pub downloaded_at: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LibraryEntry {
    /// Name of the store (see `MODEL_MANAGER_STORES`) this copy lives in.
    pub store: String,
    pub repo_id: String,
    pub revision: String,
    pub kind: ModelKind,
    pub source: Source,
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
    source: Source,
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
        source,
        files: all,
        downloaded_at: crate::download::now_ms(),
    };
    let body = serde_json::to_vec_pretty(&meta).expect("metadata is serializable");
    tokio::fs::create_dir_all(dir).await?;
    tokio::fs::write(dir.join(META_FILE), body).await
}

pub async fn list(cfg: &Config) -> AppResult<Vec<LibraryEntry>> {
    let mut entries = Vec::new();
    for kind in [ModelKind::Safetensors, ModelKind::Gguf, ModelKind::Dataset] {
        for store in cfg.stores_for(kind) {
            collect(store, &store.kind_root(kind), 2, &mut entries).await?;
        }
    }
    entries.sort_by_key(|e| std::cmp::Reverse(e.downloaded_at));
    Ok(entries)
}

/// Walk at most `depth` levels below `dir` looking for metadata sidecars.
async fn collect(
    store: &Store,
    dir: &Path,
    depth: usize,
    out: &mut Vec<LibraryEntry>,
) -> AppResult<()> {
    let mut stack = vec![(dir.to_path_buf(), depth)];
    while let Some((current, remaining)) = stack.pop() {
        let Ok(mut rd) = tokio::fs::read_dir(&current).await else { continue };
        while let Some(child) = rd.next_entry().await? {
            if !child.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let path = child.path();
            match read_metadata(&path).await {
                Some(meta) => out.push(entry_for(store, &path, meta).await),
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

/// Find a repo already in the library: in the named store, or (no name) the first store
/// that has it. Returns the store, the repo directory and its metadata.
pub async fn locate<'a>(
    cfg: &'a Config,
    store: Option<&str>,
    kind: ModelKind,
    source: Source,
    repo_id: &str,
) -> AppResult<(&'a Store, PathBuf, Metadata)> {
    validate_repo_id(repo_id)?;
    let candidates: Vec<&Store> = match store.map(str::trim).filter(|n| !n.is_empty()) {
        Some(_) => vec![cfg.store(kind, store)?],
        None => cfg.stores_for(kind).iter().collect(),
    };
    for store in candidates {
        let dir = store.repo_dir(kind, source, repo_id);
        if let Some(meta) = read_metadata(&dir).await {
            return Ok((store, dir, meta));
        }
    }
    Err(AppError::NotFound(format!("{repo_id} is not in the library")))
}

async fn entry_for(store: &Store, path: &Path, meta: Metadata) -> LibraryEntry {
    let (size_bytes, file_count, estimated_vram_bytes) = dir_stats(path).await;
    LibraryEntry {
        store: store.name.clone(),
        serve_command: serve_command(path, &meta).await,
        repo_id: meta.repo_id,
        revision: meta.revision,
        kind: meta.kind,
        source: meta.source,
        path: path.display().to_string(),
        size_bytes,
        file_count,
        downloaded_at: meta.downloaded_at,
        estimated_vram_bytes: if meta.kind == ModelKind::Dataset {
            0
        } else {
            estimated_vram_bytes
        },
        files: meta.files,
    }
}

async fn serve_command(path: &Path, meta: &Metadata) -> String {
    match meta.kind {
        ModelKind::Safetensors => {
            format!("vllm serve {} --served-model-name {}", path.display(), meta.repo_id)
        }
        // Nothing to serve; the UI hides the command box for datasets.
        ModelKind::Dataset => String::new(),
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

pub async fn delete(
    cfg: &Config,
    store: Option<&str>,
    kind: ModelKind,
    source: Source,
    repo_id: &str,
) -> AppResult<()> {
    let (store, dir, _) = locate(cfg, store, kind, source, repo_id).await?;
    let kind_root = store.kind_root(kind);
    // Belt and braces: never remove anything outside this store's per-kind directory.
    if !dir.starts_with(&kind_root) || dir == kind_root {
        return Err(AppError::BadRequest("refusing to delete outside the models dir".into()));
    }
    tokio::fs::remove_dir_all(&dir).await?;
    // Prune empty `<owner>` dirs, but keep the per-kind roots in place.
    prune_empty_parents(&dir, &kind_root).await;
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
