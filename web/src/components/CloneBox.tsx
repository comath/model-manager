import { useState } from "react";
import { useNavigate } from "react-router";
import { api } from "../api";
import { useJobs } from "../jobs";
import type { ModelKind, Source } from "../types";
import { SOURCE_LABEL } from "../types";

const LOOKS_LIKE_URL = /^(https?:\/\/|hf:\/\/|git@)/i;

/** Paste `owner/name` or a repo URL and download every file in it. */
export function CloneBox({ kind, source }: { kind: ModelKind; source: Source }) {
  const navigate = useNavigate();
  const { refresh } = useJobs();
  const [repo, setRepo] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    const value = repo.trim();
    if (!value) return;
    setBusy(true);
    setError(null);
    try {
      // A URL names its own provider; a bare `owner/name` uses the tab and source picked above.
      await api.clone(LOOKS_LIKE_URL.test(value) ? { repo: value } : { repo: value, kind, source });
      await refresh();
      navigate("/downloads");
    } catch (err) {
      setError((err as Error).message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <form className="stack" onSubmit={(event) => void submit(event)}>
      <div className="search-row">
        <input
          type="text"
          value={repo}
          placeholder={`Clone a repo from ${kind === "dataset" ? SOURCE_LABEL[source] : "Hugging Face"}: owner/name, or paste a URL`}
          onChange={(event) => setRepo(event.target.value)}
          spellCheck={false}
          autoFocus
        />
        <button className="button primary" type="submit" disabled={busy || !repo.trim()}>
          {busy ? "Starting…" : "Download all"}
        </button>
      </div>
      {error && <p className="error-text">{error}</p>}
    </form>
  );
}
