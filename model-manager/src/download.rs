//! Download jobs: one job = one repo, N files, resumable, cancellable.

use crate::config::{Config, safe_join};
use crate::error::{AppError, AppResult};
use crate::hf::{HfClient, ModelKind};
use crate::source::{Hub, Source};
use crate::store;
use futures_util::StreamExt;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncWriteExt;
use tokio::sync::{RwLock, Semaphore, broadcast};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Canceled,
}

impl JobStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, JobStatus::Completed | JobStatus::Failed | JobStatus::Canceled)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            JobStatus::Queued => "queued",
            JobStatus::Running => "running",
            JobStatus::Completed => "completed",
            JobStatus::Failed => "failed",
            JobStatus::Canceled => "canceled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "queued" => JobStatus::Queued,
            "running" => JobStatus::Running,
            "completed" => JobStatus::Completed,
            "failed" => JobStatus::Failed,
            "canceled" => JobStatus::Canceled,
            _ => return None,
        })
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FileStatus {
    Pending,
    Downloading,
    /// Already present on disk with the expected size.
    Skipped,
    Done,
    Failed,
    Canceled,
}

struct FileTrack {
    path: String,
    size: AtomicU64,
    downloaded: AtomicU64,
    status: Mutex<FileStatus>,
    error: Mutex<Option<String>>,
}

pub struct Job {
    pub id: Uuid,
    pub repo_id: String,
    pub revision: String,
    pub kind: ModelKind,
    pub source: Source,
    pub store: String,
    pub dest: PathBuf,
    created_at: u64,
    files: Vec<Arc<FileTrack>>,
    status: Mutex<JobStatus>,
    error: Mutex<Option<String>>,
    cancel: CancellationToken,
}

#[derive(Serialize)]
pub struct FileView {
    pub path: String,
    pub size: u64,
    pub downloaded: u64,
    pub status: FileStatus,
    pub error: Option<String>,
}

#[derive(Serialize)]
pub struct JobView {
    pub id: Uuid,
    pub repo_id: String,
    pub revision: String,
    pub kind: ModelKind,
    pub source: Source,
    pub store: String,
    pub dest: String,
    pub status: JobStatus,
    pub error: Option<String>,
    pub created_at: u64,
    pub total_bytes: u64,
    pub downloaded_bytes: u64,
    pub files: Vec<FileView>,
}

impl Job {
    fn view(&self) -> JobView {
        let files: Vec<FileView> = self
            .files
            .iter()
            .map(|f| FileView {
                path: f.path.clone(),
                size: f.size.load(Ordering::Relaxed),
                downloaded: f.downloaded.load(Ordering::Relaxed),
                status: *f.status.lock().unwrap(),
                error: f.error.lock().unwrap().clone(),
            })
            .collect();
        JobView {
            id: self.id,
            repo_id: self.repo_id.clone(),
            revision: self.revision.clone(),
            kind: self.kind,
            source: self.source,
            store: self.store.clone(),
            dest: self.dest.display().to_string(),
            status: *self.status.lock().unwrap(),
            error: self.error.lock().unwrap().clone(),
            created_at: self.created_at,
            total_bytes: files.iter().map(|f| f.size).sum(),
            downloaded_bytes: files.iter().map(|f| f.downloaded).sum(),
            files,
        }
    }

    fn set_status(&self, status: JobStatus) {
        *self.status.lock().unwrap() = status;
    }

    fn row(&self) -> crate::db::JobRow {
        crate::db::JobRow {
            id: self.id,
            repo_id: self.repo_id.clone(),
            revision: self.revision.clone(),
            kind: self.kind,
            source: self.source,
            store: self.store.clone(),
            dest: self.dest.clone(),
            status: *self.status.lock().unwrap(),
            error: self.error.lock().unwrap().clone(),
            created_at: self.created_at,
            files: self
                .files
                .iter()
                .map(|f| (f.path.clone(), f.size.load(Ordering::Relaxed)))
                .collect(),
        }
    }

