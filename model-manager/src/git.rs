//! GitHub datasets: a shallow fetch of the whole repo plus `git lfs pull`, so files stored in
//! Git LFS arrive as real data instead of pointer stubs. `.git` is removed afterwards (LFS keeps
//! a second copy of every large file in it).
//!
//! git runs with no system/global config; everything it needs (LFS filters, the GitHub token)
//! comes in through `GIT_CONFIG_*` environment variables, so the token never shows up in `ps`
//! and is never written into the repo.

use crate::error::{AppError, AppResult};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// The single progress row a git job shows instead of per-file rows.
pub const TRACK_NAME: &str = "git clone";

pub struct Cloned {
    /// The commit that was checked out.
    pub commit: String,
    /// Every file in the checkout, relative to its root.
    pub files: Vec<String>,
    pub bytes: u64,
}

/// Clone `repo_id` at `revision` (branch, tag or commit) into the empty folder `dir`.
/// `progress` is kept at the number of bytes on disk so far.
pub async fn clone_into(
    dir: &Path,
    repo_id: &str,
    revision: &str,
    token: Option<&str>,
    cancel: &CancellationToken,
    progress: &AtomicU64,
) -> AppResult<Cloned> {
    validate_revision(revision)?;
    tokio::fs::create_dir_all(dir).await?;
    let url = format!("https://github.com/{repo_id}.git");
    let git = Git { dir, env: git_env(token), cancel, progress };

    git.run(&["init", "--quiet"], false).await?;
    git.run(&["remote", "add", "origin", &url], false).await?;
    git.run(&["fetch", "--depth", "1", "--no-tags", "--quiet", "origin", revision], false).await?;
    // Plain files first, then the LFS objects in one batch.
    git.run(&["checkout", "--quiet", "FETCH_HEAD"], true).await?;
    git.run(&["lfs", "pull"], false).await?;
    let commit = git.run(&["rev-parse", "HEAD"], false).await?.trim().to_string();

    tokio::fs::remove_dir_all(dir.join(".git")).await?;
    let root = dir.to_path_buf();
    let (files, bytes) = tokio::task::spawn_blocking(move || walk(&root))
        .await
        .map_err(|e| AppError::Io(std::io::Error::other(e)))??;
    progress.store(bytes, Ordering::Relaxed);
    Ok(Cloned { commit, files, bytes })
}

struct Git<'a> {
    dir: &'a Path,
    env: Vec<(String, String)>,
    cancel: &'a CancellationToken,
    progress: &'a AtomicU64,
}

impl Git<'_> {
    /// Run one git command in the repo; returns stdout. Killed when the job is canceled.
    async fn run(&self, args: &[&str], skip_smudge: bool) -> AppResult<String> {
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(self.dir)
            .args(args)
            .env_clear()
            .envs(passthrough_env())
            .envs(self.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if skip_smudge {
            cmd.env("GIT_LFS_SKIP_SMUDGE", "1");
        }
        let mut child = cmd.spawn().map_err(|err| {
            AppError::Upstream(format!("could not run git (is it installed?): {err}"))
        })?;

        // Drain both pipes while waiting, or a chatty command could block on a full pipe.
        let mut stdout = child.stdout.take().expect("piped");
        let mut stderr = child.stderr.take().expect("piped");
        let out_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = stdout.read_to_end(&mut buf).await;
            buf
        });
        let err_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = stderr.read_to_end(&mut buf).await;
            buf
        });

        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let status = loop {
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    let _ = child.kill().await;
                    return Err(AppError::BadRequest("canceled".into()));
                }
                status = child.wait() => break status?,
                _ = tick.tick() => {
                    let root = self.dir.to_path_buf();
                    if let Ok(Ok((_, bytes))) = tokio::task::spawn_blocking(move || walk(&root)).await {
                        self.progress.store(bytes, Ordering::Relaxed);
                    }
                }
            }
        };
        let stdout = out_task.await.unwrap_or_default();
        let stderr = err_task.await.unwrap_or_default();
        if !status.success() {
            let stderr = String::from_utf8_lossy(&stderr);
            let tail: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
            let tail = tail[tail.len().saturating_sub(4)..].join(" | ");
            return Err(AppError::Upstream(format!("git {} failed: {tail}", args[0])));
        }
        Ok(String::from_utf8_lossy(&stdout).into_owned())
    }
}

