export type ModelKind = "safetensors" | "gguf";

export interface ModelSummary {
  id: string;
  downloads: number;
  likes: number;
  pipeline_tag: string | null;
  last_modified: string | null;
  gated: boolean;
  tags: string[];
}

export interface RepoFile {
  path: string;
  size: number;
  relevant: boolean;
}

export interface RepoDetail {
  model: ModelSummary;
  files: RepoFile[];
  revision: string;
  kind: ModelKind;
  dest: string;
  /** Where the repo would land in each configured store; the first is the default. */
  stores: { name: string; dest: string }[];
}

export type JobStatus = "queued" | "running" | "completed" | "failed" | "canceled";
export type FileStatus = "pending" | "downloading" | "skipped" | "done" | "failed" | "canceled";

export interface JobFile {
  path: string;
  size: number;
  downloaded: number;
  status: FileStatus;
  error: string | null;
}

export interface Job {
  id: string;
  repo_id: string;
  revision: string;
  kind: ModelKind;
  store: string;
  dest: string;
  status: JobStatus;
  error: string | null;
  created_at: number;
  total_bytes: number;
  downloaded_bytes: number;
  files: JobFile[];
}

export interface LibraryEntry {
  store: string;
  repo_id: string;
  revision: string;
  kind: ModelKind;
  path: string;
  size_bytes: number;
  file_count: number;
  downloaded_at: number;
  serve_command: string;
  estimated_vram_bytes: number;
  files: string[];
}

export interface ServerConfig {
  data_dir: string;
  models_dir: string;
  stores: { name: string; root: string }[];
  parallel_files: number;
  has_hf_token: boolean;
}

export interface GpuInfo {
  index: number;
  name: string;
  vram_bytes: number;
  busy_with: string | null;
}

export type RecipeParams =
  | {
      runtime: "vllm";
      served_model_name: string;
      tensor_parallel_size: number;
      gpu_memory_utilization: number;
      max_model_len: number | null;
    }
  | {
      runtime: "llama_cpp";
      gguf_file: string;
      ctx_size: number;
      n_gpu_layers: number;
    };

export interface Recipe {
  image: string;
  gpu_index: number | null;
  host_port: number;
  extra_args: string;
  params: RecipeParams;
}

export interface RecipeResponse {
  recipe: Recipe;
  preview: string;
  files: string[];
}

export type ContainerState = "running" | "paused" | "exited" | string;

export interface RunningContainer {
  id: string;
  name: string;
  repo_id: string;
  kind: ModelKind;
  gpu_index: number | null;
  state: ContainerState;
  host_port: number | null;
}

export const KIND_LABEL: Record<ModelKind, string> = {
  safetensors: "safetensors · vLLM",
  gguf: "GGUF · llama.cpp",
};

export const KINDS: ModelKind[] = ["safetensors", "gguf"];

export function isKind(value: string | null): value is ModelKind {
  return value === "safetensors" || value === "gguf";
}
