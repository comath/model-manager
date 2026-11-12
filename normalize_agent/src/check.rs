//! Validation of the agent's output and of the dataset path model-manager reported.

use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    os::unix::fs::{MetadataExt, PermissionsExt, chown, lchown},
    path::{Path, PathBuf},
};

use crate::{Owner, SOURCES};

const PARQUET_MAGIC: &[u8; 4] = b"PAR1";

pub struct Summary {
    pub parquet: Vec<String>,
    pub bytes: u64,
}

/// The dataset folder model-manager reported, checked to really be one under the root.
/// Returns (canonical folder, path relative to the root). The error is a client error.
pub fn resolve_raw(root: &Path, source: &str, path: &str) -> Result<(PathBuf, PathBuf), String> {
    if !SOURCES.contains(&source) {
        return Err(format!("unknown source {source:?} (one of {})", SOURCES.join(", ")));
    }
    let raw = Path::new(path);
    if !raw.is_absolute() {
        return Err("path must be absolute".into());
    }
    let not_under = || format!("{path} is not a folder under {}", root.display());
    // canonicalize follows symlinks, so a link pointing out of the root fails strip_prefix.
    let raw = fs::canonicalize(raw).map_err(|_| not_under())?;
    let root_real = fs::canonicalize(root).map_err(|_| not_under())?;
    let rel = raw.strip_prefix(&root_real).map_err(|_| not_under())?.to_path_buf();
    let first = rel.components().next().map(|c| c.as_os_str().to_owned());
    if rel.components().count() < 2 || first.as_deref() != Some(std::ffi::OsStr::new(source)) {
        return Err(format!(
            "{path} is not a {source} dataset folder (<root>/{source}/<owner>/<name>)"
        ));
    }
    if !raw.is_dir() {
        return Err(format!("{path} is not a directory"));
    }
    Ok((raw, rel))
}

/// Validate the agent's output and make it safe to keep on the host.
///
/// The container's root writes as host root (shift=true), so nothing it leaves behind may be a
/// symlink, device, fifo, hard link or setuid file; every mode is reset to 0644/0755.
/// Errors are user-facing job failure messages.
pub fn check_output(out: &Path, owner: Owner) -> Result<Summary, String> {
    let mut summary = Summary { parquet: Vec::new(), bytes: 0 };
    walk(out, out, owner, &mut summary)?;
    fs::set_permissions(out, fs::Permissions::from_mode(0o755)).map_err(io)?;
    if let Some((uid, gid)) = owner {
        chown(out, Some(uid), Some(gid)).map_err(io)?;
    }

    let readme = out.join("README.md");
    if !fs::metadata(&readme).map(|m| m.is_file() && m.len() > 0).unwrap_or(false) {
        return Err("output has no README.md".into());
    }
    if summary.parquet.is_empty() {
        return Err("output has no .parquet files".into());
    }
    summary.parquet.sort();
    Ok(summary)
}

fn io(e: std::io::Error) -> String {
    format!("internal error: {e}")
}

fn walk(out: &Path, dir: &Path, owner: Owner, summary: &mut Summary) -> Result<(), String> {
    for entry in fs::read_dir(dir).map_err(io)? {
        let path = entry.map_err(io)?.path();
        let md = fs::symlink_metadata(&path).map_err(io)?;
        let rel = path.strip_prefix(out).unwrap_or(&path).display().to_string();
        let ft = md.file_type();
        if ft.is_symlink() {
            return Err(format!("output contains a symlink: {rel}"));
        } else if ft.is_dir() {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).map_err(io)?;
        } else if ft.is_file() {
            if md.nlink() > 1 {
                return Err(format!("output contains a hard link: {rel}"));
            }
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).map_err(io)?;
            summary.bytes += md.len();
            if path.extension().is_some_and(|e| e == "parquet") {
                if md.len() < 12 || !has_parquet_magic(&path).map_err(io)? {
                    return Err(format!("not a valid parquet file: {rel}"));
                }
                summary.parquet.push(rel);
            }
        } else {
            return Err(format!("output contains a special file: {rel}"));
        }
        if let Some((uid, gid)) = owner {
            lchown(&path, Some(uid), Some(gid)).map_err(io)?;
        }
        if ft.is_dir() {
            walk(out, &path, owner, summary)?;
        }
    }
    Ok(())
}

fn has_parquet_magic(path: &Path) -> std::io::Result<bool> {
    let mut f = fs::File::open(path)?;
    let mut head = [0u8; 4];
    f.read_exact(&mut head)?;
    f.seek(SeekFrom::End(-4))?;
    let mut tail = [0u8; 4];
    f.read_exact(&mut tail)?;
    Ok(&head == PARQUET_MAGIC && &tail == PARQUET_MAGIC)
}
