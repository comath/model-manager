//! The agent's tools: definitions (JSON schema for the model) and implementations.

use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Component, Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use serde_json::{Value, json};
use tokio::{io::AsyncReadExt, process::Command, time::timeout};

const BASH_CAP: usize = 2 * 1024 * 1024;
const OUTPUT_HEAD: usize = 6000;
const OUTPUT_TAIL: usize = 8000;

pub struct Ctx {
    pub raw: PathBuf,
    pub out: PathBuf,
    pub scratch: PathBuf,
}

/// Result of running one tool call.
pub enum Outcome {
    Text(String),
    Finish,
    Fail(String),
}

pub fn definitions() -> Value {
    let f = |name: &str, desc: &str, props: Value, required: Value| {
        json!({"type": "function", "function": {
            "name": name, "description": desc,
            "parameters": {"type": "object", "properties": props, "required": required}
        }})
    };
    json!([
        f("bash",
          "Run a shell command (bash) with OUT_DIR as the working directory. stdout and stderr are merged; \
           long output is truncated to head and tail, so redirect big output to a file and read slices. \
           Network access is available. Use for downloads, conversions (duckdb/python), copying media, checks.",
          json!({
            "command": {"type": "string", "description": "Shell script to run."},
            "timeout_secs": {"type": "integer", "description": "Kill after this many seconds (default 600, max 7200)."}
          }), json!(["command"])),
        f("list_dir",
          "Tree view of a directory with file sizes. Large folders are collapsed to counts. Defaults to the raw dataset.",
          json!({
            "path": {"type": "string", "description": "Directory (default: RAW_DIR)."},
            "depth": {"type": "integer", "description": "Levels to show (default 2, max 6)."},
            "per_dir": {"type": "integer", "description": "Max entries listed per directory (default 40)."}
          }), json!([])),
        f("read_file",
          "Read a text file (README, dataset card, scripts, metadata). Binary files are reported, not dumped.",
          json!({
            "path": {"type": "string"},
            "offset": {"type": "integer", "description": "Byte offset to start at (default 0)."},
            "limit": {"type": "integer", "description": "Max bytes (default 20000, max 60000)."}
          }), json!(["path"])),
        f("inspect_data",
          "Schema, row count and sample rows of a parquet/csv/tsv/json/jsonl file (optionally .gz/.zst) or \
           a SQLite database (tables, schemas; pass `table` for rows).",
          json!({
            "path": {"type": "string"},
            "rows": {"type": "integer", "description": "Sample rows (default 5, max 50)."},
            "table": {"type": "string", "description": "SQLite table to sample."}
          }), json!(["path"])),
        f("extract_archive",
          "Safely unpack zip, tar, tar.gz/.bz2/.xz/.zst, 7z, rar, or a single-file gz/bz2/xz/zst. \
           Destination must be under SCRATCH or OUT_DIR (default: a new folder in SCRATCH). Lists what it extracted.",
          json!({
            "path": {"type": "string", "description": "Archive file."},
            "dest": {"type": "string", "description": "Destination directory."}
          }), json!(["path"])),
        f("write_file",
          "Create or overwrite a text file under OUT_DIR (relative paths are relative to OUT_DIR) or SCRATCH. \
           Use for README.md, schema.md and helper scripts.",
          json!({"path": {"type": "string"}, "content": {"type": "string"}}), json!(["path", "content"])),
        f("finish",
          "Validate the deliverable and end the run successfully. Returns a list of problems if it is not ready; fix them and call again.",
          json!({"summary": {"type": "string", "description": "One-paragraph summary of what you produced."}}), json!([])),
        f("fail",
          "End the run as failed because the dataset cannot be normalized (gated, gone, not a dataset, ...). Give a precise reason.",
          json!({"reason": {"type": "string"}}), json!(["reason"])),
    ])
}