    /// A job loaded from the database. Unfinished ones come back queued, ready to run again;
    /// finished ones only keep their outcome (per-file detail isn't stored).
    fn from_row(row: crate::db::JobRow) -> Self {
        let status = if row.status.is_terminal() { row.status } else { JobStatus::Queued };
        let file_status = match status {
            JobStatus::Completed => FileStatus::Done,
            JobStatus::Failed => FileStatus::Failed,
            JobStatus::Canceled => FileStatus::Canceled,
            JobStatus::Queued | JobStatus::Running => FileStatus::Pending,
        };
        let done = status == JobStatus::Completed;
        Job {
            id: row.id,
            repo_id: row.repo_id,
            revision: row.revision,
            kind: row.kind,
            source: row.source,
            store: row.store,
            dest: row.dest,
            created_at: row.created_at,
            files: row
                .files
                .into_iter()
                .map(|(path, size)| {
                    Arc::new(FileTrack {
                        path,
                        size: AtomicU64::new(size),
                        downloaded: AtomicU64::new(if done { size } else { 0 }),
                        status: Mutex::new(file_status),
                        error: Mutex::new(None),
                    })
                })
                .collect(),
            status: Mutex::new(status),
            error: Mutex::new(row.error),
            cancel: CancellationToken::new(),
        }
    }
}

/// Everything the API layer talks to.
pub struct Manager {
    cfg: Config,
    hf: HfClient,
    hub: Hub,
    gpus: Vec<crate::gpu::GpuInfo>,
    jobs: RwLock<Vec<Arc<Job>>>,
    events: broadcast::Sender<String>,
    /// Service-wide download slots (`max_downloads`), shared by every job.
    slots: Arc<Semaphore>,
    /// Where jobs persist across restarts.
    db: crate::db::Db,
}

#[derive(Debug, Clone)]
pub struct NewJob {
    pub repo_id: String,
    pub revision: String,
    pub kind: ModelKind,
    pub source: Source,
    /// Store name; `None` means the default store.
    pub store: Option<String>,
    pub files: Vec<(String, u64)>,
}

impl Manager {
    pub fn new(
        cfg: Config,
        hf: HfClient,
        gpus: Vec<crate::gpu::GpuInfo>,
        db: crate::db::Db,
    ) -> Self {
        let (events, _) = broadcast::channel(256);
        let hub = Hub::new(hf.clone(), &cfg);
        let slots = Arc::new(Semaphore::new(cfg.max_downloads));
        Self { cfg, hf, hub, gpus, jobs: RwLock::new(Vec::new()), events, slots, db }
    }

    pub fn cfg(&self) -> &Config {
        &self.cfg
    }

    pub fn hf(&self) -> &HfClient {
        &self.hf
    }

    pub fn hub(&self) -> &Hub {
        &self.hub
    }

