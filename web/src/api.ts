import type {
  GpuInfo,
  Job,
  LibraryEntry,
  ModelKind,
  ModelSummary,
  Recipe,
  RecipeResponse,
  RepoDetail,
  RunningContainer,
  ServerConfig,
  Source,
} from "./types";
import { sourceQuery } from "./types";

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(`/api${path}`, {
    ...init,
    headers: init?.body ? { "content-type": "application/json", ...init?.headers } : init?.headers,
  });
  if (!res.ok) {
    const detail = await res
      .json()
      .then((body: { error?: string }) => body.error)
      .catch(() => null);
    throw new Error(detail ?? `${res.status} ${res.statusText}`);
  }
  return res.json() as Promise<T>;
}

export const api = {
  config: () => request<ServerConfig>("/config"),

  search: (q: string, kind: ModelKind, source: Source, signal?: AbortSignal) =>
    request<{ models: ModelSummary[] }>(
      `/search?kind=${kind}${sourceQuery(kind, source)}&limit=40&q=${encodeURIComponent(q)}`,
      { signal },
    ).then((r) => r.models),

  repo: (repoId: string, kind: ModelKind, source: Source, revision?: string) =>
    request<RepoDetail>(
      `/repo/${repoId}?kind=${kind}${sourceQuery(kind, source)}${revision ? `&revision=${encodeURIComponent(revision)}` : ""}`,
    ),

  jobs: () => request<{ jobs: Job[] }>("/downloads").then((r) => r.jobs),

  startDownload: (body: {
    repo_id: string;
    kind: ModelKind;
    source?: Source;
    revision?: string;
    store?: string;
    files: { path: string; size: number }[];
  }) =>
    request<{ job: Job }>("/downloads", { method: "POST", body: JSON.stringify(body) }).then(
      (r) => r.job,
    ),

  /** Resolve `owner/name` or a repo URL and download everything in it. */
  clone: (body: { repo: string; kind?: ModelKind; source?: Source; revision?: string; store?: string }) =>
    request<{ job: Job }>("/clone", { method: "POST", body: JSON.stringify(body) }).then(
      (r) => r.job,
    ),

  cancel: (id: string) => request<{ job: Job }>(`/downloads/${id}/cancel`, { method: "POST" }),

  forget: (id: string) => request<{ ok: true }>(`/downloads/${id}`, { method: "DELETE" }),

  library: () => request<{ models: LibraryEntry[] }>("/library").then((r) => r.models),

  remove: (kind: ModelKind, repoId: string, store: string, source: Source) =>
    request<{ ok: true }>(`/library/${kind}/${repoId}${storeQuery(store)}${sourceQuery(kind, source)}`, {
      method: "DELETE",
    }),

  gpus: () => request<{ gpus: GpuInfo[] }>("/gpus").then((r) => r.gpus),

  recipe: (kind: ModelKind, repoId: string, store: string) =>
    request<RecipeResponse>(`/recipes/${kind}/${repoId}${storeQuery(store)}`),

  saveRecipe: (kind: ModelKind, repoId: string, store: string, recipe: Recipe) =>
    request<RecipeResponse>(`/recipes/${kind}/${repoId}${storeQuery(store)}`, {
      method: "PUT",
      body: JSON.stringify(recipe),
    }),

  running: () => request<{ containers: RunningContainer[] }>("/runtime").then((r) => r.containers),

  launch: (kind: ModelKind, repoId: string, store: string) =>
    request<{ container: RunningContainer }>("/runtime/launch", {
      method: "POST",
      body: JSON.stringify({ kind, repo_id: repoId, store }),
    }).then((r) => r.container),

  stopContainer: (name: string) =>
    request<{ ok: true }>(`/runtime/${name}/stop`, { method: "POST" }),

  containerLogs: (name: string, tail = 200) =>
    request<{ logs: string }>(`/runtime/${name}/logs?tail=${tail}`).then((r) => r.logs),
};

function storeQuery(store: string): string {
  return `?store=${encodeURIComponent(store)}`;
}

export function formatBytes(bytes: number): string {
  if (!bytes) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  const exp = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1);
  const value = bytes / 1024 ** exp;
  return `${value.toFixed(value >= 100 || exp === 0 ? 0 : 1)} ${units[exp]}`;
}

export function formatCount(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`;
  return String(n);
}
