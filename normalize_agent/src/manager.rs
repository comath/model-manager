//! Job queue, workers, and the per-job Incus container lifecycle.

use std::{
    collections::HashSet,
    fs::File,
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use serde_json::json;
use tokio::{
    io::AsyncWriteExt,
    process::Command,
    sync::mpsc,
    time::{sleep, timeout},
};
use tracing::{error, info, warn};

use crate::{
    CONTAINER_PREFIX, INCUS_STEP_TIMEOUT_SECS, KEEP_JOBS, LOGS, OUT_MOUNT, RAW_MOUNT, STATUS_FILE,
    Settings, WORK, check::{Summary, check_output, resolve_raw}, makedirs_owned,
};

const STEP_TIMEOUT: Duration = Duration::from_secs(INCUS_STEP_TIMEOUT_SECS);

#[derive(Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Queued,
    Running,
    Completed,
    Failed,
}

#[derive(Clone, Serialize)]
pub struct Job {
    pub id: String,
    pub source: String,
    pub repo_id: String,
    pub revision: String,
    pub raw: PathBuf,
    /// <source>/<owner>/<name>, relative to the datasets root.
    #[serde(skip)]
    pub rel: PathBuf,
    pub status: Status,
    pub error: Option<String>,
    pub created_at: f64,
    pub started_at: Option<f64>,
    pub finished_at: Option<f64>,
    pub container: Option<String>,
    pub log: Option<PathBuf>,
    pub output: Option<PathBuf>,
}

fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

#[derive(Default)]
struct State {
    /// Oldest first.
    jobs: Vec<Job>,
    active: HashSet<PathBuf>,
}

pub struct Manager {
    s: Settings,
    state: Mutex<State>,
    tx: mpsc::UnboundedSender<String>,
}

