import { useEffect, useState } from "react";
import { Link, useSearchParams } from "react-router";
import { api, formatCount } from "../api";
import { KindPicker } from "../components/KindPicker";
import type { ModelKind, ModelSummary } from "../types";
import { isKind, KIND_LABEL } from "../types";

const SUGGESTIONS = ["qwen3", "llama-3.1", "mistral", "gemma", "deepseek", "phi-4"];

export function SearchPage() {
  const [params, setParams] = useSearchParams();
  const kind: ModelKind = isKind(params.get("kind")) ? (params.get("kind") as ModelKind) : "gguf";
  const query = params.get("q") ?? "";

  const [draft, setDraft] = useState(query);
  const [results, setResults] = useState<ModelSummary[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => setDraft(query), [query]);

  useEffect(() => {
    const controller = new AbortController();
    setLoading(true);
    setError(null);
    // Debounce so typing in the box does not hammer the Hub.
    const timer = setTimeout(() => {
      api
        .search(query, kind, controller.signal)
        .then(setResults)
        .catch((err: Error) => {
          if (err.name !== "AbortError") setError(err.message);
        })
        .finally(() => setLoading(false));
    }, 250);

    return () => {
      clearTimeout(timer);
      controller.abort();
    };
  }, [query, kind]);

  const update = (next: { q?: string; kind?: ModelKind }) => {
    const merged = new URLSearchParams(params);
    if (next.q !== undefined) merged.set("q", next.q);
    if (next.kind !== undefined) merged.set("kind", next.kind);
    setParams(merged, { replace: true });
  };

  return (
    <section className="stack">
      <div className="panel hero">
        <h1>Pull a model off the Hub</h1>
        <p className="muted">
          Pick the format your runtime wants — safetensors for vLLM, GGUF for llama.cpp — and
          download straight to this machine.
        </p>
        <KindPicker value={kind} onChange={(next) => update({ kind: next })} />
        <form
          className="search-row"
          onSubmit={(event) => {
            event.preventDefault();
            update({ q: draft });
          }}
        >
          <input
            type="search"
            value={draft}
            placeholder={`Search ${KIND_LABEL[kind]} repos…`}
            onChange={(event) => {
              setDraft(event.target.value);
              update({ q: event.target.value });
            }}
            autoFocus
          />
          <button className="button" type="submit">
            Search
          </button>
        </form>
        <div className="suggestions">
          {SUGGESTIONS.map((s) => (
            <button key={s} type="button" className="chip" onClick={() => update({ q: s })}>
              {s}
            </button>
          ))}
        </div>
      </div>

      {error && <p className="panel error">{error}</p>}

      <div className="panel">
        <div className="panel-head">
          <h2>{query ? `Results for “${query}”` : "Most downloaded"}</h2>
          {loading && <span className="muted">searching…</span>}
        </div>
        {!loading && results.length === 0 && !error && <p className="muted">Nothing matched.</p>}
        <ul className="result-list">
          {results.map((model) => (
            <li key={model.id}>
              <Link className="result" to={`/model/${model.id}?kind=${kind}`}>
                <div className="result-main">
                  <span className="result-id">{model.id}</span>
                  <span className="tag-row">
                    {model.pipeline_tag && <span className="tag">{model.pipeline_tag}</span>}
                    {model.gated && <span className="tag warn">gated</span>}
                  </span>
                </div>
                <div className="result-meta">
                  <span>{formatCount(model.downloads)} downloads</span>
                  <span>♥ {formatCount(model.likes)}</span>
                </div>
              </Link>
            </li>
          ))}
        </ul>
      </div>
    </section>
  );
}
