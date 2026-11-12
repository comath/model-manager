# normalize_agent

Turns every finished dataset download into normalized parquet plus a README, by running an agent
in its own throwaway Incus container.

```
<datasets>/huggingface/naver-clova-ix/cord-v2/     raw download (model-manager)
<datasets>/normalized/huggingface/naver-clova-ix/cord-v2/
    README.md  *.parquet …  .normalize.json        agent output + manager's status record
<datasets>/normalized/.logs/<job>.log              per-run log (incus steps + agent output)
<datasets>/normalized/.work/<job>/                 staging while a job runs
```

## Flow

1. model-manager finishes a dataset download and POSTs `{source, repo_id, revision, path}` to
   `MODEL_MANAGER_DATASET_HOOK` (set automatically by the NixOS module).
2. `normalize-agent` checks `path` really is `<datasets>/<source>/<owner>/<name>` (no symlinks out,
   not `normalized/`), queues a job, and runs one at a time by default.
3. Per job it creates an **ephemeral** container from `image`, with exactly two disk devices:
   - `/data/raw` → the dataset folder, `readonly=true`
   - `/data/out` → a fresh empty staging folder, `shift=true` (writable by the container's root)

   The agent never sees other datasets, `normalized/`, or the previous output for this dataset.
4. It runs `command` in `/data/out` and waits (`timeout`, default 4 h), then deletes the container.
5. It checks the staging folder: `README.md` must exist, at least one `*.parquet` with valid
   `PAR1` magic, and no symlinks, hard links or special files. Modes are reset to 0644/0755
   (the container writes as host root, so setuid bits etc. must not survive) and ownership goes
   to the `models` account.
6. Only then does it swap the folder in for the previous normalized copy. A failed run leaves
   the previous copy untouched.

## Agent contract

What the image must provide; the manager doesn't care how the agent works inside.

| | |
| --- | --- |
| `RAW_DIR=/data/raw` | the download, read-only (includes model-manager's metadata sidecar) |
| `OUT_DIR=/data/out` | empty, write output here; it's also the working directory |
| `DATASET_SOURCE`, `DATASET_REPO_ID`, `DATASET_REVISION` | which dataset this is |
| `environment` / `forwardEnvironment` | e.g. `OPENAI_BASE_URL`, `OPENAI_MODEL`, `HF_TOKEN` |
| exit 0 | success; anything else fails the job |

Anything the agent needs to fetch (download scripts, pip packages) goes over the container's
network, which comes from the Incus `profiles`. Put ACLs there that block the LAN.

## API

| | |
| --- | --- |
| `POST /jobs` | `{source, path, repo_id?, revision?}` → `202` job (or `200` if already queued) |
| `GET /jobs` · `GET /jobs/{id}` | job status, newest first |
| `GET /healthz` | liveness |

It binds `127.0.0.1:8088` with no auth; only model-manager on the same host should reach it.

## NixOS

```nix
services.modelManager.normalizer = {
  enable = true;                       # also sets services.modelManager.datasetHook
  image = "local:normalize-agent";
  profiles = [ "normalize-agent" ];    # network + ACLs for the agent
  environment = {
    OPENAI_BASE_URL = "http://spark-1:8000/v1";
    OPENAI_MODEL = "gemma-4-31b";
  };
  environmentFile = config.sops.templates."normalize-agent.env".path;
  forwardEnvironment = [ "HF_TOKEN" ];
};
```

Needs `virtualisation.incus.enable`. `shift=true` needs idmapped mounts (Linux ≥ 5.12; ZFS ≥ 2.2).

## Running it by hand

```bash
cargo run --manifest-path normalize_agent/Cargo.toml -- --datasets-root /slow/models/datasets --image local:normalize-agent --owner models
```