impl Manager {
    pub fn start(settings: Settings, workers: usize) -> Arc<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        let m = Arc::new(Manager { s: settings, state: Mutex::default(), tx });
        let rx = Arc::new(tokio::sync::Mutex::new(rx));
        for _ in 0..workers {
            tokio::spawn(Arc::clone(&m).worker(Arc::clone(&rx)));
        }
        m
    }

    pub fn settings(&self) -> &Settings {
        &self.s
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn update(&self, id: &str, f: impl FnOnce(&mut Job)) {
        if let Some(job) = self.lock().jobs.iter_mut().find(|j| j.id == id) {
            f(job);
        }
    }

    // --- API -----------------------------------------------------------------------------

    /// Queue a job; the bool is false when an identical job was already queued.
    pub fn submit(
        &self,
        source: &str,
        repo_id: &str,
        revision: &str,
        path: &str,
    ) -> Result<(Job, bool), String> {
        let (raw, rel) = resolve_raw(&self.s.datasets_root, source, path)?;
        let job = {
            let mut st = self.lock();
            if let Some(j) = st.jobs.iter().find(|j| j.rel == rel && j.status == Status::Queued) {
                // The queued run will pick up the newest files anyway.
                return Ok((j.clone(), false));
            }
            let repo_id = if repo_id.is_empty() {
                rel.iter().skip(1).map(|p| p.to_string_lossy()).collect::<Vec<_>>().join("/")
            } else {
                repo_id.to_string()
            };
            let job = Job {
                id: uuid::Uuid::new_v4().simple().to_string(),
                source: source.to_string(),
                repo_id,
                revision: revision.to_string(),
                raw,
                rel,
                status: Status::Queued,
                error: None,
                created_at: now(),
                started_at: None,
                finished_at: None,
                container: None,
                log: None,
                output: None,
            };
            st.jobs.push(job.clone());
            prune(&mut st.jobs);
            job
        };
        let _ = self.tx.send(job.id.clone());
        info!("queued {source} {} (job {})", job.repo_id, job.id);
        Ok((job, true))
    }

    /// Newest first.
    pub fn list(&self) -> Vec<Job> {
        self.lock().jobs.iter().rev().cloned().collect()
    }

    pub fn get(&self, id: &str) -> Option<Job> {
        self.lock().jobs.iter().find(|j| j.id == id).cloned()
    }

    // --- startup -------------------------------------------------------------------------

    /// Clear what a crash mid-job left behind: containers and staging folders.
    pub async fn recover(&self) {
        let normalized = self.s.normalized();
        for dir in [WORK, LOGS] {
            if let Err(e) = makedirs_owned(&normalized.join(dir), self.s.owner) {
                warn!("could not create {dir}: {e}");
            }
        }
        if let Ok(entries) = std::fs::read_dir(normalized.join(WORK)) {
            for leftover in entries.flatten().map(|e| e.path()) {
                warn!("removing leftover staging folder {}", leftover.display());
                let _ = std::fs::remove_dir_all(&leftover);
            }
        }
        let listed = timeout(
            STEP_TIMEOUT,
            Command::new(&self.s.incus)
                .args(["list", "--format", "csv", "--columns", "n"])
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await;
        let out = match listed {
            Ok(Ok(o)) if o.status.success() => o,
            Ok(Ok(o)) => return warn!("could not list incus instances: {}", o.status),
            Ok(Err(e)) => return warn!("could not list incus instances: {e}"),
            Err(_) => return warn!("could not list incus instances: timed out"),
        };
        for name in String::from_utf8_lossy(&out.stdout).split_whitespace() {
            if name.starts_with(CONTAINER_PREFIX) {
                warn!("removing leftover container {name}");
                self.delete_container(name).await;
            }
        }
    }

    // --- jobs ----------------------------------------------------------------------------

    async fn worker(self: Arc<Self>, rx: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<String>>>) {
        loop {
            let Some(id) = rx.lock().await.recv().await else { return };
            let Some(job) = self.get(&id) else { continue };
            let busy = {
                let mut st = self.lock();
                !st.active.insert(job.rel.clone())
            };
            if busy {
                // Another worker is normalizing the same dataset; retry once it's done.
                let _ = self.tx.send(id);
                sleep(Duration::from_secs(5)).await;
                continue;
            }
            // A separate task, so a panic fails the job instead of killing the worker.
            let handle = tokio::spawn(Arc::clone(&self).run(job.clone()));
            if let Err(e) = handle.await {
                error!("job {} crashed: {e}", job.id);
                self.finish(&job.id, Err(format!("internal error: {e}")));
            }
            self.lock().active.remove(&job.rel);
        }
    }

    fn finish(&self, id: &str, result: Result<(), String>) {
        let mut line = None;
        self.update(id, |job| {
            job.finished_at = Some(now());
            match &result {
                Ok(()) => {
                    job.status = Status::Completed;
                    job.error = None;
                }
                Err(e) => {
                    job.status = Status::Failed;
                    job.error = Some(e.clone());
                }
            }
            line = Some((job.repo_id.clone(), job.output.clone()));
        });
        let id = id.to_string();
        match (result, line) {
            (Err(e), Some((repo, _))) => warn!("job {id} ({repo}) failed: {e}"),
            (Ok(()), Some((repo, out))) => {
                info!("job {id} ({repo}) completed: {}", out.map(|p| p.display().to_string()).unwrap_or_default())
            }
            _ => {}
        }
    }

    async fn run(self: Arc<Self>, job: Job) {
        let result = self.run_job(&job).await;
        self.finish(&job.id, result);
    }

    async fn run_job(self: &Arc<Self>, job: &Job) -> Result<(), String> {
        let s = &self.s;
        let normalized = s.normalized();
        let work = normalized.join(WORK).join(&job.id);
        let log_path = normalized.join(LOGS).join(format!("{}.log", job.id));
        let name = format!("{CONTAINER_PREFIX}{}", &job.id[..12]);
        self.update(&job.id, |j| {
            j.status = Status::Running;
            j.started_at = Some(now());
            j.container = Some(name.clone());
            j.log = Some(log_path.clone());
        });

        // Stays root-owned until checked; the container writes as root.
        makedirs_owned(&work, None).map_err(internal)?;
        let result = self.run_staged(job, &name, &work, &log_path).await;
        if work.exists() && (result.is_ok() || !s.keep_failed) {
            let _ = std::fs::remove_dir_all(&work);
        }
        result
    }

    async fn run_staged(
        self: &Arc<Self>,
        job: &Job,
        name: &str,
        work: &Path,
        log_path: &Path,
    ) -> Result<(), String> {
        let logf = File::create(log_path).map_err(internal)?;
        let ran = self.run_container(job, name, work, &logf).await;
        self.delete_container(name).await;
        ran?;
        if let Some((uid, gid)) = self.s.owner {
            std::os::unix::fs::chown(log_path, Some(uid), Some(gid)).map_err(internal)?;
        }
        let this = Arc::clone(self);
        let (job, work) = (job.clone(), work.to_path_buf());
        tokio::task::spawn_blocking(move || {
            let summary = check_output(&work, this.s.owner)?;
            this.install(&job, &work, summary)
        })
        .await
        .map_err(internal)?
    }

    async fn run_container(&self, job: &Job, name: &str, work: &Path, mut logf: &File) -> Result<(), String> {
        let s = &self.s;
        let mut env = s.env.clone();
        for (k, v) in [
            ("DATASET_SOURCE", job.source.as_str()),
            ("DATASET_REPO_ID", job.repo_id.as_str()),
            ("DATASET_REVISION", job.revision.as_str()),
            ("RAW_DIR", RAW_MOUNT),
            ("OUT_DIR", OUT_MOUNT),
        ] {
            env.insert(k.into(), v.into());
        }
        let mut create: Vec<String> = ["create", &s.image, name, "--ephemeral"].map(String::from).into();
        for p in &s.profiles {
            create.extend(["--profile".into(), p.clone()]);
        }
        // Config and devices go in on stdin (JSON is YAML), so secrets never show up in `ps`.
        let mut config = s.container_config.clone();
        config.extend(env.iter().map(|(k, v)| (format!("environment.{k}"), v.clone())));
        let devices = json!({
            "raw": {"type": "disk", "source": job.raw, "path": RAW_MOUNT, "readonly": "true"},
            // shift=true: the container's root may write here (as host root; check_output
            // fixes ownership and modes before anything is kept).
            "out": {"type": "disk", "source": work, "path": OUT_MOUNT, "shift": "true"},
        });
        let _ = writeln!(logf, "devices: {devices}");
        let instance = json!({"config": config, "devices": devices});
        self.incus(&create, logf, Some(instance.to_string())).await?;
        self.incus(&["start".to_string(), name.to_string()], logf, None).await?;

        let _ = writeln!(logf, "$ agent: {}", s.command.join(" "));
        let mut child = Command::new(&s.incus)
            .args(["exec", name, "--cwd", OUT_MOUNT, "--"])
            .args(&s.command)
            .stdin(Stdio::null())
            .stdout(logf.try_clone().map_err(internal)?)
            .stderr(logf.try_clone().map_err(internal)?)
            .kill_on_drop(true)
            .spawn()
            .map_err(internal)?;
        let status = match timeout(Duration::from_secs(s.timeout), child.wait()).await {
            Ok(r) => r.map_err(internal)?,
            Err(_) => return Err(format!("agent did not finish within {}s", s.timeout)),
        };
        if !status.success() {
            return Err(format!(
                "agent exited with {} (see {})",
                status.code().map_or("a signal".to_string(), |c| format!("status {c}")),
                self.log_hint(job)
            ));
        }
        Ok(())
    }

    fn log_hint(&self, job: &Job) -> String {
        self.s.normalized().join(LOGS).join(format!("{}.log", job.id)).display().to_string()
    }

    async fn incus(&self, args: &[String], mut logf: &File, stdin: Option<String>) -> Result<(), String> {
        let _ = writeln!(logf, "$ {} {}", self.s.incus, args.join(" "));
        let mut cmd = Command::new(&self.s.incus);
        cmd.args(args)
            .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(logf.try_clone().map_err(internal)?)
            .stderr(logf.try_clone().map_err(internal)?)
            .kill_on_drop(true);
        let step = async {
            let mut child = cmd.spawn()?;
            if let Some(input) = stdin {
                let mut pipe = child.stdin.take().expect("stdin was piped");
                pipe.write_all(input.as_bytes()).await?;
            }
            child.wait().await
        };
        let sub = args.first().map_or("", String::as_str);
        match timeout(STEP_TIMEOUT, step).await {
            Err(_) => Err(format!("`incus {sub}` timed out after {INCUS_STEP_TIMEOUT_SECS}s")),
            Ok(Err(e)) => Err(internal(e)),
            Ok(Ok(st)) if st.success() => Ok(()),
            Ok(Ok(st)) => Err(format!(
                "`incus {sub}` failed with {} (see log)",
                st.code().map_or("a signal".to_string(), |c| format!("status {c}"))
            )),
        }
    }

    async fn delete_container(&self, name: &str) {
        let _ = timeout(
            STEP_TIMEOUT,
            Command::new(&self.s.incus)
                .args(["delete", "--force", name])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .status(),
        )
        .await;
    }

    /// Swap the checked output in for the previous normalized copy, if any. Blocking.
    fn install(&self, job: &Job, work: &Path, summary: Summary) -> Result<(), String> {
        use std::os::unix::fs::{PermissionsExt, chown};
        let final_dir = self.s.normalized().join(&job.rel);
        let status = json!({
            "source": job.source,
            "repo_id": job.repo_id,
            "revision": job.revision,
            "raw": job.raw,
            "job": job.id,
            "image": self.s.image,
            "normalized_at": now(),
            "parquet": summary.parquet,
            "bytes": summary.bytes,
        });
        let status_path = work.join(STATUS_FILE);
        let text = serde_json::to_string_pretty(&status).map_err(internal)?;
        std::fs::write(&status_path, text + "\n").map_err(internal)?;
        std::fs::set_permissions(&status_path, std::fs::Permissions::from_mode(0o644)).map_err(internal)?;
        if let Some((uid, gid)) = self.s.owner {
            chown(&status_path, Some(uid), Some(gid)).map_err(internal)?;
        }

        if let Some(parent) = final_dir.parent() {
            makedirs_owned(parent, self.s.owner).map_err(internal)?;
        }
        let old = final_dir.exists().then(|| self.s.normalized().join(WORK).join(format!("{}.old", job.id)));
        if let Some(old) = &old {
            std::fs::rename(&final_dir, old).map_err(internal)?;
        }
        std::fs::rename(work, &final_dir).map_err(internal)?;
        if let Some(old) = old {
            let _ = std::fs::remove_dir_all(old);
        }
        self.update(&job.id, |j| j.output = Some(final_dir.clone()));
        Ok(())
    }
}

fn internal(e: impl std::fmt::Display) -> String {
    format!("internal error: {e}")
}

/// Drop the oldest finished jobs beyond KEEP_JOBS.
fn prune(jobs: &mut Vec<Job>) {
    let excess = jobs.len().saturating_sub(KEEP_JOBS);
    let mut dropped = 0;
    jobs.retain(|j| {
        let finished = matches!(j.status, Status::Completed | Status::Failed);
        if finished && dropped < excess {
            dropped += 1;
            false
        } else {
            true
        }
    });
}
