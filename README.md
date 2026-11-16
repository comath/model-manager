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
cargo run -p model-manager           # API on http://127.0.0.1:8087
cd web && pnpm install && pnpm dev # UI on http://127.0.0.1:5173 (proxies /api)
```

For a single-process setup, build the frontend once and let axum serve it:

```bash
cd web && pnpm build && cd .. && cargo run -p model-manager   # everything on http://127.0.0.1:8087
```

## NixOS

The flake exports `packages.<system>.default` (server; the built frontend is `passthru.web`) and
`nixosModules.default`, which runs it as a hardened systemd service under a `models` system
account that owns the store roots:

```nix
inputs.model-manager.url = "github:comath/model-manager";
inputs.model-manager.inputs.nixpkgs.follows = "nixpkgs";

# in a nixosSystem:
imports = [ inputs.model-manager.nixosModules.default ];
services.modelManager = {
  enable = true;
  bind = "0.0.0.0:8087";               # default 127.0.0.1:8087; there is no auth
  stores = [
    { name = "fast"; path = "/fast/models"; }
    { name = "slow"; path = "/slow/models"; }
  ];
  environmentFile = "/run/secrets/model-manager.env";   # optional: HF_TOKEN=…
};
```

Other options, each mapped to the env var in the Configuration table below:

| Option | Env var |
| --- | --- |
| `defaultStore` | `MODEL_MANAGER_DEFAULT_STORE` (unset: the store named `slow`, else the first) |
| `datasetsDir` | `MODEL_MANAGER_DATASETS_DIR` |
| `parallelFiles`, `containerRuntime` | `MODEL_MANAGER_PARALLEL_FILES`, `MODEL_MANAGER_CONTAINER_RUNTIME` |
| `tls.cert`, `tls.key`, `tls.clientCa` | `MODEL_MANAGER_TLS_CERT`, `_TLS_KEY`, `_CLIENT_CA` (the last enables mTLS) |
| `tailscaleBind` | `MODEL_MANAGER_TAILSCALE_BIND`: a second, unauthenticated plain-HTTP listener |
| `normalizer.*` | the optional [dataset normalizer](manage_agent/README.md); off unless `normalizer.enable` |

### Publishing on a tailnet

Instead of `tls`, let Tailscale terminate HTTPS and keep the app on loopback:

```nix
services.tailscale.enable = true;
services.modelManager = {
  bind = "127.0.0.1:8087";
  tailscaleServe.enable = true;        # https://model-manager.<tailnet>.ts.net
  # tailscaleServe.service = "svc:model-manager"; httpsPort = 443; target = "http://<bind>";
};
```

This adds a `model-manager-tailscale-serve` oneshot that runs
`tailscale serve --bg --service=svc:model-manager --https=443 http://127.0.0.1:8087` (idempotent;
retried until tailscaled is logged in). One-time setup in the Tailscale admin console: define the
Service `svc:model-manager` (port 443), tag this host and allow the tag to host it, and enable
MagicDNS/HTTPS. The app has **no auth**: anyone who can reach the host can delete models and
launch containers, so restrict access with an ACL. Remove it with
`tailscale serve clear svc:model-manager`. Not combinable with `tls.*`.

When `web/pnpm-lock.yaml` changes, update the `hash` in `nix/package.nix` (set it to
`lib.fakeHash`, run `nix build`, copy the hash from the error).

## What it does

- **Discover** — Hub search filtered to the chosen format, sorted by downloads.
- **Pick files** — the file list is pre-checked with exactly what the runtime needs
  (weights + tokenizer/config for vLLM, the `.gguf` you choose for llama.cpp); tick
  *show all files* for the rest.
- **Download** — several files at a time, resumable (`*.part` sidecars, HTTP `Range`),
  cancellable, with per-file progress streamed to the browser over SSE.
- **Bulk clone** — the clone box on the search page takes one repo per line (`owner/name` or a
  Hugging Face / Kaggle / GitHub URL; blank lines and `#` comments are ignored), or load a `.txt`
  of them. Each line becomes a download job; lines that fail stay in the box with the error.
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

### Several stores (e.g. fast and slow disks)

Set `MODEL_MANAGER_STORES=fast=/fast/models,slow=/slow/models` to download into named roots
instead of `<data>/models`. Each store gets the same `<kind>/<owner>/<name>` layout. The model
page shows a picker (the last choice is remembered in the browser), the store named `slow` is the
default (override with `MODEL_MANAGER_DEFAULT_STORE=<name>`; with no match, the first store is used), and the library lists every store, tagging each entry with where it lives. The same repo
can sit in more than one store; delete, recipes and launch act on the copy you pick.

### Datasets

Pick the **dataset** tab on the discover page, then a source: **Hugging Face**, **Kaggle** or
**GitHub**. Files land in a separate datasets folder (`MODEL_MANAGER_DATASETS_DIR`, default
`<data>/datasets`) as `<source>/<owner>/<name>`, independent of the model stores. Datasets show up
in the library alongside models (without a container recipe).