pub fn truncate(s: &str, head: usize, tail: usize) -> String {
    if s.len() <= head + tail {
        return s.to_string();
    }
    let mut h = head;
    while !s.is_char_boundary(h) {
        h -= 1;
    }
    let mut t = s.len() - tail;
    while !s.is_char_boundary(t) {
        t += 1;
    }
    format!("{}\n…[{} bytes omitted]…\n{}", &s[..h], t - h, &s[t..])
}

fn human(n: u64) -> String {
    let mut v = n as f64;
    for unit in ["B", "KB", "MB", "GB", "TB"] {
        if v < 1024.0 || unit == "TB" {
            return if unit == "B" { format!("{n} B") } else { format!("{v:.1} {unit}") };
        }
        v /= 1024.0;
    }
    unreachable!()
}

/// Resolve `..`/`.` lexically (the path may not exist yet).
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn arg_str<'a>(a: &'a Value, k: &str) -> Option<&'a str> {
    a.get(k).and_then(Value::as_str)
}
fn arg_u64(a: &Value, k: &str) -> Option<u64> {
    a.get(k).and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
}

fn sql_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

impl Ctx {
    fn resolve(&self, p: &str, base: &Path) -> PathBuf {
        let p = Path::new(p);
        normalize(&if p.is_absolute() { p.to_path_buf() } else { base.join(p) })
    }

    fn writable(&self, p: &Path) -> bool {
        p.starts_with(&self.out) || p.starts_with(&self.scratch)
    }

    pub async fn run(&self, name: &str, args: &Value) -> Outcome {
        let r = match name {
            "bash" => self.bash(args).await,
            "list_dir" => self.list_dir(args),
            "read_file" => self.read_file(args),
            "inspect_data" => self.inspect_data(args).await,
            "extract_archive" => self.extract_archive(args).await,
            "write_file" => self.write_file(args),
            "finish" => {
                return match crate::validate::validate(self).await {
                    Ok(report) => {
                        println!("finish: {report}");
                        Outcome::Finish
                    }
                    Err(problems) => Outcome::Text(format!(
                        "The deliverable is not ready. Fix these and call finish again:\n{problems}"
                    )),
                };
            }
            "fail" => return Outcome::Fail(arg_str(args, "reason").unwrap_or("no reason given").to_string()),
            other => Err(format!("unknown tool {other:?}")),
        };
        Outcome::Text(r.unwrap_or_else(|e| format!("error: {e}")))
    }

    // --- bash ----------------------------------------------------------------------------