/// What git needs from the service's environment; everything else is cleared.
fn passthrough_env() -> impl Iterator<Item = (String, String)> {
    std::env::vars().filter(|(k, _)| {
        matches!(
            k.as_str(),
            "PATH" | "HOME" | "LANG" | "TZ" | "SSL_CERT_FILE" | "NIX_SSL_CERT_FILE"
        )
    })
}

fn git_env(token: Option<&str>) -> Vec<(String, String)> {
    let mut config = vec![
        ("filter.lfs.process", "git-lfs filter-process".to_string()),
        ("filter.lfs.smudge", "git-lfs smudge -- %f".to_string()),
        ("filter.lfs.clean", "git-lfs clean -- %f".to_string()),
        ("filter.lfs.required", "true".to_string()),
        ("advice.detachedHead", "false".to_string()),
        ("init.defaultBranch", "main".to_string()),
    ];
    if let Some(token) = token {
        // GitHub's git and LFS endpoints take the token as basic auth; scoped to github.com so
        // it isn't sent to the LFS storage hosts GitHub redirects to.
        let basic = base64(format!("x-access-token:{token}").as_bytes());
        config.push((
            "http.https://github.com/.extraheader",
            format!("Authorization: Basic {basic}"),
        ));
    }
    let mut env = vec![
        ("GIT_TERMINAL_PROMPT".to_string(), "0".to_string()),
        ("GIT_CONFIG_NOSYSTEM".to_string(), "1".to_string()),
        ("GIT_CONFIG_GLOBAL".to_string(), "/dev/null".to_string()),
        ("GIT_CONFIG_COUNT".to_string(), config.len().to_string()),
    ];
    for (i, (key, value)) in config.into_iter().enumerate() {
        env.push((format!("GIT_CONFIG_KEY_{i}"), key.to_string()));
        env.push((format!("GIT_CONFIG_VALUE_{i}"), value));
    }
    env
}

/// Branch, tag or commit names only; never something git could read as an option.
fn validate_revision(revision: &str) -> AppResult<()> {
    let ok = !revision.is_empty()
        && !revision.starts_with('-')
        && !revision.contains("..")
        && revision.chars().all(|c| c.is_ascii_alphanumeric() || "._/-".contains(c));
    if ok {
        Ok(())
    } else {
        Err(AppError::BadRequest(format!("invalid git revision {revision:?}")))
    }
}

/// Every regular file below `root` (relative paths) and their total size. Symlinks count as
/// entries but aren't followed.
fn walk(root: &Path) -> std::io::Result<(Vec<String>, u64)> {
    let mut files = Vec::new();
    let mut bytes = 0;
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            let path = entry.path();
            if ft.is_dir() {
                stack.push(path);
            } else {
                bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
                if let Ok(rel) = path.strip_prefix(root)
                    && !rel.starts_with(".git")
                {
                    files.push(rel.to_string_lossy().into_owned());
                }
            }
        }
    }
    files.sort();
    Ok((files, bytes))
}

fn base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648() {
        for (raw, enc) in
            [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foobar", "Zm9vYmFy")]
        {
            assert_eq!(base64(raw.as_bytes()), enc);
        }
    }

    #[test]
    fn revisions() {
        for ok in ["main", "HEAD", "v1.2.0", "feature/x", "0123abcd"] {
            assert!(validate_revision(ok).is_ok(), "{ok}");
        }
        for bad in ["", "--upload-pack=x", "-x", "a..b", "a b", "a;b", "$(x)"] {
            assert!(validate_revision(bad).is_err(), "{bad}");
        }
    }
}
