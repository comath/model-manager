import { useEffect, useState } from "react";
import { Link, useSearchParams } from "react-router";
import { api, formatCount } from "../api";
import { CloneBox } from "../components/CloneBox";
import { KindPicker } from "../components/KindPicker";
import { SourcePicker } from "../components/SourcePicker";
import type { ModelKind, ModelSummary, Source } from "../types";
import { isKind, isSource, KIND_LABEL, SOURCE_LABEL, sourceQuery } from "../types";

const MODEL_SUGGESTIONS = ["qwen3", "llama-3.1", "mistral", "gemma", "deepseek", "phi-4"];
const DATASET_SUGGESTIONS: Record<Source, string[]> = {
  huggingface: ["fineweb", "squad", "gsm8k", "c4", "alpaca", "mmlu"],
  kaggle: ["titanic", "house prices", "mnist", "imdb", "covid", "spotify"],
  github: ["awesome public datasets", "iris", "csv dataset", "benchmark", "corpus"],
};

const PREF_KEY = "model-manager.discover";

/** Last-used kind/source, so the tab you picked yesterday is the one you land on. */
function readPrefs(): { kind?: string; source?: string } {
  try {
    return JSON.parse(localStorage.getItem(PREF_KEY) ?? "{}");
  } catch {
    return {};
  }
}

export function SearchPage() {
  const [params, setParams] = useSearchParams();
  const prefs = readPrefs();
  const rawKind = params.get("kind") ?? prefs.kind ?? null;
  const kind: ModelKind = isKind(rawKind) ? rawKind : "safetensors";
  const rawSource = params.get("source") ?? prefs.source ?? null;
  const source: Source = isSource(rawSource) ? rawSource : "huggingface";
  const query = params.get("q") ?? "";
  const isDataset = kind === "dataset";
  const suggestions = isDataset ? DATASET_SUGGESTIONS[source] : MODEL_SUGGESTIONS;

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
        .search(query, kind, source, controller.signal)
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
  }, [query, kind, source]);

  const update = (next: { q?: string; kind?: ModelKind; source?: Source }) => {
    const merged = new URLSearchParams(params);
    if (next.q !== undefined) merged.set("q", next.q);
    if (next.kind !== undefined) merged.set("kind", next.kind);
    if (next.source !== undefined) merged.set("source", next.source);
    setParams(merged, { replace: true });
    if (next.kind !== undefined || next.source !== undefined) {
      try {
        localStorage.setItem(
          PREF_KEY,
          JSON.stringify({ kind: next.kind ?? kind, source: next.source ?? source }),
        );
      } catch {
        // Storage unavailable; the choice just won't stick.
      }
    }
  };

  return (
    <section className="stack">
      <div className="panel hero">
        <h1>{isDataset ? "Pull a dataset off the Hub" : "Pull a model off the Hub"}</h1>
        <p className="muted">
          {isDataset
            ? "Download datasets from Hugging Face, Kaggle or GitHub into this machine's datasets folder."
            : "Pick the format your runtime wants — safetensors for vLLM, GGUF for llama.cpp — and download straight to this machine."}
        </p>
        <KindPicker value={kind} onChange={(next) => update({ kind: next })} />
        {isDataset && <SourcePicker value={source} onChange={(next) => update({ source: next })} />}
        <CloneBox kind={kind} source={source} />
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
            placeholder={isDataset ? `Search ${SOURCE_LABEL[source]} datasets…` : `Search ${KIND_LABEL[kind]} repos…`}
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
          {suggestions.map((s) => (
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
              <Link className="result" to={`/model/${model.id}?kind=${kind}${sourceQuery(kind, source)}`}>
                <div className="result-main">
                  <span className="result-id">{model.id}</span>
                  <span className="tag-row">
                    {model.pipeline_tag && <span className="tag">{model.pipeline_tag}</span>}
                    {model.gated && <span className="tag warn">gated</span>}
                  </span>
                </div>
                <div className="result-meta">
                  {model.downloads > 0 && <span>{formatCount(model.downloads)} downloads</span>}
                  <span>{source === "github" ? "★" : "♥"} {formatCount(model.likes)}</span>
                </div>
                {model.description && <p className="muted small">{model.description}</p>}
              </Link>
            </li>
          ))}
        </ul>
      </div>
    </section>
  );
}
