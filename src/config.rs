use std::path::{Path, PathBuf};

/// Runtime configuration, resolved from environment variables at startup.
#[derive(Clone, Debug)]
pub struct Config {
    /// Root directory holding downloaded models and metadata.
    pub data_dir: PathBuf,
    /// Address the HTTP server binds to.
    pub bind: String,
    /// Optional HuggingFace token, used for gated/private repos.
    pub hf_token: Option<String>,
    /// How many files of a single job are fetched at once.
    pub parallel_files: usize,
    /// Directory containing the built frontend, served at `/`.
    pub web_dir: PathBuf,
    /// Container CLI used to launch/stop/inspect model runtimes (`podman` or `docker`).
    pub container_runtime: String,
}

impl Config {
    pub fn from_env() -> Self {
        let data_dir =
            std::env::var_os("MODEL_MANAGER_DATA_DIR").map(PathBuf::from).unwrap_or_else(|| {
                dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".model-manager")
            });

        let hf_token = ["HF_TOKEN", "HUGGING_FACE_HUB_TOKEN"]
            .iter()
            .find_map(|k| std::env::var(k).ok())
            .filter(|t| !t.trim().is_empty());

        Self {
            data_dir,
            bind: std::env::var("MODEL_MANAGER_BIND").unwrap_or_else(|_| "127.0.0.1:8087".into()),
            hf_token,
            parallel_files: std::env::var("MODEL_MANAGER_PARALLEL_FILES")
                .ok()
                .and_then(|v| v.parse().ok())
                .filter(|n| *n > 0)
                .unwrap_or(2),
            web_dir: std::env::var_os("MODEL_MANAGER_WEB_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("web/dist")),
            container_runtime: std::env::var("MODEL_MANAGER_CONTAINER_RUNTIME")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| "podman".into()),
        }
    }

    pub fn models_dir(&self) -> PathBuf {
        self.data_dir.join("models")
    }

    /// Where a repo of a given kind lands: `<data>/models/<kind>/<owner>/<name>`.
    pub fn repo_dir(&self, kind: crate::hf::ModelKind, repo_id: &str) -> PathBuf {
        let mut dir = self.models_dir().join(kind.as_str());
        for part in repo_id.split('/').filter(|p| !p.is_empty()) {
            dir.push(sanitize(part));
        }
        dir
    }
}

/// Keep path components inert: no separators, no traversal, no hidden files.
pub fn sanitize(part: &str) -> String {
    let cleaned: String = part
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '\0' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    match cleaned.trim() {
        "" | "." | ".." => "_".to_string(),
        s => s.to_string(),
    }
}

/// Join a repo-relative file path onto `base`, rejecting anything that escapes it.
pub fn safe_join(base: &Path, rel: &str) -> Option<PathBuf> {
    let mut out = base.to_path_buf();
    let mut pushed = false;
    for part in rel.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." || part.contains('\\') {
            return None;
        }
        out.push(sanitize(part));
        pushed = true;
    }
    pushed.then_some(out)
}
