import { useEffect, useMemo, useState } from "react";
import { Link, useNavigate, useParams, useSearchParams } from "react-router";
import { api, formatBytes, formatCount } from "../api";
import { useJobs } from "../jobs";
import type { ModelKind, RepoDetail } from "../types";
import { isKind, KIND_LABEL } from "../types";

export function ModelPage() {
  const params = useParams();
  const repoId = params["*"] ?? "";
  const [search] = useSearchParams();
  const kind: ModelKind = isKind(search.get("kind"))
    ? (search.get("kind") as ModelKind)
    : "gguf";
  const navigate = useNavigate();
  const { refresh } = useJobs();

  const [detail, setDetail] = useState<RepoDetail | null>(null);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [showAll, setShowAll] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [starting, setStarting] = useState(false);

  useEffect(() => {
    let live = true;
    setDetail(null);
    setError(null);
    api
      .repo(repoId, kind)
      .then((data) => {
        if (!live) return;
        setDetail(data);
        // Everything the runtime needs is checked by default; extras are not.
        setSelected(new Set(data.files.filter((f) => f.relevant).map((f) => f.path)));
      })
      .catch((err: Error) => live && setError(err.message));
    return () => {
      live = false;
    };
  }, [repoId, kind]);

  const files = useMemo(
    () => (detail ? detail.files.filter((f) => showAll || f.relevant) : []),
    [detail, showAll],
  );
  const totalBytes = useMemo(
    () =>
      detail
        ? detail.files.filter((f) => selected.has(f.path)).reduce((sum, f) => sum + f.size, 0)
        : 0,
    [detail, selected],
  );

  const toggle = (path: string) =>
    setSelected((current) => {
      const next = new Set(current);
      if (!next.delete(path)) next.add(path);
      return next;
    });

  const start = async () => {
    if (!detail) return;
    setStarting(true);
    setError(null);
    try {
      await api.startDownload({
        repo_id: detail.model.id,
        kind,
        revision: detail.revision,
        files: detail.files
          .filter((f) => selected.has(f.path))
          .map((f) => ({ path: f.path, size: f.size })),
      });
      await refresh();
      navigate("/downloads");
    } catch (err) {
      setError((err as Error).message);
    } finally {
      setStarting(false);
    }
  };

  if (error && !detail) {
    return (
      <section className="panel empty">
        <h1>{repoId}</h1>
        <p className="error-text">{error}</p>
        <Link className="button" to="/">
          Back to discover
        </Link>
      </section>
    );
  }

  if (!detail) return <section className="panel">Loading {repoId}…</section>;

  const gguf = kind === "gguf";
  return (
    <section className="stack">
      <div className="panel">
        <div className="panel-head">
          <div>
            <h1>{detail.model.id}</h1>
            <p className="muted">
              {KIND_LABEL[kind]} · revision {detail.revision} ·{" "}
              {formatCount(detail.model.downloads)} downloads
              {detail.model.gated && " · gated repo (needs HF_TOKEN)"}
            </p>
          </div>
          <a
            className="button ghost"
            href={`https://huggingface.co/${detail.model.id}`}
            target="_blank"
            rel="noreferrer"
          >
            View on HF
          </a>
        </div>
        <p className="muted small">
          Saving to <code>{detail.dest}</code>
        </p>
        {gguf && (
          <p className="hint">
            GGUF repos ship one file per quantisation — pick a single quant (e.g. <code>Q4_K_M</code>
            ), or every shard of a split one.
          </p>
        )}
      </div>

      <div className="panel">
        <div className="panel-head">
          <h2>Files</h2>
          <div className="row-actions">
            <label className="toggle">
              <input
                type="checkbox"
                checked={showAll}
                onChange={(event) => setShowAll(event.target.checked)}
              />
              show all {detail.files.length} files
            </label>
            <button
              type="button"
              className="button ghost"
              onClick={() => setSelected(new Set(files.map((f) => f.path)))}
            >
              Select all
            </button>
            <button type="button" className="button ghost" onClick={() => setSelected(new Set())}>
              Clear
            </button>
          </div>
        </div>

        <ul className="file-list">
          {files.map((file) => (
            <li key={file.path}>
              <label className={selected.has(file.path) ? "file selected" : "file"}>
                <input
                  type="checkbox"
                  checked={selected.has(file.path)}
                  onChange={() => toggle(file.path)}
                />
                <span className="file-path">{file.path}</span>
                {!file.relevant && <span className="tag">extra</span>}
                <span className="file-size">{formatBytes(file.size)}</span>
              </label>
            </li>
          ))}
        </ul>
      </div>

      {error && <p className="panel error">{error}</p>}

      <div className="panel action-bar">
        <span>
          <strong>{selected.size}</strong> files · <strong>{formatBytes(totalBytes)}</strong>
        </span>
        <button
          type="button"
          className="button primary"
          disabled={selected.size === 0 || starting}
          onClick={start}
        >
          {starting ? "Starting…" : "Download"}
        </button>
      </div>
    </section>
  );
}
