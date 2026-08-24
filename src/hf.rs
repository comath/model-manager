//! Thin client for the public HuggingFace Hub API.

use crate::error::{AppError, AppResult};
use serde::{Deserialize, Serialize};

pub const HF_ENDPOINT: &str = "https://huggingface.co";

/// The two runtimes we care about, and therefore the two file flavours.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelKind {
    /// `*.safetensors` weights + configs, laid out as vLLM expects.
    Safetensors,
    /// One or more `*.gguf` shards for llama.cpp.
    Gguf,
}

impl ModelKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ModelKind::Safetensors => "safetensors",
            ModelKind::Gguf => "gguf",
        }
    }

    /// Hub search filter that narrows results to repos containing this flavour.
    fn search_filter(self) -> &'static str {
        match self {
            ModelKind::Safetensors => "safetensors",
            ModelKind::Gguf => "gguf",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "safetensors" | "vllm" => Some(ModelKind::Safetensors),
            "gguf" | "llamacpp" | "llama.cpp" => Some(ModelKind::Gguf),
            _ => None,
        }
    }

    /// Does this file belong to a download of this kind?
    pub fn wants(self, path: &str) -> bool {
        let lower = path.to_ascii_lowercase();
        let name = lower.rsplit('/').next().unwrap_or(&lower);
        match self {
            ModelKind::Gguf => lower.ends_with(".gguf"),
            ModelKind::Safetensors => {
                lower.ends_with(".safetensors")
                    || matches!(
                        name,
                        "config.json"
                            | "generation_config.json"
                            | "tokenizer.json"
                            | "tokenizer.model"
                            | "tokenizer_config.json"
                            | "special_tokens_map.json"
                            | "preprocessor_config.json"
                            | "processor_config.json"
                            | "chat_template.jinja"
                            | "chat_template.json"
                            | "model.safetensors.index.json"
                            | "vocab.json"
                            | "merges.txt"
                    )
            }
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelSummary {
    pub id: String,
    pub downloads: u64,
    pub likes: u64,
    pub pipeline_tag: Option<String>,
    pub last_modified: Option<String>,
    pub gated: bool,
    pub tags: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawModel {
    id: String,
    #[serde(default)]
    downloads: u64,
    #[serde(default)]
    likes: u64,
    #[serde(default)]
    pipeline_tag: Option<String>,
    #[serde(default, rename = "lastModified")]
    last_modified: Option<String>,
    #[serde(default)]
    gated: serde_json::Value,
    #[serde(default)]
    tags: Vec<String>,
}

impl From<RawModel> for ModelSummary {
    fn from(r: RawModel) -> Self {
        ModelSummary {
            id: r.id,
            downloads: r.downloads,
            likes: r.likes,
            pipeline_tag: r.pipeline_tag,
            last_modified: r.last_modified,
            // `gated` is `false` or one of "auto" / "manual".
            gated: !matches!(r.gated, serde_json::Value::Bool(false) | serde_json::Value::Null),
            tags: r.tags,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoFile {
    pub path: String,
    pub size: u64,
    /// True when the file is one this `ModelKind` actually needs.
    pub relevant: bool,
}

#[derive(Debug, Deserialize)]
struct RawTreeEntry {
    #[serde(rename = "type")]
    kind: String,
    path: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    lfs: Option<RawLfs>,
}

#[derive(Debug, Deserialize)]
struct RawLfs {
    #[serde(default)]
    size: u64,
}

#[derive(Clone)]
pub struct HfClient {
    http: reqwest::Client,
    token: Option<String>,
}

impl HfClient {
    pub fn new(http: reqwest::Client, token: Option<String>) -> Self {
        Self { http, token }
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    pub fn authorize(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.token {
            Some(t) => req.bearer_auth(t),
            None => req,
        }
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> AppResult<T> {
        let resp = self.authorize(self.http.get(url)).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let snippet: String = body.chars().take(300).collect();
            return Err(match status.as_u16() {
                404 => AppError::NotFound(format!("not found on the Hub ({url})")),
                401 | 403 => AppError::Upstream(format!(
                    "{status} — the repo is gated or private; set HF_TOKEN and restart"
                )),
                _ => AppError::Upstream(format!("{status}: {snippet}")),
            });
        }
        Ok(resp.json::<T>().await?)
    }

    /// Full-text search, restricted to repos that carry the requested flavour.
    pub async fn search(
        &self,
        query: &str,
        kind: ModelKind,
        limit: usize,
    ) -> AppResult<Vec<ModelSummary>> {
        let url = format!(
            "{HF_ENDPOINT}/api/models?search={}&filter={}&sort=downloads&direction=-1&limit={}",
            urlencoding::encode(query.trim()),
            kind.search_filter(),
            limit.clamp(1, 100),
        );
        let raw: Vec<RawModel> = self.get_json(&url).await?;
        Ok(raw.into_iter().map(ModelSummary::from).collect())
    }

    pub async fn model_info(&self, repo_id: &str, revision: &str) -> AppResult<ModelSummary> {
        let url = format!(
            "{HF_ENDPOINT}/api/models/{}/revision/{}",
            repo_id,
            urlencoding::encode(revision)
        );
        let raw: RawModel = self.get_json(&url).await?;
        Ok(raw.into())
    }

    /// Recursive file listing with real (LFS-aware) sizes.
    pub async fn list_files(
        &self,
        repo_id: &str,
        revision: &str,
        kind: ModelKind,
    ) -> AppResult<Vec<RepoFile>> {
        let url = format!(
            "{HF_ENDPOINT}/api/models/{}/tree/{}?recursive=true&expand=true",
            repo_id,
            urlencoding::encode(revision)
        );
        let raw: Vec<RawTreeEntry> = self.get_json(&url).await?;
        let mut files: Vec<RepoFile> = raw
            .into_iter()
            .filter(|e| e.kind == "file")
            .map(|e| RepoFile {
                relevant: kind.wants(&e.path),
                size: e.lfs.as_ref().map(|l| l.size).filter(|s| *s > 0).unwrap_or(e.size),
                path: e.path,
            })
            .collect();
        files.sort_by(|a, b| b.relevant.cmp(&a.relevant).then_with(|| a.path.cmp(&b.path)));
        Ok(files)
    }

    pub fn resolve_url(&self, repo_id: &str, revision: &str, path: &str) -> String {
        let encoded_path = path
            .split('/')
            .map(|seg| urlencoding::encode(seg).into_owned())
            .collect::<Vec<_>>()
            .join("/");
        format!(
            "{HF_ENDPOINT}/{}/resolve/{}/{}",
            repo_id,
            urlencoding::encode(revision),
            encoded_path
        )
    }
}

/// Reject anything that is not a plain `owner/name` (or bare `name`) repo id.
pub fn validate_repo_id(repo_id: &str) -> AppResult<()> {
    let ok = !repo_id.is_empty()
        && repo_id.len() <= 200
        && repo_id.matches('/').count() <= 1
        && !repo_id.starts_with('/')
        && !repo_id.ends_with('/')
        && repo_id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
        && !repo_id.contains("..");
    ok.then_some(()).ok_or_else(|| AppError::BadRequest(format!("invalid repo id: {repo_id:?}")))
}