    pub fn gpus(&self) -> &[crate::gpu::GpuInfo] {
        &self.gpus
    }

    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.events.subscribe()
    }

    fn emit(&self, payload: serde_json::Value) {
        // A send error just means nobody is listening on the SSE endpoint.
        let _ = self.events.send(payload.to_string());
    }

    fn emit_job(&self, job: &Job) {
        self.emit(serde_json::json!({ "type": "job", "job": job.view() }));
    }

    pub fn emit_library_changed(&self) {
        self.emit(serde_json::json!({ "type": "library" }));
    }

    pub async fn list(&self) -> Vec<JobView> {
        self.jobs.read().await.iter().map(|j| j.view()).collect()
    }

    pub async fn get(&self, id: Uuid) -> Option<JobView> {
        self.jobs.read().await.iter().find(|j| j.id == id).map(|j| j.view())
    }

    pub async fn cancel(&self, id: Uuid) -> AppResult<JobView> {
        let job = self
            .jobs
            .read()
            .await
            .iter()
            .find(|j| j.id == id)
            .cloned()
            .ok_or_else(|| AppError::NotFound(format!("no such job: {id}")))?;
        job.cancel.cancel();
        Ok(job.view())
    }

    /// Drop a finished job from the list. Running jobs must be canceled first.
    pub async fn forget(&self, id: Uuid) -> AppResult<()> {
        let mut jobs = self.jobs.write().await;
        let Some(pos) = jobs.iter().position(|j| j.id == id) else {
            return Err(AppError::NotFound(format!("no such job: {id}")));
        };
        if !jobs[pos].status.lock().unwrap().is_terminal() {
            return Err(AppError::BadRequest("job is still running — cancel it first".into()));
        }
        jobs.remove(pos);
        self.db.delete(id);
        Ok(())
    }

    pub async fn enqueue(self: &Arc<Self>, mut req: NewJob) -> AppResult<JobView> {
        if req.source == Source::Github {
            // Always the whole repo via git (crate::git); one progress row stands for the clone.
            req.files = vec![(crate::git::TRACK_NAME.to_string(), 0)];
        }
        if req.files.is_empty() {
            return Err(AppError::BadRequest("no files selected".into()));
        }
        let store = self.cfg.store(req.kind, req.store.as_deref())?;
        let dest = store.repo_dir(req.kind, req.source, &req.repo_id);
        for (path, _) in &req.files {
            if safe_join(&dest, path).is_none() {
                return Err(AppError::BadRequest(format!("unsafe file path: {path:?}")));
            }
        }

        let job = Arc::new(Job {
            id: Uuid::new_v4(),
            repo_id: req.repo_id,
            revision: req.revision,
            kind: req.kind,
            source: req.source,
            store: store.name.clone(),
            dest,
            created_at: now_ms(),
            files: req
                .files
                .iter()
                .map(|(path, size)| {
                    Arc::new(FileTrack {
                        path: path.clone(),
                        size: AtomicU64::new(*size),
                        downloaded: AtomicU64::new(0),
                        status: Mutex::new(FileStatus::Pending),
                        error: Mutex::new(None),
                    })
                })
                .collect(),
            status: Mutex::new(JobStatus::Queued),
            error: Mutex::new(None),
            cancel: CancellationToken::new(),
        });

        {
            // Check and insert under one lock, so two requests can't both get past the check.
            let mut jobs = self.jobs.write().await;
            if let Some(active) = active_job_for(&jobs, &job.dest) {
                return Err(AppError::Conflict(format!(
                    "{} is already downloading into store {:?} (job {}); wait for it or cancel it first",
                    active.repo_id, active.store, active.id
                )));
            }
            jobs.push(job.clone());
            self.db.insert(&job.row());
        }
        let view = job.view();
        self.emit_job(&job);

        let manager = self.clone();
        tokio::spawn(async move { manager.run(job).await });

        Ok(view)
    }

    /// Load saved jobs at startup: history comes back as-is, and jobs a restart interrupted
    /// are queued again (HTTP files resume from their `.part`; git clones start over).
    pub async fn restore(self: &Arc<Self>) {
        self.db.prune(crate::db::KEEP_FINISHED);
        sweep_stale_clones(&self.cfg.datasets_store.root.join(Source::Github.as_str())).await;
        let resume = self.load_jobs().await;
        if !resume.is_empty() {
            tracing::info!(count = resume.len(), "resuming interrupted downloads");
        }
        for job in resume {
            let manager = self.clone();
            tokio::spawn(async move { manager.run(job).await });
        }
    }

    /// Put every saved job into the list; returns the ones to run again.
    async fn load_jobs(&self) -> Vec<Arc<Job>> {
        let mut jobs = self.jobs.write().await;
        let mut resume = Vec::new();
        for row in self.db.load() {
            let job = Arc::new(Job::from_row(row));
            if !job.status.lock().unwrap().is_terminal() {
                self.db.set_status(job.id, JobStatus::Queued, None);
                resume.push(job.clone());
            }
            jobs.push(job);
        }
        resume
    }

    async fn run(self: Arc<Self>, job: Arc<Job>) {
        job.set_status(JobStatus::Running);
        self.db.set_status(job.id, JobStatus::Running, None);
        self.emit_job(&job);

        // Push progress to the UI on a timer rather than on every chunk.
        let ticker = {
            let (manager, job) = (self.clone(), job.clone());
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_millis(400));
                loop {
                    interval.tick().await;
                    manager.emit_job(&job);
                }
            })
        };

        if job.source == Source::Github {
            self.clone_github(&job).await;
        }
        let mut tasks = tokio::task::JoinSet::new();
        for file in job.files.iter().filter(|_| job.source != Source::Github) {
            let (manager, job, file) = (self.clone(), job.clone(), file.clone());
            tasks.spawn(async move {
                let Some(_slot) = manager.slot(&job.cancel).await else {
                    *file.status.lock().unwrap() = FileStatus::Canceled;
                    return;
                };
                match manager.fetch_file(&job, &file).await {
                    Ok(status) => *file.status.lock().unwrap() = status,
                    Err(err) => {
                        let canceled = job.cancel.is_cancelled();
                        *file.status.lock().unwrap() =
                            if canceled { FileStatus::Canceled } else { FileStatus::Failed };
                        if !canceled {
                            *file.error.lock().unwrap() = Some(err.to_string());
                            *job.error.lock().unwrap() = Some(format!("{}: {err}", file.path));
                            // One bad file aborts the rest of the repo.
                            job.cancel.cancel();
                        }
                    }
                }
            });
        }
        tasks.join_all().await;
        ticker.abort();

        let failed = job.files.iter().any(|f| *f.status.lock().unwrap() == FileStatus::Failed);
        let status = if failed {
            JobStatus::Failed
        } else if job.cancel.is_cancelled() {
            JobStatus::Canceled
        } else {
            JobStatus::Completed
        };
        job.set_status(status);
        self.db.set_status(job.id, status, job.error.lock().unwrap().as_deref());

        if status == JobStatus::Completed {
            let files: Vec<String> = job.files.iter().map(|f| f.path.clone()).collect();
            // A git clone wrote its own (fresh) metadata before being moved into place.
            if job.source != Source::Github
                && let Err(err) = store::write_metadata(
                    &job.dest,
                    &job.repo_id,
                    &job.revision,
                    job.kind,
                    job.source,
                    &files,
                )
                .await
            {
                tracing::warn!(%err, "could not write model metadata");
            }
            self.emit_library_changed();
            if job.kind == ModelKind::Dataset {
                self.notify_dataset_hook(&job);
            }
        }
        self.emit_job(&job);
        tracing::info!(job = %job.id, repo = %job.repo_id, ?status, "job finished");
    }

    /// Wait for a service-wide download slot; `None` if the job is canceled while waiting.
    async fn slot(&self, cancel: &CancellationToken) -> Option<tokio::sync::OwnedSemaphorePermit> {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => None,
            permit = self.slots.clone().acquire_owned() => Some(permit.expect("slots never close")),
        }
    }

    /// Clone a GitHub repo whole into a hidden sibling folder, then swap it in for `dest`, so
    /// a canceled or failed clone never leaves a half-written dataset behind.
    async fn clone_github(&self, job: &Job) {
        let file = &job.files[0];
        let Some(_slot) = self.slot(&job.cancel).await else {
            *file.status.lock().unwrap() = FileStatus::Canceled;
            return;
        };
        *file.status.lock().unwrap() = FileStatus::Downloading;
        let name =
            job.dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let tmp = job.dest.with_file_name(format!(".{name}.clone-{}", job.id));
        let old = job.dest.with_file_name(format!(".{name}.old-{}", job.id));
        // A clone interrupted by a restart can't be resumed; start it over.
        let _ = tokio::fs::remove_dir_all(&tmp).await;

        let result = async {
            let cloned = crate::git::clone_into(
                &tmp,
                &job.repo_id,
                &job.revision,
                self.cfg.github_token.as_deref(),
                &job.cancel,
                &file.downloaded,
            )
            .await?;
            store::write_metadata(&tmp, &job.repo_id, &cloned.commit, job.kind, job.source, &cloned.files)
                .await?;
            if tokio::fs::try_exists(&job.dest).await? {
                tokio::fs::rename(&job.dest, &old).await?;
            }
            tokio::fs::rename(&tmp, &job.dest).await?;
            let _ = tokio::fs::remove_dir_all(&old).await;
            tracing::info!(repo = %job.repo_id, commit = %cloned.commit, files = cloned.files.len(), "cloned");
            Ok::<u64, AppError>(cloned.bytes)
        }
        .await;

        match result {
            Ok(bytes) => {
                file.size.store(bytes, Ordering::Relaxed);
                file.downloaded.store(bytes, Ordering::Relaxed);
                *file.status.lock().unwrap() = FileStatus::Done;
            }
            Err(err) => {
                let _ = tokio::fs::remove_dir_all(&tmp).await;
                let canceled = job.cancel.is_cancelled();
                *file.status.lock().unwrap() =
                    if canceled { FileStatus::Canceled } else { FileStatus::Failed };
                if !canceled {
                    *file.error.lock().unwrap() = Some(err.to_string());
                    *job.error.lock().unwrap() = Some(err.to_string());
                }
            }
        }
    }

    /// Tell `MODEL_MANAGER_DATASET_HOOK` (if set) that a dataset finished downloading.
    /// Fire-and-forget: a failing hook is logged, never fails the download.
    fn notify_dataset_hook(&self, job: &Job) {
        let Some(url) = self.cfg.dataset_hook.clone() else { return };
        let body = serde_json::json!({
            "source": job.source.as_str(),
            "repo_id": job.repo_id,
            "revision": job.revision,
            "path": job.dest.display().to_string(),
        });
        let request = self.hf.http().post(&url).json(&body).timeout(Duration::from_secs(15));
        let repo = job.repo_id.clone();
        tokio::spawn(async move {
            match request.send().await {
                Ok(resp) if resp.status().is_success() => {
                    tracing::info!(%repo, "dataset hook notified");
                }
                Ok(resp) => {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    tracing::warn!(%repo, %status, %text, "dataset hook rejected the download");
                }
                Err(err) => tracing::warn!(%repo, %err, "dataset hook unreachable"),
            }
        });
    }

    /// Fetch one file, resuming from a `.part` sidecar when one is present.
    async fn fetch_file(&self, job: &Job, file: &FileTrack) -> AppResult<FileStatus> {
        let target = safe_join(&job.dest, &file.path)
            .ok_or_else(|| AppError::BadRequest(format!("unsafe file path: {:?}", file.path)))?;
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let expected = file.size.load(Ordering::Relaxed);
        if let Ok(meta) = tokio::fs::metadata(&target).await
            && meta.is_file()
            && (expected == 0 || meta.len() == expected)
        {
            file.size.store(meta.len(), Ordering::Relaxed);
            file.downloaded.store(meta.len(), Ordering::Relaxed);
            return Ok(FileStatus::Skipped);
        }

        *file.status.lock().unwrap() = FileStatus::Downloading;
        let part = target.with_file_name(format!(
            "{}.part",
            target.file_name().and_then(|n| n.to_str()).unwrap_or("download")
        ));
        let mut offset = tokio::fs::metadata(&part).await.map(|m| m.len()).unwrap_or(0);

        let mut request =
            self.hub.file_request(job.source, job.kind, &job.repo_id, &job.revision, &file.path);
        if offset > 0 {
            request = request.header(reqwest::header::RANGE, format!("bytes={offset}-"));
        }
        let response = request.send().await?;
        let status = response.status();
        if !status.is_success() {
            return Err(match status.as_u16() {
                401 | 403 => {
                    AppError::Upstream(format!("access denied — {}", job.source.auth_hint()))
                }
                404 => AppError::NotFound(format!("{} is not in the repo", file.path)),
                _ => AppError::Upstream(format!("{status} while fetching {}", file.path)),
            });
        }

        // The server ignored our Range header, so start over from zero.
        let resuming = offset > 0 && status == reqwest::StatusCode::PARTIAL_CONTENT;
        if offset > 0 && !resuming {
            offset = 0;
        }
        if let Some(len) = response.content_length() {
            file.size.store(offset + len, Ordering::Relaxed);
        }
        file.downloaded.store(offset, Ordering::Relaxed);

        let mut sink = tokio::io::BufWriter::new(
            tokio::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(!resuming)
                .append(resuming)
                .open(&part)
                .await?,
        );

        let mut stream = response.bytes_stream();
        loop {
            let chunk = tokio::select! {
                biased;
                _ = job.cancel.cancelled() => {
                    sink.flush().await?;
                    return Ok(FileStatus::Canceled);
                }
                chunk = stream.next() => chunk,
            };
            let Some(chunk) = chunk else { break };
            let chunk = chunk?;
            sink.write_all(&chunk).await?;
            file.downloaded.fetch_add(chunk.len() as u64, Ordering::Relaxed);
        }
        sink.flush().await?;
        drop(sink);

        tokio::fs::rename(&part, &target).await?;
        let final_len = tokio::fs::metadata(&target).await.map(|m| m.len()).unwrap_or(0);
        file.size.store(final_len, Ordering::Relaxed);
        file.downloaded.store(final_len, Ordering::Relaxed);
        Ok(FileStatus::Done)
    }
}

