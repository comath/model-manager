//! The `finish` tool's checks on the deliverable. They mirror (and go beyond) the host's checks,
//! so the agent hears about problems while it can still fix them.

use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Seek, SeekFrom},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use serde_json::Value;

use crate::tools::Ctx;

const MAX_DESCRIBE: usize = 300;
const MAX_PATH_CHECK_FILES: usize = 20;
const MAX_PATH_CHECK_COLUMNS: usize = 12;

#[derive(Default)]
struct Scan {
    problems: Vec<String>,
    parquet: Vec<PathBuf>,
    files: u64,
    bytes: u64,
}

fn scan(out: &Path) -> Scan {
    let mut s = Scan::default();
    walk(out, out, &mut s);
    s.parquet.sort();
    s
}

fn walk(root: &Path, dir: &Path, s: &mut Scan) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let path = e.path();
        let rel = path.strip_prefix(root).unwrap_or(&path).display().to_string();
        let Ok(md) = fs::symlink_metadata(&path) else { continue };
        let name = e.file_name().to_string_lossy().to_string();
        let ft = md.file_type();
        if ft.is_symlink() {
            s.problems.push(format!("symlink: {rel} (copy the real file instead)"));
        } else if ft.is_dir() {
            if matches!(name.as_str(), "__pycache__" | ".cache" | ".ipynb_checkpoints" | ".git") {
                s.problems.push(format!("junk directory in output: {rel}"));
            } else {
                walk(root, &path, s);
            }
        } else if ft.is_file() {
            s.files += 1;
            s.bytes += md.len();
            if md.nlink() > 1 {
                s.problems.push(format!("hard link: {rel} (copy it)"));
            }
            if name == ".normalize.json" || name.ends_with(".part") || name.ends_with(".tmp") || name == ".DS_Store" {
                s.problems.push(format!("stray file in output: {rel} (delete it)"));
            }
            if name.ends_with(".parquet") {
                if md.len() < 12 || !parquet_magic(&path).unwrap_or(false) {
                    s.problems.push(format!("not a valid parquet file: {rel}"));
                } else {
                    s.parquet.push(path);
                }
            }
        } else {
            s.problems.push(format!("special file: {rel}"));
        }
    }
}

fn parquet_magic(p: &Path) -> std::io::Result<bool> {
    let mut f = fs::File::open(p)?;
    let (mut a, mut b) = ([0u8; 4], [0u8; 4]);
    f.read_exact(&mut a)?;
    f.seek(SeekFrom::End(-4))?;
    f.read_exact(&mut b)?;
    Ok(&a == b"PAR1" && &b == b"PAR1")
}

fn sql_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn parse_rows(json: &str) -> Vec<Value> {
    serde_json::from_str::<Vec<Value>>(json.trim()).unwrap_or_default()
}

/// Ok(report) when the deliverable passes; Err(problem list) otherwise.
pub async fn validate(ctx: &Ctx) -> Result<String, String> {
    let out = ctx.out.clone();
    let mut sc = tokio::task::spawn_blocking({
        let out = out.clone();
        move || scan(&out)
    })
    .await
    .map_err(|e| e.to_string())?;

    let mut doc_problems: Vec<String> = Vec::new();
    let mut read_doc = |name: &str, min: u64| -> Option<String> {
        match fs::metadata(out.join(name)) {
            Ok(m) if m.is_file() && m.len() >= min => fs::read_to_string(out.join(name)).ok(),
            Ok(m) if m.is_file() => {
                doc_problems.push(format!("{name} is too short ({} bytes); write a real document", m.len()));
                None
            }
            _ => {
                doc_problems.push(format!("{name} is missing"));
                None
            }
        }
    };
    let _readme = read_doc("README.md", 200);
    let schema_md = read_doc("schema.md", 100);
    sc.problems.extend(doc_problems);
    if sc.parquet.is_empty() {
        sc.problems.push("no .parquet files in the output".into());
    }

    // Columns of every parquet file (evenly sampled when there are very many shards).
    let step = sc.parquet.len().div_ceil(MAX_DESCRIBE).max(1);
    let mut columns: BTreeSet<String> = BTreeSet::new();
    let mut string_cols: Vec<(PathBuf, Vec<String>)> = Vec::new();
    for f in sc.parquet.iter().step_by(step) {
        let sql = format!("DESCRIBE SELECT * FROM read_parquet({})", sql_str(&f.to_string_lossy()));
        match ctx.duckdb(&sql).await {
            Ok(json) => {
                let mut strs = Vec::new();
                for row in parse_rows(&json) {
                    let (Some(name), ty) = (
                        row.get("column_name").and_then(Value::as_str),
                        row.get("column_type").and_then(Value::as_str).unwrap_or(""),
                    ) else {
                        continue;
                    };
                    columns.insert(name.to_string());
                    if ty == "VARCHAR" {
                        strs.push(name.to_string());
                    }
                }
                string_cols.push((f.clone(), strs));
            }
            Err(e) => {
                let rel = f.strip_prefix(&ctx.out).unwrap_or(f).display();
                sc.problems.push(format!("duckdb cannot read {rel}: {}", e.lines().next().unwrap_or("")));
            }
        }
    }

    if let Some(schema) = &schema_md {
        let missing: Vec<&String> = columns.iter().filter(|c| !schema.contains(&format!("`{c}`"))).collect();
        if !missing.is_empty() {
            let shown: Vec<String> = missing.iter().take(20).map(|c| format!("`{c}`")).collect();
            sc.problems.push(format!(
                "schema.md does not document {} column(s): {}{}",
                missing.len(),
                shown.join(", "),
                if missing.len() > 20 { ", ..." } else { "" }
            ));
        }
    }

    // Media paths: any string value starting with `media/` must name an existing file.
    let mut missing_media: Vec<String> = Vec::new();
    let mut checked = 0usize;
    'outer: for (file, cols) in string_cols.iter().take(MAX_PATH_CHECK_FILES) {
        for col in cols.iter().take(MAX_PATH_CHECK_COLUMNS) {
            let q = format!("\"{}\"", col.replace('"', "\"\""));
            let sql = format!(
                "SELECT DISTINCT {q} AS p FROM read_parquet({}) WHERE {q} LIKE 'media/%' LIMIT 2000",
                sql_str(&file.to_string_lossy())
            );
            let Ok(json) = ctx.duckdb(&sql).await else { continue };
            for row in parse_rows(&json) {
                let Some(p) = row.get("p").and_then(Value::as_str) else { continue };
                checked += 1;
                if !ctx.out.join(p).is_file() {
                    missing_media.push(p.to_string());
                    if missing_media.len() >= 10 {
                        break 'outer;
                    }
                }
            }
        }
    }
    if !missing_media.is_empty() {
        sc.problems.push(format!(
            "media paths referenced in parquet but missing on disk (first {}): {}",
            missing_media.len(),
            missing_media.join(", ")
        ));
    }

    if sc.problems.is_empty() {
        Ok(format!(
            "OK: {} parquet file(s), {} columns documented, {} media paths spot-checked, {} files / {:.1} MB total.",
            sc.parquet.len(),
            columns.len(),
            checked,
            sc.files,
            sc.bytes as f64 / 1e6
        ))
    } else {
        Err(sc.problems.iter().map(|p| format!("- {p}")).collect::<Vec<_>>().join("\n"))
    }
}
