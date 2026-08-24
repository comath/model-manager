# model-manager

A local model downloader: Rust (tokio + axum) API with a React Router frontend. Search the
HuggingFace Hub, pick the format your runtime wants — **safetensors for vLLM** or **GGUF for
llama.cpp** — and pull the files onto this machine with live progress.

## Quick start

```bash
nix develop            # rust toolchain + node + pnpm
```

Two terminals, both inside the dev shell:

```bash
cargo run                          # API on http://127.0.0.1:8087
cd web && pnpm install && pnpm dev # UI on http://127.0.0.1:5173 (proxies /api)
```

For a single-process setup, build the frontend once and let axum serve it:

```bash
cd web && pnpm build && cd .. && cargo run   # everything on http://127.0.0.1:8087
```

## What it does

- **Discover** — Hub search filtered to the chosen format, sorted by downloads.
- **Pick files** — the file list is pre-checked with exactly what the runtime needs
  (weights + tokenizer/config for vLLM, the `.gguf` you choose for llama.cpp); tick
  *show all files* for the rest.
- **Download** — several files at a time, resumable (`*.part` sidecars, HTTP `Range`),
  cancellable, with per-file progress streamed to the browser over SSE.
- **Library** — what is on disk, its size, an estimated VRAM footprint (weight bytes; excludes
  KV-cache/activation overhead), and the bare-metal command to serve it (`vllm serve <dir>` or
  `llama-server -m <file>`). Delete removes the directory.
- **Run in a container** — from a library entry, edit a per-model launch recipe (image, port, GPU,
  and the vLLM/llama.cpp flags that matter) and launch it as a Podman container. The **Running**
  page lists active containers with live logs and a stop button.

## Running models in containers

Each downloaded repo gets an editable recipe, stored as `docker-recipe.json` next to its
`model-manager.json`. Podman itself is the registry — every container we launch carries
`model-manager=1`/`model-manager.repo=…`/`model-manager.gpu=…` labels, so `podman ps` after a
restart still finds them; nothing about running state is tracked separately.

- **GPU discovery**: at startup the server runs `nvidia-smi` once to list the host's GPUs (index,
  name, VRAM). On a host with no NVIDIA driver this is just an empty list — everything else still
  works, minus the GPU dropdown.
- **One GPU, one model**: a recipe's GPU field picks a specific card via CDI
  (`--device nvidia.com/gpu=<index>`). Before launching, the server checks for another of our
  containers already claiming that index and refuses if so — so on a 3-GPU box you get at most 3
  concurrent models, one per card, without a separate counter.
- **Podman, not Docker**: the container binary is configurable
  (`MODEL_MANAGER_CONTAINER_RUNTIME`, default `podman`) but the whole flow — CDI GPU syntax, image
  entrypoints — is written and tested against Podman.
- CDI device passthrough needs `nvidia-ctk cdi generate` to have been run on the host once so
  `nvidia.com/gpu=<index>` resolves; without it, launches with a GPU set will fail with a clear
  CDI error.

## Layout on disk

```
~/.model-manager/models/
  safetensors/<owner>/<name>/…      # vLLM points at this directory
  gguf/<owner>/<name>/<quant>.gguf  # llama.cpp points at the file
```

Each model directory carries a `model-manager.json` sidecar; that is what the library scan reads.

## Configuration

| Variable | Default | Meaning |
| --- | --- | --- |
| `MODEL_MANAGER_DATA_DIR` | `~/.model-manager` | Root for downloaded models |
| `MODEL_MANAGER_BIND` | `127.0.0.1:8087` | Listen address |
| `MODEL_MANAGER_PARALLEL_FILES` | `2` | Files fetched concurrently per job |
| `MODEL_MANAGER_WEB_DIR` | `web/dist` | Built frontend to serve at `/` |
| `MODEL_MANAGER_CONTAINER_RUNTIME` | `podman` | Binary used to launch/stop/inspect model containers |
| `HF_TOKEN` | — | Required for gated/private repos |
| `RUST_LOG` | `model_manager=info` | Log filter |

## API

| Method | Path | |
| --- | --- | --- |
| `GET` | `/api/config` | data dir, token presence, concurrency |
| `GET` | `/api/search?q=&kind=&limit=` | Hub search (`kind` = `safetensors` \| `gguf`) |
| `GET` | `/api/repo/{owner}/{name}?kind=&revision=` | file tree with sizes and a relevance flag |
| `POST` | `/api/downloads` | `{repo_id, kind, revision?, files:[{path,size}]}` |
| `GET` | `/api/downloads` · `/api/downloads/{id}` | job state |
| `POST` | `/api/downloads/{id}/cancel` | cancel (partial files stay, and resume later) |
| `DELETE` | `/api/downloads/{id}` | drop a finished job from the list |
| `GET` | `/api/library` · `DELETE /api/library/{kind}/{owner}/{name}` | on-disk models |
| `GET` | `/api/gpus` | discovered GPUs, and which repo (if any) currently holds each one |
| `GET`/`PUT` | `/api/recipes/{kind}/{owner}/{name}` | load (or default) / save a launch recipe |
| `GET` | `/api/runtime` | running containers we launched |
| `POST` | `/api/runtime/launch` | `{kind, repo_id}` — launch using the saved (or default) recipe |
| `POST` | `/api/runtime/{name}/stop` · `GET /api/runtime/{name}/logs?tail=` | stop / tail logs |
| `GET` | `/api/events` | SSE: job progress + library invalidation |

## Notes

- The server binds to loopback and has no auth — it is a local tool, not a public service.
- Downloads have no overall timeout; 100 GB repos are expected to take a while.
- Re-running a download skips files already present at the expected size, so it doubles as a
  repair/`sync` for an interrupted repo.
