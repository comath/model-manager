//! Persistent download jobs in SQLite (`<data>/model-manager.sqlite`).
//!
//! Only what a job needs to be restarted is stored: the repo, where it goes, the file list and
//! the outcome. Byte progress isn't: a resumed job picks up from the `.part` files on disk. Every
//! call is a single small statement, so they run inline; failures are logged, never fatal to a
//! download.

use crate::download::JobStatus;
use crate::hf::ModelKind;
use crate::source::Source;
use rusqlite::{Connection, params};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use uuid::Uuid;

/// Finished jobs kept in the history; older ones are dropped at startup.
pub const KEEP_FINISHED: usize = 500;

pub struct Db {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct JobRow {
    pub id: Uuid,
    pub repo_id: String,
    pub revision: String,
    pub kind: ModelKind,
    pub source: Source,
    pub store: String,
    pub dest: PathBuf,
    pub status: JobStatus,
    pub error: Option<String>,
    pub created_at: u64,
    pub files: Vec<(String, u64)>,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS jobs (
    id          TEXT PRIMARY KEY,
    repo_id     TEXT NOT NULL,
    revision    TEXT NOT NULL,
    kind        TEXT NOT NULL,
    source      TEXT NOT NULL,
    store       TEXT NOT NULL,
    dest        TEXT NOT NULL,
    status      TEXT NOT NULL,
    error       TEXT,
    created_at  INTEGER NOT NULL,
    finished_at INTEGER,
    files       TEXT NOT NULL  -- JSON [[path, size], …]
);
CREATE INDEX IF NOT EXISTS jobs_created ON jobs (created_at);
";

impl Db {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Self::init(conn)
    }

    #[cfg(test)]
    pub fn in_memory() -> Self {
        Self::init(Connection::open_in_memory().unwrap()).unwrap()
    }

    fn init(conn: Connection) -> rusqlite::Result<Self> {
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn insert(&self, row: &JobRow) {
        let files = serde_json::to_string(&row.files).expect("file list is serializable");
        self.log("insert", |c| {
            c.execute(
                "INSERT OR REPLACE INTO jobs
                 (id, repo_id, revision, kind, source, store, dest, status, error, created_at, files)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    row.id.to_string(),
                    row.repo_id,
                    row.revision,
                    row.kind.as_str(),
                    row.source.as_str(),
                    row.store,
                    row.dest.to_string_lossy(),
                    row.status.as_str(),
                    row.error,
                    row.created_at as i64,
                    files,
                ],
            )
        });
    }

    pub fn set_status(&self, id: Uuid, status: JobStatus, error: Option<&str>) {
        let finished = status.is_terminal().then(|| crate::download::now_ms() as i64);
        self.log("update", |c| {
            c.execute(
                "UPDATE jobs SET status = ?2, error = ?3, finished_at = ?4 WHERE id = ?1",
                params![id.to_string(), status.as_str(), error, finished],
            )
        });
    }

    pub fn delete(&self, id: Uuid) {
        self.log("delete", |c| c.execute("DELETE FROM jobs WHERE id = ?1", [id.to_string()]));
    }

    /// Drop all but the newest `keep` finished jobs.
    pub fn prune(&self, keep: usize) {
        self.log("prune", |c| {
            c.execute(
                "DELETE FROM jobs WHERE status IN ('completed', 'failed', 'canceled') AND id NOT IN (
                     SELECT id FROM jobs WHERE status IN ('completed', 'failed', 'canceled')
                     ORDER BY created_at DESC LIMIT ?1)",
                [keep as i64],
            )
        });
    }

    /// Every stored job, oldest first. Rows that no longer parse are skipped with a warning.
    pub fn load(&self) -> Vec<JobRow> {
        let conn = self.conn.lock().unwrap();
        let result = conn
            .prepare(
                "SELECT id, repo_id, revision, kind, source, store, dest, status, error,
                        created_at, files
                 FROM jobs ORDER BY created_at",
            )
            .and_then(|mut stmt| {
                stmt.query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, String>(5)?,
                        r.get::<_, String>(6)?,
                        r.get::<_, String>(7)?,
                        r.get::<_, Option<String>>(8)?,
                        r.get::<_, i64>(9)?,
                        r.get::<_, String>(10)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
            });
        let raw = match result {
            Ok(raw) => raw,
            Err(err) => {
                tracing::error!(%err, "could not load download jobs");
                return Vec::new();
            }
        };
        raw.into_iter()
            .filter_map(
                |(
                    id,
                    repo_id,
                    revision,
                    kind,
                    source,
                    store,
                    dest,
                    status,
                    error,
                    created,
                    files,
                )| {
                    let row = (|| {
                        Some(JobRow {
                            id: Uuid::parse_str(&id).ok()?,
                            repo_id,
                            revision,
                            kind: ModelKind::parse(&kind)?,
                            source: Source::parse(&source)?,
                            store,
                            dest: PathBuf::from(dest),
                            status: JobStatus::parse(&status)?,
                            error,
                            created_at: created.max(0) as u64,
                            files: serde_json::from_str(&files).ok()?,
                        })
                    })();
                    if row.is_none() {
                        tracing::warn!(%id, "skipping unreadable download job");
                    }
                    row
                },
            )
            .collect()
    }

    #[cfg(test)]
    pub fn status_of(&self, id: Uuid) -> Option<String> {
        use rusqlite::OptionalExtension;
        self.conn
            .lock()
            .unwrap()
            .query_row("SELECT status FROM jobs WHERE id = ?1", [id.to_string()], |r| r.get(0))
            .optional()
            .unwrap()
    }

    fn log(&self, what: &str, op: impl FnOnce(&Connection) -> rusqlite::Result<usize>) {
        if let Err(err) = op(&self.conn.lock().unwrap()) {
            tracing::warn!(%err, "jobs database {what} failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(status: JobStatus, created_at: u64) -> JobRow {
        JobRow {
            id: Uuid::new_v4(),
            repo_id: "owner/name".into(),
            revision: "main".into(),
            kind: ModelKind::Dataset,
            source: Source::Github,
            store: "datasets".into(),
            dest: PathBuf::from("/slow/models/datasets/github/owner/name"),
            status,
            error: None,
            created_at,
            files: vec![("a.parquet".into(), 12), ("dir/b.csv".into(), 0)],
        }
    }

    #[test]
    fn round_trip_update_delete() {
        let db = Db::in_memory();
        let a = row(JobStatus::Queued, 1);
        db.insert(&a);
        assert_eq!(db.load(), vec![a.clone()]);

        db.set_status(a.id, JobStatus::Failed, Some("boom"));
        let loaded = db.load();
        assert_eq!(loaded[0].status, JobStatus::Failed);
        assert_eq!(loaded[0].error.as_deref(), Some("boom"));

        db.delete(a.id);
        assert!(db.load().is_empty());
    }

    #[test]
    fn prune_keeps_unfinished_and_newest_finished() {
        let db = Db::in_memory();
        let running = row(JobStatus::Running, 0);
        db.insert(&running);
        let finished: Vec<_> = (1..=5).map(|t| row(JobStatus::Completed, t)).collect();
        finished.iter().for_each(|r| db.insert(r));

        db.prune(2);
        let ids: Vec<_> = db.load().into_iter().map(|r| r.id).collect();
        assert_eq!(ids, [running.id, finished[3].id, finished[4].id]);
    }
}
