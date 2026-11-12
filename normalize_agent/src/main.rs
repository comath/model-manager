//! normalize-agent manager: turns finished dataset downloads into normalized parquet.
//!
//! model-manager POSTs every completed dataset download to /jobs. For each job this starts a
//! dedicated, ephemeral Incus container that sees exactly two folders:
//!
//!     /data/raw   the downloaded dataset, <datasets>/<source>/<owner>/<name>    (read-only)
//!     /data/out   a fresh, empty staging folder                                 (read/write)
//!
//! and runs the agent command in it. The agent never sees other datasets, other normalized
//! output, or the previous normalized copy of this dataset. When it exits 0, the container is
//! deleted, the staging folder is checked (README.md plus at least one valid parquet file; no
//! symlinks, special files or hard links; modes reset, ownership handed to --owner) and moved
//! into place:
//!
//!     <datasets>/normalized/<source>/<owner>/<name>/
//!
//! Logs go to <datasets>/normalized/.logs/<job id>.log, staging to <datasets>/normalized/.work/.
//! The incus CLI does the container work.

mod check;
mod manager;
mod server;

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use clap::Parser;
use tracing::{info, warn};

pub const SOURCES: [&str; 3] = ["huggingface", "kaggle", "github"];
pub const NORMALIZED: &str = "normalized";
pub const WORK: &str = ".work";
pub const LOGS: &str = ".logs";
pub const STATUS_FILE: &str = ".normalize.json";
pub const CONTAINER_PREFIX: &str = "mm-normalize-";
pub const RAW_MOUNT: &str = "/data/raw";
pub const OUT_MOUNT: &str = "/data/out";
pub const MAX_BODY: usize = 64 * 1024;
pub const KEEP_JOBS: usize = 200;
pub const INCUS_STEP_TIMEOUT_SECS: u64 = 300;

pub type Owner = Option<(u32, u32)>;

pub struct Settings {
    pub datasets_root: PathBuf,
    pub image: String,
    pub command: Vec<String>,
    pub profiles: Vec<String>,
    pub container_config: BTreeMap<String, String>,
    pub env: BTreeMap<String, String>,
    pub timeout: u64,
    pub owner: Owner,
    pub incus: String,
    pub keep_failed: bool,
}

impl Settings {
    pub fn normalized(&self) -> PathBuf {
        self.datasets_root.join(NORMALIZED)
    }
}

fn parse_pair(s: &str) -> Result<(String, String), String> {
    match s.split_once('=') {
        Some((k, v)) if !k.is_empty() => Ok((k.to_string(), v.to_string())),
        _ => Err(format!("expected KEY=VALUE, got {s:?}")),
    }
}

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Listen address (host:port)
    #[arg(long, default_value = "127.0.0.1:8088")]
    bind: String,
    /// model-manager's datasets folder (MODEL_MANAGER_DATASETS_DIR)
    #[arg(long)]
    datasets_root: PathBuf,
    /// Incus image the agent runs in
    #[arg(long)]
    image: String,
    /// Agent command inside the container (whitespace-separated argv)
    #[arg(long, default_value = "normalize-dataset")]
    command: String,
    /// Incus profile for the container (repeatable; default: Incus' default)
    #[arg(long = "profile")]
    profiles: Vec<String>,
    /// Incus instance config, e.g. limits.memory=32GiB (repeatable)
    #[arg(long = "config", value_name = "KEY=VALUE", value_parser = parse_pair)]
    config: Vec<(String, String)>,
    /// Environment for the agent (repeatable)
    #[arg(long = "env", value_name = "KEY=VALUE", value_parser = parse_pair)]
    env: Vec<(String, String)>,
    /// Pass this variable from the manager's environment to the agent (repeatable)
    #[arg(long = "forward-env", value_name = "NAME")]
    forward_env: Vec<String>,
    /// Seconds an agent run may take
    #[arg(long, default_value_t = 4 * 3600)]
    timeout: u64,
    /// Agents running at once
    #[arg(long, default_value_t = 1)]
    workers: usize,
    /// user[:group] that owns the normalized output
    #[arg(long)]
    owner: Option<String>,
    /// incus CLI binary
    #[arg(long, default_value = "incus")]
    incus: String,
    /// Keep staging folders of failed jobs
    #[arg(long)]
    keep_failed: bool,
    #[arg(long, default_value = "info")]
    log_level: String,
}

fn parse_owner(spec: Option<&str>) -> Result<Owner> {
    let Some(spec) = spec.filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let (user, group) = match spec.split_once(':') {
        Some((u, g)) => (u, Some(g).filter(|g| !g.is_empty())),
        None => (spec, None),
    };
    let u = nix::unistd::User::from_name(user)?.with_context(|| format!("unknown user {user:?}"))?;
    let gid = match group {
        Some(g) => nix::unistd::Group::from_name(g)?
            .with_context(|| format!("unknown group {g:?}"))?
            .gid,
        None => u.gid,
    };
    Ok(Some((u.uid.as_raw(), gid.as_raw())))
}

/// Create `path` and any missing parents as 0755, owned by `owner`.
pub fn makedirs_owned(path: &Path, owner: Owner) -> std::io::Result<()> {
    use std::os::unix::fs::{PermissionsExt, chown};
    let mut missing = Vec::new();
    let mut p = path;
    while !p.exists() {
        missing.push(p);
        match p.parent() {
            Some(parent) => p = parent,
            None => break,
        }
    }
    std::fs::create_dir_all(path)?;
    for p in missing {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755))?;
        if let Some((uid, gid)) = owner {
            chown(p, Some(uid), Some(gid))?;
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(&args.log_level))
        .init();

    let mut env: BTreeMap<String, String> = args.env.into_iter().collect();
    for name in &args.forward_env {
        match std::env::var(name) {
            Ok(v) if !v.is_empty() => {
                env.insert(name.clone(), v);
            }
            _ => warn!("--forward-env {name}: not set in the environment"),
        }
    }
    if !args.datasets_root.is_absolute() {
        bail!("--datasets-root must be absolute");
    }
    let settings = Settings {
        datasets_root: args.datasets_root,
        image: args.image,
        command: args.command.split_whitespace().map(String::from).collect(),
        profiles: args.profiles,
        container_config: args.config.into_iter().collect(),
        env,
        timeout: args.timeout,
        owner: parse_owner(args.owner.as_deref())?,
        incus: args.incus,
        keep_failed: args.keep_failed,
    };
    if settings.command.is_empty() {
        bail!("--command must not be empty");
    }
    makedirs_owned(&settings.normalized(), settings.owner)?;

    let manager = manager::Manager::start(settings, args.workers.max(1));
    manager.recover().await;

    let listener = tokio::net::TcpListener::bind(&args.bind)
        .await
        .with_context(|| format!("binding {}", args.bind))?;
    info!(
        "listening on {}; datasets {}, image {}",
        args.bind,
        manager.settings().datasets_root.display(),
        manager.settings().image
    );
    axum::serve(listener, server::router(Arc::clone(&manager))).await?;
    Ok(())
}
