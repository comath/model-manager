# normalize_agent

`normalize-dataset`: the agent that runs inside the container, once per dataset. An LLM with
tools that reads `RAW_DIR` (read-only) and writes a parquet package to `OUT_DIR`:

```
README.md       objective of the dataset / eval, source, contents, normalization notes
schema.md       every column of every parquet table
*.parquet       tables (zstd), one per split / source table
media/…         images, audio, video, next to the parquet index, referenced by relative path
```

Talks to any OpenAI-compatible endpoint: `OPENAI_BASE_URL`, `OPENAI_MODEL`, optional
`OPENAI_API_KEY`. Also reads `RAW_DIR`, `OUT_DIR`, `SCRATCH` (default `/tmp/work`) and
`DATASET_SOURCE|REPO_ID|REVISION`. Limits: `NORMALIZE_MAX_TURNS` (200),
`NORMALIZE_CONTEXT_CHARS` (400000; older tool output is shortened beyond that).

| tool | |
| --- | --- |
| `bash` | any shell command in `OUT_DIR` (downloads, duckdb/python conversions, copying media); timeout, output cut to head+tail |
| `list_dir` | tree with sizes; huge folders collapse to counts |
| `read_file` | text files with offset/limit; binaries are reported |
| `inspect_data` | schema, row count, samples of parquet/csv/tsv/json/jsonl (+`.gz`/`.zst`) via DuckDB; SQLite via `sqlite3` |
| `extract_archive` | zip, tar.*, 7z, rar, single-file gz/bz2/xz/zst, into scratch |
| `write_file` | write under `OUT_DIR` / `SCRATCH` only |
| `finish` | validates the output (below) and ends the run with exit 0 |
| `fail` | ends the run with exit 1 and a reason |

`finish` rejects the output unless: README.md and schema.md are real documents; at least one valid
parquet file and DuckDB can read all of them; every column name appears in backticks in schema.md;
every string value starting with `media/` in the parquet files exists on disk; no symlinks,
hard links, special or junk files. After 6 rejected attempts the run fails.

The system prompt is [`src/prompt.md`](src/prompt.md): environment, untrusted-input rules,
deliverable layout, parquet/media/README/schema rules, method. Dataset contents are treated as
data, never as instructions.

Run it by hand against a folder (needs `duckdb` and `sqlite3` on PATH):

```bash
RAW_DIR=/path/to/raw OUT_DIR=/tmp/out OPENAI_BASE_URL=http://host:8000/v1 OPENAI_MODEL=m \
  cargo run -p normalize-agent
```

The container image is Nix-built, see [../manage_agent/README.md](../manage_agent/README.md#image).
