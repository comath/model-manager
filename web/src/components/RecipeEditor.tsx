import { useEffect, useState } from "react";
import { api, formatBytes } from "../api";
import type { GpuInfo, ModelKind, Recipe, RecipeParams } from "../types";

type VllmParams = Extract<RecipeParams, { runtime: "vllm" }>;
type LlamaCppParams = Extract<RecipeParams, { runtime: "llama_cpp" }>;

function VllmFields({
  params,
  onChange,
}: {
  params: VllmParams;
  onChange: (next: VllmParams) => void;
}) {
  return (
    <>
      <label>
        Served model name
        <input
          value={params.served_model_name}
          onChange={(e) => onChange({ ...params, served_model_name: e.target.value })}
        />
      </label>
      <label>
        Tensor parallel size
        <input
          type="number"
          min={1}
          value={params.tensor_parallel_size}
          onChange={(e) => onChange({ ...params, tensor_parallel_size: Number(e.target.value) })}
        />
      </label>
      <label>
        GPU memory utilization
        <input
          type="number"
          min={0}
          max={1}
          step={0.05}
          value={params.gpu_memory_utilization}
          onChange={(e) => onChange({ ...params, gpu_memory_utilization: Number(e.target.value) })}
        />
      </label>
      <label>
        Max model length (optional)
        <input
          type="number"
          value={params.max_model_len ?? ""}
          placeholder="model default"
          onChange={(e) =>
            onChange({ ...params, max_model_len: e.target.value === "" ? null : Number(e.target.value) })
          }
        />
      </label>
    </>
  );
}

function LlamaCppFields({
  params,
  files,
  onChange,
}: {
  params: LlamaCppParams;
  files: string[];
  onChange: (next: LlamaCppParams) => void;
}) {
  return (
    <>
      <label>
        GGUF file
        <select
          value={params.gguf_file}
          onChange={(e) => onChange({ ...params, gguf_file: e.target.value })}
        >
          {files.length === 0 && <option value="">(no .gguf files downloaded)</option>}
          {files.map((f) => (
            <option key={f} value={f}>
              {f}
            </option>
          ))}
        </select>
      </label>
      <label>
        Context size
        <input
          type="number"
          value={params.ctx_size}
          onChange={(e) => onChange({ ...params, ctx_size: Number(e.target.value) })}
        />
      </label>
      <label>
        GPU layers offloaded
        <input
          type="number"
          value={params.n_gpu_layers}
          onChange={(e) => onChange({ ...params, n_gpu_layers: Number(e.target.value) })}
        />
        <span className="field-hint">999 = offload everything</span>
      </label>
    </>
  );
}

export function RecipeEditor({
  kind,
  repoId,
  store,
  files,
  estimatedVramBytes,
  onLaunched,
}: {
  kind: ModelKind;
  repoId: string;
  store: string;
  files: string[];
  estimatedVramBytes: number;
  onLaunched?: () => void;
}) {
  const [recipe, setRecipe] = useState<Recipe | null>(null);
  const [preview, setPreview] = useState("");
  const [gpus, setGpus] = useState<GpuInfo[]>([]);
  const [busy, setBusy] = useState<"save" | "launch" | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [savedAt, setSavedAt] = useState<number | null>(null);

  useEffect(() => {
    api.recipe(kind, repoId, store).then((r) => {
      setRecipe(r.recipe);
      setPreview(r.preview);
    });
    api.gpus().then(setGpus).catch(() => undefined);
  }, [kind, repoId, store]);

  const ggufFiles = files.filter((f) => f.toLowerCase().endsWith(".gguf"));

  const update = (patch: Partial<Recipe>) => setRecipe((r) => (r ? { ...r, ...patch } : r));
  const setParams = (params: RecipeParams) => setRecipe((r) => (r ? { ...r, params } : r));

  const save = async () => {
    if (!recipe) return;
    setBusy("save");
    setError(null);
    try {
      const res = await api.saveRecipe(kind, repoId, store, recipe);
      setPreview(res.preview);
      setSavedAt(Date.now());
    } catch (err) {
      setError((err as Error).message);
    } finally {
      setBusy(null);
    }
  };

  const launch = async () => {
    if (!recipe) return;
    setBusy("launch");
    setError(null);
    try {
      await api.saveRecipe(kind, repoId, store, recipe);
      await api.launch(kind, repoId, store);
      onLaunched?.();
    } catch (err) {
      setError((err as Error).message);
    } finally {
      setBusy(null);
    }
  };

  if (!recipe) return <p className="muted small">Loading recipe…</p>;

  const gpuFits = (g: GpuInfo) => g.vram_bytes >= estimatedVramBytes;

  return (
    <div className="recipe">
      <div className="recipe-grid">
        <label>
          Image
          <input
            value={recipe.image}
            onChange={(e) => update({ image: e.target.value })}
          />
        </label>
        <label>
          Host port
          <input
            type="number"
            value={recipe.host_port}
            onChange={(e) => update({ host_port: Number(e.target.value) })}
          />
        </label>
        <label>
          GPU
          <select
            value={recipe.gpu_index ?? ""}
            onChange={(e) =>
              update({ gpu_index: e.target.value === "" ? null : Number(e.target.value) })
            }
          >
            <option value="">CPU (no GPU)</option>
            {gpus.map((g) => (
              <option key={g.index} value={g.index} disabled={!!g.busy_with && g.busy_with !== repoId}>
                GPU {g.index}: {g.name} ({formatBytes(g.vram_bytes)}
                {gpuFits(g) ? "" : " — too small"}
                {g.busy_with && g.busy_with !== repoId ? ` — busy: ${g.busy_with}` : ""})
              </option>
            ))}
          </select>
        </label>

        {recipe.params.runtime === "vllm" ? (
          <VllmFields params={recipe.params} onChange={setParams} />
        ) : (
          <LlamaCppFields params={recipe.params} files={ggufFiles} onChange={setParams} />
        )}

        <label className="span-2">
          Extra args
          <input
            value={recipe.extra_args}
            placeholder="--any-additional-flag value"
            onChange={(e) => update({ extra_args: e.target.value })}
          />
        </label>
      </div>

      <p className="muted small">
        Estimated VRAM for weights: <strong>{formatBytes(estimatedVramBytes)}</strong> (excludes
        KV-cache/activation overhead, which grows with context length).
      </p>

      <div className="command">
        <code>{preview}</code>
      </div>

      {error && <p className="error-text">{error}</p>}

      <div className="row-actions">
        <button type="button" className="button ghost" disabled={busy !== null} onClick={save}>
          {busy === "save" ? "Saving…" : savedAt ? "Saved ✓" : "Save recipe"}
        </button>
        <button type="button" className="button primary" disabled={busy !== null} onClick={launch}>
          {busy === "launch" ? "Launching…" : "Save & Launch"}
        </button>
      </div>
    </div>
  );
}
