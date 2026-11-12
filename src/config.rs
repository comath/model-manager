use crate::error::{AppError, AppResult};
use std::path::{Path, PathBuf};

/// How we authenticate to Kaggle: legacy username + key, or a bearer API token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KaggleAuth {
    Basic { username: String, key: String },
    Bearer(String),
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// `KAGGLE_USERNAME` + `KAGGLE_KEY`, `KAGGLE_API_TOKEN`, or `~/.kaggle/kaggle.json`.
fn kaggle_auth_from_env() -> Option<KaggleAuth> {
    if let (Some(username), Some(key)) =
        (env_nonempty("KAGGLE_USERNAME"), env_nonempty("KAGGLE_KEY"))
    {
        return Some(KaggleAuth::Basic { username, key });
    }
    if let Some(token) = env_nonempty("KAGGLE_API_TOKEN") {
        return Some(KaggleAuth::Bearer(token));
    }
    let body = std::fs::read(dirs::home_dir()?.join(".kaggle").join("kaggle.json")).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&body).ok()?;
    Some(KaggleAuth::Basic {
        username: v.get("username")?.as_str()?.to_string(),
        key: v.get("key")?.as_str()?.to_string(),
    })
}

/// A named root that models are downloaded into, e.g. `fast=/fast/models`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Store {
    pub name: String,
    pub root: PathBuf,
}

impl Store {
    /// Directory holding every repo of this kind: `<root>/<kind>` for models; the
    /// datasets store *is* the datasets folder, so it adds no extra level.
    pub fn kind_root(&self, kind: crate::hf::ModelKind) -> PathBuf {
        match kind {
            crate::hf::ModelKind::Dataset => self.root.clone(),
            _ => self.root.join(kind.as_str()),
        }
    }

    /// Where a repo of a given kind lands: `<root>/<kind>/<owner>/<name>`
    /// (`<datasets dir>/<owner>/<name>` for datasets).
    pub fn repo_dir(
        &self,
        kind: crate::hf::ModelKind,
        source: crate::source::Source,
        repo_id: &str,
    ) -> PathBuf {
        let mut dir = self.kind_root(kind);
        if kind == crate::hf::ModelKind::Dataset {
            dir.push(source.as_str());
        }
        for part in repo_id.split('/').filter(|p| !p.is_empty()) {
            dir.push(sanitize(part));
        }
        dir
    }
}

/// Runtime configuration, resolved from environment variables at startup.
#[derive(Clone, Debug)]
pub struct Config {
    /// Root directory holding downloaded models and metadata.
    pub data_dir: PathBuf,
    /// Where models can be downloaded to. Never empty; the first one is the default.
    pub stores: Vec<Store>,
    /// Where datasets are downloaded (`MODEL_MANAGER_DATASETS_DIR`), as `<source>/<owner>/<name>`.
    pub datasets_store: Store,
    /// Kaggle credentials; public datasets work without.
    pub kaggle: Option<KaggleAuth>,
    /// GitHub token; raises rate limits and unlocks private repos.
    pub github_token: Option<String>,
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
    /// Path to TLS server certificate (PEM).
    pub tls_cert: Option<PathBuf>,
    /// Path to TLS server private key (PEM).
    pub tls_key: Option<PathBuf>,
    /// Path to CA certificate (PEM) for verifying client certificates (mTLS).
    pub client_ca: Option<PathBuf>,
    /// Optional unauthenticated HTTP bind address for Tailscale connections.
    pub tailscale_bind: Option<String>,
    /// URL POSTed `{source, repo_id, revision, path}` after each completed dataset download
    /// (`MODEL_MANAGER_DATASET_HOOK`), e.g. the normalize-agent manager's `/jobs`.
    pub dataset_hook: Option<String>,
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

        let tls_cert = std::env::var_os("MODEL_MANAGER_TLS_CERT").map(PathBuf::from);
        let tls_key = std::env::var_os("MODEL_MANAGER_TLS_KEY").map(PathBuf::from);
        let client_ca = std::env::var_os("MODEL_MANAGER_CLIENT_CA").map(PathBuf::from);
        let tailscale_bind = ["MODEL_MANAGER_TAILSCALE_BIND", "MODEL_MANAGER_HTTP_BIND"]
            .iter()
            .find_map(|k| std::env::var(k).ok())
            .filter(|s| !s.trim().is_empty());

        let stores = match std::env::var("MODEL_MANAGER_STORES") {
            Ok(raw) if !raw.trim().is_empty() => parse_stores(&raw).unwrap_or_else(|err| {
                panic!("MODEL_MANAGER_STORES: {err}");
            }),
            _ => vec![Store { name: "default".into(), root: data_dir.join("models") }],
        };

        let datasets_root = std::env::var_os("MODEL_MANAGER_DATASETS_DIR")
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| data_dir.join("datasets"));
        if !datasets_root.is_absolute() {
            panic!(
                "MODEL_MANAGER_DATASETS_DIR: {} is not an absolute path",
                datasets_root.display()
            );
        }

        Self {
            stores,
            datasets_store: Store { name: "datasets".into(), root: datasets_root },
            kaggle: kaggle_auth_from_env(),
            github_token: env_nonempty("GITHUB_TOKEN").or_else(|| env_nonempty("GH_TOKEN")),
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
            tls_cert,
            tls_key,
            client_ca,
            tailscale_bind,
            dataset_hook: env_nonempty("MODEL_MANAGER_DATASET_HOOK"),
        }
    }

    pub fn is_tls_enabled(&self) -> bool {
        self.tls_cert.is_some() && self.tls_key.is_some()
    }

    pub fn is_mtls_enabled(&self) -> bool {
        self.is_tls_enabled() && self.client_ca.is_some()
    }

    /// The stores a repo of this kind can live in: the model stores, or just the datasets folder.
    pub fn stores_for(&self, kind: crate::hf::ModelKind) -> &[Store] {
        match kind {
            crate::hf::ModelKind::Dataset => std::slice::from_ref(&self.datasets_store),
            _ => &self.stores,
        }
    }

    /// The named store for this kind, or its default (first) one when no name is given.
    pub fn store(&self, kind: crate::hf::ModelKind, name: Option<&str>) -> AppResult<&Store> {
        let stores = self.stores_for(kind);
        match name.map(str::trim).filter(|n| !n.is_empty()) {
            None => Ok(&stores[0]),
            Some(name) => stores.iter().find(|s| s.name == name).ok_or_else(|| {
                let known: Vec<&str> = stores.iter().map(|s| s.name.as_str()).collect();
                AppError::BadRequest(format!("unknown store {name:?} ({})", known.join(" | ")))
            }),
        }
    }
}

