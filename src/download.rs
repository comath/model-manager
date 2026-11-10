//! Download jobs: one job = one repo, N files, resumable, cancellable.

use crate::config::{Config, safe_join};
use crate::error::{AppError, AppResult};
use crate::hf::{HfClient, ModelKind};
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
}

/// Everything the API layer talks to.
pub struct Manager {
    cfg: Config,
    hf: HfClient,
    gpus: Vec<crate::gpu::GpuInfo>,
    jobs: RwLock<Vec<Arc<Job>>>,
    events: broadcast::Sender<String>,
}

#[derive(Debug, Clone)]
pub struct NewJob {
    pub repo_id: String,
    pub revision: String,
    pub kind: ModelKind,
    /// Store name; `None` means the default store.
    pub store: Option<String>,
    pub files: Vec<(String, u64)>,
}

impl Manager {
    pub fn new(cfg: Config, hf: HfClient, gpus: Vec<crate::gpu::GpuInfo>) -> Self {
        let (events, _) = broadcast::channel(256);
        Self { cfg, hf, gpus, jobs: RwLock::new(Vec::new()), events }
    }

    pub fn cfg(&self) -> &Config {
        &self.cfg
    }

    pub fn hf(&self) -> &HfClient {
        &self.hf
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
        Ok(())
    }

    pub async fn enqueue(self: &Arc<Self>, req: NewJob) -> AppResult<JobView> {
        if req.files.is_empty() {
            return Err(AppError::BadRequest("no files selected".into()));
        }
        let store = self.cfg.store(req.store.as_deref())?;
        let dest = store.repo_dir(req.kind, &req.repo_id);
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

        self.jobs.write().await.push(job.clone());
        let view = job.view();
        self.emit_job(&job);

        let manager = self.clone();
        tokio::spawn(async move { manager.run(job).await });

        Ok(view)
    }

    async fn run(self: Arc<Self>, job: Arc<Job>) {
        job.set_status(JobStatus::Running);
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

        let semaphore = Arc::new(Semaphore::new(self.cfg.parallel_files));
        let mut tasks = tokio::task::JoinSet::new();
        for file in &job.files {
            let (manager, job, file, semaphore) =
                (self.clone(), job.clone(), file.clone(), semaphore.clone());
            tasks.spawn(async move {
                let _permit = semaphore.acquire_owned().await.expect("semaphore open");
                if job.cancel.is_cancelled() {
                    *file.status.lock().unwrap() = FileStatus::Canceled;
                    return;
                }
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

        if status == JobStatus::Completed {
            let files: Vec<String> = job.files.iter().map(|f| f.path.clone()).collect();
            if let Err(err) =
                store::write_metadata(&job.dest, &job.repo_id, &job.revision, job.kind, &files)
                    .await
            {
                tracing::warn!(%err, "could not write model metadata");
            }
            self.emit_library_changed();
        }
        self.emit_job(&job);
        tracing::info!(job = %job.id, repo = %job.repo_id, ?status, "job finished");
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

        let url = self.hf.resolve_url(&job.repo_id, &job.revision, &file.path);
        let mut request = self.hf.authorize(self.hf.http().get(&url));
        if offset > 0 {
            request = request.header(reqwest::header::RANGE, format!("bytes={offset}-"));
        }
        let response = request.send().await?;
        let status = response.status();
        if !status.is_success() {
            return Err(match status.as_u16() {
                401 | 403 => AppError::Upstream(
                    "access denied — the repo is gated or private; set HF_TOKEN and restart".into(),
                ),
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

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}
