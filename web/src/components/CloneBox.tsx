import { useRef, useState } from "react";
import { useNavigate } from "react-router";
import { api } from "../api";
import { useJobs } from "../jobs";
import type { ModelKind, Source } from "../types";
import { SOURCE_LABEL } from "../types";

const LOOKS_LIKE_URL = /^(https?:\/\/|hf:\/\/|git@)/i;

/** One repo per line; blank lines and `#` comments are ignored. */
function parseLines(text: string): string[] {
  return text
    .split(/\r?\n/)
    .map((l) => l.trim())
    .filter((l) => l && !l.startsWith("#"));
}

/** Paste `owner/name` or repo URLs (one per line, or load a .txt) and download every file. */
export function CloneBox({ kind, source }: { kind: ModelKind; source: Source }) {
  const navigate = useNavigate();
  const { refresh } = useJobs();
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const [errors, setErrors] = useState<string[]>([]);
  const fileInput = useRef<HTMLInputElement>(null);
  const repos = parseLines(text);

  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    if (repos.length === 0) return;
    setBusy(true);
    setErrors([]);
    const failed: { line: string; message: string }[] = [];
    for (const value of repos) {
      try {
        // A URL names its own provider; a bare `owner/name` uses the tab and source picked above.
        await api.clone(
          LOOKS_LIKE_URL.test(value) ? { repo: value } : { repo: value, kind, source },
        );
      } catch (err) {
        failed.push({ line: value, message: (err as Error).message });
      }
    }
    await refresh();
    setBusy(false);
    if (failed.length === 0) {
      navigate("/downloads");
    } else {
      // Keep only the failed lines so a retry doesn't re-queue the ones that started.
      setText(failed.map((f) => f.line).join("\n"));
      setErrors(failed.map((f) => `${f.line}: ${f.message}`));
    }
  };

  const loadFile = async (file: File | undefined) => {
    if (!file) return;
    setText(await file.text());
    if (fileInput.current) fileInput.current.value = "";
  };

  return (
    <form className="stack" onSubmit={(event) => void submit(event)}>
      <textarea
        value={text}
        rows={Math.min(Math.max(repos.length, 1), 8)}
        placeholder={`Clone repos from ${kind === "dataset" ? SOURCE_LABEL[source] : "Hugging Face"}: owner/name or a URL — one per line to download several`}
        onChange={(event) => setText(event.target.value)}
        spellCheck={false}
        autoFocus
        style={{ width: "100%", font: "inherit", resize: "vertical" }}
      />
      <div className="search-row">
        <input
          ref={fileInput}
          type="file"
          accept=".txt,text/plain"
          onChange={(event) => void loadFile(event.target.files?.[0])}
        />
        <button className="button primary" type="submit" disabled={busy || repos.length === 0}>
          {busy
            ? "Starting…"
            : repos.length > 1
              ? `Download all ${repos.length} repos`
              : "Download all"}
        </button>
      </div>
      {errors.map((e) => (
        <p key={e} className="error-text">
          {e}
        </p>
      ))}
    </form>
  );
}