- **Hugging Face** — dataset repos; `HF_TOKEN` for gated ones.
- **Kaggle** — public datasets work without credentials. For private ones set `KAGGLE_USERNAME` +
  `KAGGLE_KEY` (or `KAGGLE_API_TOKEN`); `~/.kaggle/kaggle.json` is also read. The revision is the
  dataset version number.
- **GitHub** — any repo, treated as a dataset (search is by stars). Files are fetched from
  `raw.githubusercontent.com` at the repo's default branch. Set `GITHUB_TOKEN` (or `GH_TOKEN`) for
  private repos and to lift the 10 requests/minute unauthenticated search limit.

With the NixOS module, put these in the `environmentFile`.

Set `MODEL_MANAGER_DATASET_HOOK` to have each finished dataset download POSTed somewhere; the
[manage-agent](manage_agent/README.md) and the [normalize agent](normalize_agent/README.md) uses it to turn datasets into parquet under
`<datasets>/normalized/`.

## Configuration

| Variable | Default | Meaning |
| --- | --- | --- |
| `MODEL_MANAGER_DATA_DIR` | `~/.model-manager` | Root for downloaded models |
| `MODEL_MANAGER_DATASETS_DIR` | `<data>/datasets` | Absolute path where datasets are downloaded (`<source>/<owner>/<name>`) |
| `MODEL_MANAGER_DATASET_HOOK` | — | URL POSTed `{source, repo_id, revision, path}` after each completed dataset download |
| `KAGGLE_USERNAME` + `KAGGLE_KEY`, or `KAGGLE_API_TOKEN` | — | Kaggle credentials (optional for public datasets) |
| `GITHUB_TOKEN` / `GH_TOKEN` | — | GitHub token (optional) |
| `MODEL_MANAGER_STORES` | — | `name=/abs/path,…` download targets; replaces `<data>/models` |
| `MODEL_MANAGER_DEFAULT_STORE` | `slow` | Store used when none is chosen; falls back to the first store if no store has this name |
| `MODEL_MANAGER_TLS_CERT` / `_TLS_KEY` / `_CLIENT_CA` | — | PEM paths for TLS; adding a client CA enables mTLS |
| `MODEL_MANAGER_TAILSCALE_BIND` | — | Extra unauthenticated plain-HTTP listen address (alias `MODEL_MANAGER_HTTP_BIND`) |
| `MODEL_MANAGER_BIND` | `127.0.0.1:8087` | Listen address |
| `MODEL_MANAGER_PARALLEL_FILES` | `2` | Files fetched concurrently per job |
| `MODEL_MANAGER_WEB_DIR` | `web/dist` | Built frontend to serve at `/` |
| `MODEL_MANAGER_CONTAINER_RUNTIME` | `podman` | Binary used to launch/stop/inspect model containers |
| `HF_TOKEN` | — | Required for gated/private repos |
| `RUST_LOG` | `model_manager=info` | Log filter |

## API

| Method | Path | |
| --- | --- | --- |
| `GET` | `/api/config` | data dir, stores, token presence, concurrency |
| `GET` | `/api/search?q=&kind=&limit=` | Hub search (`kind` = `safetensors` \| `gguf`) |
| `GET` | `/api/repo/{owner}/{name}?kind=&revision=` | file tree with sizes and a relevance flag |
| `POST` | `/api/downloads` | `{repo_id, kind, revision?, store?, files:[{path,size}]}` |
| `GET` | `/api/downloads` · `/api/downloads/{id}` | job state |
| `POST` | `/api/downloads/{id}/cancel` | cancel (partial files stay, and resume later) |
| `DELETE` | `/api/downloads/{id}` | drop a finished job from the list |
| `GET` | `/api/library` · `DELETE /api/library/{kind}/{owner}/{name}?store=` | on-disk models |
| `GET` | `/api/gpus` | discovered GPUs, and which repo (if any) currently holds each one |
| `GET`/`PUT` | `/api/recipes/{kind}/{owner}/{name}?store=` | load (or default) / save a launch recipe |
| `GET` | `/api/runtime` | running containers we launched |
| `POST` | `/api/runtime/launch` | `{kind, repo_id, store?}` — launch using the saved (or default) recipe |
| `POST` | `/api/runtime/{name}/stop` · `GET /api/runtime/{name}/logs?tail=` | stop / tail logs |
| `GET` | `/api/events` | SSE: job progress + library invalidation |

## Notes

- The server binds to loopback and has no auth — it is a local tool, not a public service.
- Downloads have no overall timeout; 100 GB repos are expected to take a while.
- Re-running a download skips files already present at the expected size, so it doubles as a
  repair/`sync` for an interrupted repo.