    async fn bash(&self, a: &Value) -> Result<String, String> {
        let cmd = arg_str(a, "command").ok_or("`command` is required")?;
        let secs = arg_u64(a, "timeout_secs").unwrap_or(600).clamp(1, 7200);
        let mut c = Command::new("bash");
        c.arg("-c")
            .arg(format!("exec 2>&1\n{cmd}"))
            .current_dir(&self.out)
            .env("SCRATCH", &self.scratch)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .process_group(0);
        let mut child = c.spawn().map_err(|e| e.to_string())?;
        let pid = child.id();
        let mut stdout = child.stdout.take().expect("piped");
        let work = async move {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                let n = stdout.read(&mut chunk).await?;
                if n == 0 {
                    break;
                }
                if buf.len() < BASH_CAP {
                    buf.extend_from_slice(&chunk[..n]);
                }
            }
            let status = child.wait().await?;
            Ok::<_, std::io::Error>((buf, status))
        };
        match timeout(Duration::from_secs(secs), work).await {
            Ok(Ok((buf, status))) => {
                let text = String::from_utf8_lossy(&buf);
                let code = status.code().map_or("killed by signal".to_string(), |c| format!("exit status {c}"));
                Ok(format!("{}\n[{code}]", truncate(&text, OUTPUT_HEAD, OUTPUT_TAIL)))
            }
            Ok(Err(e)) => Err(e.to_string()),
            Err(_) => {
                if let Some(pid) = pid {
                    let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid as i32), nix::sys::signal::Signal::SIGKILL);
                }
                Err(format!("timed out after {secs}s and was killed (raise timeout_secs, or run it in the background writing to a log file)"))
            }
        }
    }

    pub async fn duckdb(&self, sql: &str) -> Result<String, String> {
        let run = Command::new("duckdb")
            .args(["-json", "-c", sql])
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output();
        match timeout(Duration::from_secs(600), run).await {
            Err(_) => Err("duckdb timed out".into()),
            Ok(Err(e)) => Err(format!("cannot run duckdb: {e}")),
            Ok(Ok(o)) if o.status.success() => Ok(String::from_utf8_lossy(&o.stdout).to_string()),
            Ok(Ok(o)) => Err(String::from_utf8_lossy(&o.stderr).trim().to_string()),
        }
    }

    // --- list_dir ------------------------------------------------------------------------

    fn list_dir(&self, a: &Value) -> Result<String, String> {
        let path = arg_str(a, "path").map_or_else(|| self.raw.clone(), |p| self.resolve(p, &self.out));
        let depth = arg_u64(a, "depth").unwrap_or(2).clamp(1, 6) as usize;
        let per_dir = arg_u64(a, "per_dir").unwrap_or(40).clamp(5, 500) as usize;
        let md = fs::symlink_metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if !md.is_dir() {
            return Ok(format!("{} ({})", path.display(), human(md.len())));
        }
        let mut out = format!("{}/\n", path.display());
        let mut lines = 0usize;
        tree(&path, 1, depth, per_dir, &mut out, &mut lines);
        Ok(out)
    }

    // --- read_file -----------------------------------------------------------------------

    fn read_file(&self, a: &Value) -> Result<String, String> {
        let path = self.resolve(arg_str(a, "path").ok_or("`path` is required")?, &self.out);
        let offset = arg_u64(a, "offset").unwrap_or(0);
        let limit = arg_u64(a, "limit").unwrap_or(20_000).clamp(1, 60_000) as usize;
        let mut f = fs::File::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let size = f.metadata().map_err(|e| e.to_string())?.len();
        f.seek(SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
        let mut buf = Vec::new();
        f.take(limit as u64).read_to_end(&mut buf).map_err(|e| e.to_string())?;
        if buf.iter().take(4096).any(|&b| b == 0) {
            return Ok(format!(
                "{} is a binary file ({}). Use inspect_data for data files, or bash (`file`, `xxd | head`).",
                path.display(),
                human(size)
            ));
        }
        let end = offset + buf.len() as u64;
        let mut s = String::from_utf8_lossy(&buf).to_string();
        if end < size {
            s.push_str(&format!("\n…[showing bytes {offset}-{end} of {size}; pass offset={end} for more]"));
        }
        Ok(s)
    }

    // --- write_file ----------------------------------------------------------------------

    fn write_file(&self, a: &Value) -> Result<String, String> {
        let path = self.resolve(arg_str(a, "path").ok_or("`path` is required")?, &self.out);
        let content = arg_str(a, "content").ok_or("`content` is required")?;
        if !self.writable(&path) {
            return Err(format!("{} is outside OUT_DIR and SCRATCH", path.display()));
        }
        if path.file_name().is_some_and(|n| n == ".normalize.json") {
            return Err("that file name is reserved".into());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        fs::write(&path, content).map_err(|e| e.to_string())?;
        Ok(format!("wrote {} bytes to {}", content.len(), path.display()))
    }

    // --- extract_archive -----------------------------------------------------------------

    async fn extract_archive(&self, a: &Value) -> Result<String, String> {
        let path = self.resolve(arg_str(a, "path").ok_or("`path` is required")?, &self.out);
        if !path.is_file() {
            return Err(format!("{} is not a file", path.display()));
        }
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
        let stem = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
        let dest = match arg_str(a, "dest") {
            Some(d) => self.resolve(d, &self.scratch),
            None => self.scratch.join("extracted").join(stem.trim_end_matches(".tar")),
        };
        if !self.writable(&dest) {
            return Err(format!("{} is outside SCRATCH and OUT_DIR", dest.display()));
        }
        fs::create_dir_all(&dest).map_err(|e| e.to_string())?;

        let single: Option<(&str, &[&str])> = [
            (".gz", ("gzip", &["-dc"][..])),
            (".bz2", ("bzip2", &["-dc"][..])),
            (".xz", ("xz", &["-dc"][..])),
            (".zst", ("zstd", &["-dc"][..])),
        ]
        .into_iter()
        .find(|(ext, _)| name.ends_with(ext))
        .map(|(_, t)| t);
        let tarball = [".tar", ".tar.gz", ".tgz", ".tar.bz2", ".tbz2", ".tar.xz", ".txz", ".tar.zst", ".tzst"]
            .iter()
            .any(|e| name.ends_with(e));

        let result = if let (Some((tool, flags)), false) = (single, tarball) {
            // A lone compressed file: decompress to <dest>/<name without extension>.
            let target = dest.join(path.file_stem().unwrap_or_default());
            let f = fs::File::create(&target).map_err(|e| e.to_string())?;
            let child = Command::new(tool).args(flags).arg(&path).stdout(f).stdin(Stdio::null()).kill_on_drop(true).output();
            run_cmd(child, tool).await
        } else {
            // bsdtar reads zip, tar.*, 7z and rar; it refuses `..` and absolute member paths.
            let first = run_cmd(
                Command::new("bsdtar").arg("-xf").arg(&path).arg("-C").arg(&dest).stdin(Stdio::null()).kill_on_drop(true).output(),
                "bsdtar",
            )
            .await;
            if first.is_err() && (name.ends_with(".zip") || name.ends_with(".7z") || name.ends_with(".rar")) {
                let mut o = std::ffi::OsString::from("-o");
                o.push(&dest);
                let second = run_cmd(
                    Command::new("7z").args(["x", "-y"]).arg(o).arg(&path).stdin(Stdio::null()).kill_on_drop(true).output(),
                    "7z",
                )
                .await;
                second.map_err(|e2| format!("{}; fallback: {e2}", first.unwrap_err()))
            } else {
                first
            }
        };
        result?;

        let mut files = Vec::new();
        let mut total = 0u64;
        collect(&dest, &dest, &mut files, &mut total, 100_000);
        files.sort();
        let mut s = format!("extracted to {}: {} files, {}\n", dest.display(), files.len(), human(total));
        for f in files.iter().take(40) {
            s.push_str(&format!("  {f}\n"));
        }
        if files.len() > 40 {
            s.push_str(&format!("  … {} more (use list_dir)\n", files.len() - 40));
        }
        Ok(s)
    }

    // --- inspect_data --------------------------------------------------------------------

    async fn inspect_data(&self, a: &Value) -> Result<String, String> {
        let path = self.resolve(arg_str(a, "path").ok_or("`path` is required")?, &self.out);
        let rows = arg_u64(a, "rows").unwrap_or(5).clamp(1, 50);
        if !path.is_file() {
            return Err(format!("{} is not a file", path.display()));
        }
        let p = path.to_string_lossy().to_string();
        let lower = p.to_lowercase();
        let base = lower.trim_end_matches(".gz").trim_end_matches(".zst");
        let ext = base.rsplit('.').next().unwrap_or("");

        if matches!(ext, "sqlite" | "sqlite3" | "db" | "db3") {
            return self.inspect_sqlite(&p, arg_str(a, "table"), rows).await;
        }
        let src = match ext {
            "parquet" => format!("read_parquet({})", sql_str(&p)),
            "csv" => format!("read_csv({})", sql_str(&p)),
            "tsv" => format!("read_csv({}, delim='\\t')", sql_str(&p)),
            "json" | "jsonl" | "ndjson" => format!("read_json_auto({})", sql_str(&p)),
            _ => {
                return Err(format!(
                    "no built-in reader for .{ext}; use bash (python/pandas/openpyxl, `file`, `head`)"
                ));
            }
        };
        let mut s = format!("{} ({})\n", p, human(fs::metadata(&path).map_or(0, |m| m.len())));
        s.push_str(&format!(
            "\n## schema\n{}",
            self.duckdb(&format!("DESCRIBE SELECT * FROM {src}")).await?
        ));
        s.push_str(&format!(
            "\n## rows\n{}",
            self.duckdb(&format!("SELECT count(*) AS rows FROM {src}")).await?
        ));
        if ext == "parquet" {
            // Hugging Face writes the feature types (Image, Audio, ClassLabel...) here.
            if let Ok(kv) = self
                .duckdb(&format!(
                    "SELECT key::VARCHAR AS key, substr(value::VARCHAR, 1, 3000) AS value FROM parquet_kv_metadata({})",
                    sql_str(&p)
                ))
                .await
            {
                if kv.trim().len() > 2 {
                    s.push_str(&format!("\n## parquet key/value metadata\n{kv}"));
                }
            }
        }
        // Cells are cut to 200 chars so embedded blobs and long texts don't flood the context.
        let cut = format!(
            "SELECT substr(COLUMNS(*)::VARCHAR, 1, 200) AS \"\\0\" FROM {src} LIMIT {rows}"
        );
        let sample = match self.duckdb(&cut).await {
            Ok(v) => v,
            Err(_) => self.duckdb(&format!("SELECT * FROM {src} LIMIT {rows}")).await?,
        };
        s.push_str(&format!("\n## first {rows} rows (cells cut to 200 chars)\n{sample}"));
        Ok(truncate(&s, 12_000, 4_000))
    }

    async fn inspect_sqlite(&self, p: &str, table: Option<&str>, rows: u64) -> Result<String, String> {
        let q = |sql: &str| {
            let sql = sql.to_string();
            let p = p.to_string();
            async move {
                let o = timeout(
                    Duration::from_secs(300),
                    Command::new("sqlite3").args(["-readonly", "-json", &p, &sql]).stdin(Stdio::null()).kill_on_drop(true).output(),
                )
                .await
                .map_err(|_| "sqlite3 timed out".to_string())?
                .map_err(|e| format!("cannot run sqlite3: {e}"))?;
                if o.status.success() {
                    Ok(String::from_utf8_lossy(&o.stdout).to_string())
                } else {
                    Err(String::from_utf8_lossy(&o.stderr).trim().to_string())
                }
            }
        };
        let quote = |t: &str| format!("\"{}\"", t.replace('"', "\"\""));
        let mut s = String::new();
        match table {
            Some(t) => {
                s.push_str(&format!("## {t}: columns\n{}", q(&format!("PRAGMA table_info({})", quote(t))).await?));
                s.push_str(&format!("\n## rows\n{}", q(&format!("SELECT count(*) AS rows FROM {}", quote(t))).await?));
                s.push_str(&format!(
                    "\n## first {rows} rows\n{}",
                    q(&format!("SELECT * FROM {} LIMIT {rows}", quote(t))).await?
                ));
            }
            None => {
                let listing = q("SELECT name, type, sql FROM sqlite_master WHERE type IN ('table','view') ORDER BY name").await?;
                s.push_str(&format!("## tables and views\n{listing}"));
                let names: Vec<String> = serde_json::from_str::<Vec<Value>>(listing.trim())
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|r| r.get("name").and_then(Value::as_str).map(String::from))
                    .take(100)
                    .collect();
                s.push_str("\n## row counts\n");
                for n in names {
                    let c = q(&format!("SELECT count(*) AS c FROM {}", quote(&n))).await.unwrap_or_else(|e| e);
                    s.push_str(&format!("{n}: {}\n", c.trim()));
                }
                s.push_str("\n(pass `table` to see columns and sample rows)");
            }
        }
        Ok(truncate(&s, 12_000, 4_000))
    }
}

