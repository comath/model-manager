import { useCallback, useEffect, useState } from "react";
import { Link, useNavigate } from "react-router";
import { api, formatBytes } from "../api";
import { CopyButton } from "../components/CopyButton";
import { RecipeEditor } from "../components/RecipeEditor";
import { useJobs } from "../jobs";
import type { LibraryEntry } from "../types";
import { KIND_LABEL, SOURCE_LABEL, sourceQuery } from "../types";

export function LibraryPage() {
  const { libraryVersion } = useJobs();
  const navigate = useNavigate();
  const [models, setModels] = useState<LibraryEntry[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [open, setOpen] = useState<string | null>(null);

  const load = useCallback(() => {
    api
      .library()
      .then(setModels)
      .catch((err: Error) => setError(err.message));
  }, []);

  // Reloads on mount and whenever a download completes or a model is deleted.
  useEffect(load, [load, libraryVersion]);

  const remove = async (entry: LibraryEntry) => {
    if (!confirm(`Delete ${entry.repo_id} (${formatBytes(entry.size_bytes)}) from disk?`)) return;
    try {
      await api.remove(entry.kind, entry.repo_id, entry.store, entry.source);
      load();
    } catch (err) {
      setError((err as Error).message);
    }
  };

  if (error) return <p className="panel error">{error}</p>;
  if (!models) return <p className="panel">Loading library…</p>;

  if (models.length === 0) {
    return (
      <section className="panel empty">
        <h1>Library is empty</h1>
        <p className="muted">Downloaded models and datasets show up here; models come with the command to serve them.</p>
        <Link className="button" to="/">
          Discover models
        </Link>
      </section>
    );
  }

  const total = models.reduce((sum, m) => sum + m.size_bytes, 0);
  // Only label stores when there is more than one to tell apart.
  const multiStore = new Set(models.map((m) => m.store)).size > 1;
  return (
    <section className="stack">
      <div className="panel-head bare">
        <h1>Library</h1>
        <span className="muted">
          {models.length} {models.length === 1 ? "model" : "models"} · {formatBytes(total)}
        </span>
      </div>
      <ul className="stack bare">
        {models.map((entry) => {
          const key = `${entry.store}/${entry.kind}/${entry.source}/${entry.repo_id}`;
          return (
            <li className="panel" key={key}>
              <div className="panel-head">
                <div>
                  <h3>
                    {entry.repo_id}
                    {multiStore && <span className="tag">{entry.store}</span>}
                  </h3>
                  <p className="muted small">
                    {entry.kind === "dataset" ? `dataset · ${SOURCE_LABEL[entry.source]}` : KIND_LABEL[entry.kind]} · {formatBytes(entry.size_bytes)} on disk ·{" "}
                    {entry.kind !== "dataset" && <>~{formatBytes(entry.estimated_vram_bytes)} VRAM · </>}
                    {entry.file_count}{" "}
                    {entry.file_count === 1 ? "file" : "files"} ·{" "}
                    {new Date(entry.downloaded_at).toLocaleString()}
                  </p>
                </div>
                <div className="row-actions">
                  <Link className="button ghost" to={`/model/${entry.repo_id}?kind=${entry.kind}${sourceQuery(entry.kind, entry.source)}`}>
                    Repo
                  </Link>
                  <button type="button" className="button danger" onClick={() => void remove(entry)}>
                    Delete
                  </button>
                </div>
              </div>
              {entry.serve_command && (
                <div className="command">
                  <code>{entry.serve_command}</code>
                  <CopyButton text={entry.serve_command} />
                </div>
              )}
              <p className="muted small">
                <code>{entry.path}</code>
              </p>
              {entry.kind !== "dataset" && (
                <button
                  type="button"
                  className="disclosure"
                  onClick={() => setOpen((cur) => (cur === key ? null : key))}
                >
                  {open === key ? "Hide container recipe" : "Run in a container…"}
                </button>
              )}
              {entry.kind !== "dataset" && open === key && (
                <RecipeEditor
                  kind={entry.kind}
                  repoId={entry.repo_id}
                  store={entry.store}
                  files={entry.files}
                  estimatedVramBytes={entry.estimated_vram_bytes}
                  onLaunched={() => navigate("/running")}
                />
              )}
            </li>
          );
        })}
      </ul>
    </section>
  );
}