/// Clean up after git clones a restart interrupted, in `<datasets>/github/<owner>/`:
/// `.<name>.clone-<id>` folders are dropped, and an `.<name>.old-<id>` whose swap never finished
/// is moved back if `<name>` is missing (otherwise it's the stale copy and goes).
async fn sweep_stale_clones(github_root: &std::path::Path) {
    let Ok(mut owners) = tokio::fs::read_dir(github_root).await else { return };
    while let Ok(Some(owner)) = owners.next_entry().await {
        let Ok(mut entries) = tokio::fs::read_dir(owner.path()).await else { continue };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let file_name = entry.file_name();
            let Some(hidden) = file_name.to_str().and_then(|n| n.strip_prefix('.')) else {
                continue;
            };
            let path = entry.path();
            if hidden.contains(".clone-") {
                tracing::warn!(path = %path.display(), "removing interrupted git clone");
                let _ = tokio::fs::remove_dir_all(&path).await;
            } else if let Some((name, _)) = hidden.rsplit_once(".old-") {
                let original = owner.path().join(name);
                if tokio::fs::try_exists(&original).await.unwrap_or(true) {
                    tracing::warn!(path = %path.display(), "removing stale replaced dataset");
                    let _ = tokio::fs::remove_dir_all(&path).await;
                } else {
                    tracing::warn!(path = %original.display(), "restoring dataset from an interrupted swap");
                    let _ = tokio::fs::rename(&path, &original).await;
                }
            }
        }
    }
}