/// Parse `name=/path,name=/path`. Names are short identifiers; paths must be absolute.
fn parse_stores(raw: &str) -> Result<Vec<Store>, String> {
    let mut stores: Vec<Store> = Vec::new();
    for item in raw.split(',').map(str::trim).filter(|i| !i.is_empty()) {
        let (name, root) =
            item.split_once('=').ok_or_else(|| format!("{item:?} is not name=/path"))?;
        let (name, root) = (name.trim(), PathBuf::from(root.trim()));
        if name.is_empty()
            || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(format!("store name {name:?} must be letters, digits, - or _"));
        }
        if !root.is_absolute() {
            return Err(format!("store {name:?}: {} is not an absolute path", root.display()));
        }
        if stores.iter().any(|s| s.name == name) {
            return Err(format!("store {name:?} is listed twice"));
        }
        stores.push(Store { name: name.to_string(), root });
    }
    if stores.is_empty() {
        return Err("no stores given".into());
    }
    Ok(stores)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tls_and_mtls_helpers() {
        let mut cfg = Config {
            data_dir: PathBuf::from("/tmp"),
            stores: vec![Store { name: "default".into(), root: PathBuf::from("/tmp/models") }],
            datasets_store: Store { name: "datasets".into(), root: PathBuf::from("/tmp/datasets") },
            kaggle: None,
            github_token: None,
            bind: "127.0.0.1:8087".into(),
            hf_token: None,
            parallel_files: 2,
            web_dir: PathBuf::from("web/dist"),
            container_runtime: "podman".into(),
            tls_cert: None,
            tls_key: None,
            client_ca: None,
            tailscale_bind: None,
        };

        assert!(!cfg.is_tls_enabled());
        assert!(!cfg.is_mtls_enabled());

        cfg.tls_cert = Some(PathBuf::from("/etc/certs/server.crt"));
        assert!(!cfg.is_tls_enabled()); // missing key

        cfg.tls_key = Some(PathBuf::from("/etc/certs/server.key"));
        assert!(cfg.is_tls_enabled());
        assert!(!cfg.is_mtls_enabled()); // missing client CA

        cfg.client_ca = Some(PathBuf::from("/etc/certs/ca.crt"));
        assert!(cfg.is_tls_enabled());
        assert!(cfg.is_mtls_enabled());
    }

    #[test]
    fn test_parse_stores() {
        let stores = parse_stores("fast=/fast/models, slow=/slow/models").unwrap();
        assert_eq!(stores.len(), 2);
        assert_eq!(stores[0], Store { name: "fast".into(), root: PathBuf::from("/fast/models") });
        assert_eq!(stores[1].name, "slow");

        assert!(parse_stores("fast").is_err());
        assert!(parse_stores("fast=relative/path").is_err());
        assert!(parse_stores("fa st=/x").is_err());
        assert!(parse_stores("a=/x,a=/y").is_err());
        assert!(parse_stores(" , ").is_err());
    }

    #[test]
    fn test_store_lookup_and_repo_dir() {
        let cfg = Config {
            data_dir: PathBuf::from("/tmp"),
            stores: parse_stores("fast=/fast/models,slow=/slow/models").unwrap(),
            datasets_store: Store {
                name: "datasets".into(),
                root: PathBuf::from("/data/datasets"),
            },
            kaggle: None,
            github_token: None,
            bind: "127.0.0.1:8087".into(),
            hf_token: None,
            parallel_files: 2,
            web_dir: PathBuf::from("web/dist"),
            container_runtime: "podman".into(),
            tls_cert: None,
            tls_key: None,
            client_ca: None,
            tailscale_bind: None,
        };
        assert_eq!(cfg.store(crate::hf::ModelKind::Gguf, None).unwrap().name, "fast");
        assert_eq!(cfg.store(crate::hf::ModelKind::Gguf, Some("")).unwrap().name, "fast");
        assert_eq!(cfg.store(crate::hf::ModelKind::Gguf, Some("slow")).unwrap().name, "slow");
        assert!(cfg.store(crate::hf::ModelKind::Gguf, Some("nope")).is_err());

        let dir = cfg.store(crate::hf::ModelKind::Gguf, Some("slow")).unwrap().repo_dir(
            crate::hf::ModelKind::Gguf,
            crate::source::Source::default(),
            "org/../m",
        );
        assert_eq!(dir, PathBuf::from("/slow/models/gguf/org/_/m"));

        let ds = cfg.store(crate::hf::ModelKind::Dataset, None).unwrap();
        assert_eq!(ds.name, "datasets");
        assert_eq!(
            ds.repo_dir(crate::hf::ModelKind::Dataset, crate::source::Source::Kaggle, "org/data"),
            PathBuf::from("/data/datasets/kaggle/org/data")
        );
        assert!(cfg.store(crate::hf::ModelKind::Dataset, Some("slow")).is_err());
    }
}