async fn run_cmd(
    fut: impl std::future::Future<Output = std::io::Result<std::process::Output>>,
    tool: &str,
) -> Result<(), String> {
    match timeout(Duration::from_secs(3600), fut).await {
        Err(_) => Err(format!("{tool} timed out")),
        Ok(Err(e)) => Err(format!("cannot run {tool}: {e}")),
        Ok(Ok(o)) if o.status.success() => Ok(()),
        Ok(Ok(o)) => Err(format!(
            "{tool} failed: {}",
            truncate(String::from_utf8_lossy(&o.stderr).trim(), 1500, 1500)
        )),
    }
}

fn collect(root: &Path, dir: &Path, files: &mut Vec<String>, total: &mut u64, cap: usize) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        if files.len() >= cap {
            return;
        }
        let Ok(md) = fs::symlink_metadata(e.path()) else { continue };
        if md.is_dir() {
            collect(root, &e.path(), files, total, cap);
        } else {
            *total += md.len();
            files.push(e.path().strip_prefix(root).unwrap_or(&e.path()).display().to_string());
        }
    }
}

fn tree(dir: &Path, level: usize, max_depth: usize, per_dir: usize, out: &mut String, lines: &mut usize) {
    const MAX_LINES: usize = 400;
    let Ok(rd) = fs::read_dir(dir) else { return };
    let mut entries: Vec<(String, fs::Metadata)> = rd
        .flatten()
        .filter_map(|e| Some((e.file_name().to_string_lossy().to_string(), fs::symlink_metadata(e.path()).ok()?)))
        .collect();
    entries.sort_by(|a, b| b.1.is_dir().cmp(&a.1.is_dir()).then(a.0.cmp(&b.0)));
    let indent = "  ".repeat(level);
    let total = entries.len();
    for (name, md) in entries.iter().take(per_dir) {
        if *lines >= MAX_LINES {
            out.push_str(&format!("{indent}… (output limit reached; list subfolders separately)\n"));
            return;
        }
        *lines += 1;
        if md.is_dir() {
            if level >= max_depth {
                let (mut files, mut bytes) = (Vec::new(), 0u64);
                collect(dir, &dir.join(name), &mut files, &mut bytes, 50_000);
                let plus = if files.len() >= 50_000 { "+" } else { "" };
                out.push_str(&format!("{indent}{name}/  ({}{plus} files, {})\n", files.len(), human(bytes)));
            } else {
                out.push_str(&format!("{indent}{name}/\n"));
                tree(&dir.join(name), level + 1, max_depth, per_dir, out, lines);
            }
        } else if md.file_type().is_symlink() {
            out.push_str(&format!("{indent}{name}  (symlink)\n"));
        } else {
            out.push_str(&format!("{indent}{name}  {}\n", human(md.len())));
        }
    }
    if total > per_dir {
        let rest = &entries[per_dir..];
        let dirs = rest.iter().filter(|e| e.1.is_dir()).count();
        let bytes: u64 = rest.iter().filter(|e| e.1.is_file()).map(|e| e.1.len()).sum();
        let mut exts: std::collections::BTreeMap<String, usize> = Default::default();
        for (n, _) in rest.iter().filter(|e| e.1.is_file()) {
            let ext = Path::new(n).extension().map_or("(none)".to_string(), |e| format!(".{}", e.to_string_lossy()));
            *exts.entry(ext).or_default() += 1;
        }
        let mut ex: Vec<_> = exts.into_iter().collect();
        ex.sort_by(|a, b| b.1.cmp(&a.1));
        let ex: Vec<String> = ex.iter().take(5).map(|(e, n)| format!("{n}×{e}")).collect();
        out.push_str(&format!(
            "{indent}… {} more entries ({dirs} dirs, {} files, {}; {})\n",
            total - per_dir,
            total - per_dir - dirs,
            human(bytes),
            ex.join(", ")
        ));
    }
}