/// A queued or running job writing into `dest`. Two jobs sharing a folder would append to the
/// same `.part` files and corrupt them.
fn active_job_for<'a>(jobs: &'a [Arc<Job>], dest: &std::path::Path) -> Option<&'a Arc<Job>> {
    jobs.iter().find(|j| j.dest == dest && !j.status.lock().unwrap().is_terminal())
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(dest: &str, status: JobStatus) -> Arc<Job> {
        Arc::new(Job {
            id: Uuid::new_v4(),
            repo_id: "Qwen/Qwen3-8B".into(),
            revision: "main".into(),
            kind: ModelKind::Safetensors,
            source: Source::Huggingface,
            store: "slow".into(),
            dest: PathBuf::from(dest),
            created_at: 0,
            files: Vec::new(),
            status: Mutex::new(status),
            error: Mutex::new(None),
            cancel: CancellationToken::new(),
        })
    }

    fn manager(max_downloads: usize) -> Manager {
        let cfg = Config {
            data_dir: PathBuf::from("/tmp"),
            stores: vec![crate::config::Store { name: "slow".into(), root: "/tmp/m".into() }],
            datasets_store: crate::config::Store { name: "datasets".into(), root: "/tmp/d".into() },
            kaggle: None,
            github_token: None,
            bind: "127.0.0.1:0".into(),
            hf_token: None,
            max_downloads,
            web_dir: PathBuf::from("web/dist"),
            container_runtime: "podman".into(),
            tls_cert: None,
            tls_key: None,
            client_ca: None,
            tailscale_bind: None,
            dataset_hook: None,
        };
        Manager::new(
            cfg,
            HfClient::new(reqwest::Client::new(), None),
            Vec::new(),
            crate::db::Db::in_memory(),
        )
    }

    #[tokio::test]
    async fn slots_are_shared_across_jobs() {
        let mgr = manager(2);
        let (a, b) = (CancellationToken::new(), CancellationToken::new());
        let first = mgr.slot(&a).await.expect("slot 1");
        let _second = mgr.slot(&b).await.expect("slot 2");

        // A third download, from any job, waits until a slot frees up.
        let c = CancellationToken::new();
        let waiting = tokio::time::timeout(Duration::from_millis(50), mgr.slot(&c)).await;
        assert!(waiting.is_err(), "third download must wait");
        drop(first);
        assert!(mgr.slot(&c).await.is_some());

        // Canceling a job releases its waiting downloads without a slot.
        let d = CancellationToken::new();
        d.cancel();
        assert!(mgr.slot(&d).await.is_none());
    }

    #[tokio::test]
    async fn restart_requeues_unfinished_and_keeps_history() {
        let mgr = manager(2);
        let interrupted = job("/slow/models/safetensors/a/b", JobStatus::Running);
        let finished = job("/slow/models/safetensors/c/d", JobStatus::Completed);
        mgr.db.insert(&interrupted.row());
        mgr.db.insert(&finished.row());

        let resume = mgr.load_jobs().await;
        assert_eq!(resume.iter().map(|j| j.id).collect::<Vec<_>>(), [interrupted.id]);
        assert_eq!(*resume[0].status.lock().unwrap(), JobStatus::Queued);
        assert_eq!(mgr.db.status_of(interrupted.id).as_deref(), Some("queued"));

        let listed = mgr.list().await;
        assert_eq!(listed.len(), 2);
        let done = listed.iter().find(|j| j.id == finished.id).unwrap();
        assert_eq!(done.status, JobStatus::Completed);

        // The resumed job still blocks a duplicate download into its folder.
        let jobs = mgr.jobs.read().await;
        assert!(active_job_for(&jobs, &interrupted.dest).is_some());
    }

    #[tokio::test]
    async fn sweep_drops_clones_and_restores_unfinished_swaps() {
        let root = std::env::temp_dir().join(format!("mm-sweep-{}", Uuid::new_v4()));
        let owner = root.join("owner");
        for d in [".a.clone-1", ".b.old-2", "c", ".c.old-3", "d"] {
            std::fs::create_dir_all(owner.join(d)).unwrap();
        }
        sweep_stale_clones(&root).await;
        let mut left: Vec<_> = std::fs::read_dir(&owner)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["b", "c", "d"]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn only_unfinished_jobs_in_the_same_folder_block() {
        let dest = "/slow/models/safetensors/Qwen/Qwen3-8B";
        let path = std::path::Path::new(dest);
        let finished: Vec<_> = [JobStatus::Completed, JobStatus::Failed, JobStatus::Canceled]
            .into_iter()
            .map(|s| job(dest, s))
            .collect();
        assert!(active_job_for(&finished, path).is_none());

        // Same repo in another store is a different folder.
        let other_store = vec![job("/fast/models/safetensors/Qwen/Qwen3-8B", JobStatus::Running)];
        assert!(active_job_for(&other_store, path).is_none());

        for status in [JobStatus::Queued, JobStatus::Running] {
            let mut jobs = finished.clone();
            jobs.push(job(dest, status));
            let found = active_job_for(&jobs, path).expect("active job blocks");
            assert_eq!(*found.status.lock().unwrap(), status);
        }
    }
}
