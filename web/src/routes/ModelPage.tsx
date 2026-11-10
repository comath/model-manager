import { useEffect, useMemo, useState } from "react";
import { Link, useNavigate, useParams, useSearchParams } from "react-router";
import { api, formatBytes, formatCount } from "../api";
import { useJobs } from "../jobs";
import type { ModelKind, RepoDetail } from "../types";
import { isKind, KIND_LABEL } from "../types";

const STORE_KEY = "model-manager.store";

function readStoredStore(): string | null {
  try {
    return localStorage.getItem(STORE_KEY);
  } catch {
    return null;
  }
}

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
  const [store, setStore] = useState<string | null>(readStoredStore);

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
        store: (detail.stores.find((s) => s.name === store) ?? detail.stores[0])?.name,
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

  // Last-used store if it still exists, else the server's default (first).
  const activeStore = detail.stores.find((s) => s.name === store) ?? detail.stores[0];
  const pickStore = (name: string) => {
    setStore(name);
    try {
      localStorage.setItem(STORE_KEY, name);
    } catch {
      // Storage unavailable (private mode); the choice just won't stick.
    }
  };

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
        {detail.stores.length > 1 && (
          <div className="kind-picker" role="radiogroup" aria-label="Download to">
            {detail.stores.map((s) => (
              <button
                key={s.name}
                type="button"
                role="radio"
                aria-checked={s.name === activeStore?.name}
                className={s.name === activeStore?.name ? "active" : ""}
                onClick={() => pickStore(s.name)}
              >
                {s.name}
              </button>
            ))}
          </div>
        )}
        <p className="muted small">
          Saving to <code>{activeStore?.dest ?? detail.dest}</code>
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
