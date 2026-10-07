# lmdeck

A small LM Studio for [Omarchy](https://github.com/omacom/omarchy) on Apple Silicon. It is one native Rust binary (~6 MB, ~7 MB RSS idle) that gives you:

- **A TUI** for the whole loop: find models, download them, load and unload them, chat, change settings, and update the runtimes.
- **An Ollama-compatible server** on `127.0.0.1:11434`. It also serves the **OpenAI API** under `/v1`, so Open WebUI, opencode, crush, editors, MCP clients and anything else that speaks Ollama or OpenAI works unchanged.
- **Two backends, routed per model**:
  - **MLX** models run on the Apple GPU through [omarchy-mlx](https://github.com/joshuaswarren/omarchy-mlx) (`mlx_lm.server`, with a decode fast path, see below).
  - **GGUF** models run through **llama.cpp** built with Vulkan on Honeykrisp.
- **Runtime management**: migrate from the old `install.sh` setup to the Omarchy packages, update `omarchy-mlx`, and build or update llama.cpp.

```
lmdeck            # TUI
lmdeck serve      # the server (or: lmdeck service install)
```

## Install

```bash
./install.sh            # builds, installs ~/.local/bin/lmdeck, adds launcher + Omarchy menu rows
./install.sh --service  # …and enables the server as a systemd user service
```

The Omarchy menu gets **Apps › lmdeck** and **Setup › Local LLM Server**. The second one toggles the service and shows ✓ while it runs.

## Runtimes

| Runtime | How lmdeck gets it | Used for |
|---|---|---|
| omarchy-mlx | `omarchy-pkg-add omarchy-mlx omarchy-mlx-vulkan` (Omarchy repo) | MLX safetensors models on the Apple GPU |
| llama.cpp | built from source, `-DGGML_VULKAN=ON`, into `~/.local/share/lmdeck/llama.cpp` | GGUF models |

**Migrating from the script install.** If omarchy-mlx was installed with its old `curl … install.sh` (a venv under `~/.local/share/mlx-omarchy`), press `M` on the Dashboard or run `lmdeck migrate`. It does four things:

1. Installs the packages with Omarchy's package helper (`omarchy-pkg-add`, falling back to `omarchy-pkg-aur-add`).
2. Smoke-tests `/usr/bin/mlx-omarchy` on the GPU. If that fails, the legacy install stays.
3. Runs upstream's `mlx-omarchy-retire-legacy`: a dry run first, then you confirm. It removes the old venv, launchers and shims. Chat data and `~/.cache/huggingface` models are kept.
4. Refreshes the Omarchy launcher.

`lmdeck migrate --ane` also installs `omarchy-ane-dkms`. Skip it if your kernel already has `/dev/accel/accel0`.

**Updating.** `U` or `lmdeck update` runs `sudo pacman -Syu --needed omarchy-mlx omarchy-mlx-vulkan`. It is a full upgrade because Arch doesn't support partial upgrades. `B` or `lmdeck llama build` rebuilds llama.cpp from `llama.git_ref` (default `master`).

## Models

Everything lands in the standard Hugging Face hub cache (`~/.cache/huggingface/hub`), so `mlx_lm`, `huggingface-cli` and lmdeck share the same files.

| Name | Example |
|---|---|
| MLX repo | `mlx-community/Qwen3-4B-Instruct-2507-4bit` |
| GGUF repo + quant | `unsloth/Qwen3.5-4B-GGUF:Q4_K_M` (no tag → `download.gguf_default_quant`) |
| Loose GGUF in `models.extra_dirs` | `local/<file stem>` |
| Short forms | `qwen3-4b-instruct-2507-4bit`, `hf.co/org/repo:Q4_K_M`, aliases from `/api/copy` |

The downloader opens 6 parallel range connections per large file. It resumes after interruption (sidecar state file), checks every LFS file with SHA-256, and only then links it into the snapshot. Gated repos need a token in `~/.cache/huggingface/token` or `HF_TOKEN`.

The Discover tab has two sources. The first is the **omarchy-mlx curated catalog**, which shows which models the project has qualified on this GPU backend. The second is a live **Hugging Face search** (`g` switches between MLX and GGUF). Opening a repo lists its quants with sizes and whether each fits in RAM.

## Using it from other programs

```bash
# Ollama clients
OLLAMA_HOST=http://127.0.0.1:11434
# OpenAI clients
OPENAI_BASE_URL=http://127.0.0.1:11434/v1  OPENAI_API_KEY=lmdeck
```

Endpoints:

- Ollama: `/api/chat`, `/api/generate`, `/api/tags`, `/api/ps`, `/api/show`, `/api/pull`, `/api/delete`, `/api/copy`, `/api/embed`, `/api/embeddings`, `/api/version`.
- OpenAI: any `POST /v1/*` is proxied to the model's backend (`chat/completions`, `completions`, `embeddings`; llama.cpp also does `/v1/messages`). `GET /v1/models` lists local models.

The Ollama translation covers:

- streaming NDJSON
- `think` (and `thinking` in the output)
- tool calls, both directions
- images (as data URIs)
- `format` (`json` or a JSON schema)
- `options` (`temperature`, `top_p`, `top_k`, `min_p`, `seed`, `stop`, `num_predict`, `repeat_penalty`, …)
- `keep_alive`, including `0` (unload now) and `-1` (keep forever)
- an empty prompt loads the model; empty plus `keep_alive: 0` unloads it
- real timing fields (`eval_count`/`eval_duration`, …)

Control endpoints used by the TUI: `/lmdeck/status`, `/lmdeck/load`, `/lmdeck/unload`, `/lmdeck/pull`, `/lmdeck/cancel`, `/lmdeck/downloads`, `/lmdeck/reload`, `/lmdeck/logs`.

## Performance notes

- **MLX decode fast path** (`mlx.fast_path`, on by default). Stock `mlx_lm.server` 0.31 calls `logprobs[token].item()` on every token, even when nobody asked for logprobs. On omarchy-mlx that extra synchronous GPU round trip halves decode. Its batched generator is also about 40% slower per stream on this backend. lmdeck starts the server through a small launcher (`src/mlx_fast_server.py`) that only does that read when logprobs are requested, and routes requests through the single-stream path. Set `mlx.batching = true` if you serve several clients at once. Unknown mlx-lm versions run unpatched.

  Measured on an M1 Max (Qwen3-0.6B-4bit, 256 tokens, greedy, legacy omarchy-mlx wheel):

  | | decode tok/s |
  |---|---|
  | stock `mlx_lm.server` | 34–35 |
  | lmdeck fast path | 62–81 |
  | `mlx_lm.generate` (no server) | 62–77 |

- **One model resident by default** (`server.max_loaded = 1`). Unified memory makes a second big model a swap risk. The least-recently-used idle model is evicted when another is requested.
- **Memory admission**: a load is refused with a clear error when weights plus overhead plus KV estimate exceed `MemAvailable − server.mem_reserve_gib`.
- **Warm-up on load**: MLX models get a 1-token request before they are marked ready. That loads the weights and compiles the Vulkan pipelines, so the first real request doesn't pay for it.
- `server.uclamp_boost` runs backends under `uclampset -m 1024`. It keeps schedutil from parking the CPU thread that feeds the GPU, which matters on power-saver profiles. There's no difference on `asahi-performance`.
- `hooks.on_load` and `hooks.on_idle` can switch tuned profiles, e.g. `tuned-adm profile asahi-performance` and `tuned-adm profile asahi-balanced`.
- The daemon is a 2-worker tokio runtime, the proxy streams without buffering, and the TUI redraws only on events plus a 1 s status poll.

## Files

| Path | What |
|---|---|
| `~/.config/lmdeck/config.toml` | settings (all editable in the Settings tab) |
| `~/.local/state/lmdeck/logs/` | `lmdeck.log` plus one log per model backend |
| `~/.local/share/lmdeck/` | llama.cpp build, MLX launcher shim |
| `~/.config/systemd/user/lmdeck.service` | optional user service |

## Keys

| Tab | Keys |
|---|---|
| Dashboard | `s` start/stop server · `S` install service · `a` unload all · `M` migrate · `U` update MLX · `B` build llama.cpp · `D` launcher + menu · `C` clear finished downloads · `r` re-check versions |
| Models | `⏎` load · `u` unload · `c` chat in terminal · `x` per-model context · `d` delete · `y` copy name |
| Discover | `/` search · `g` MLX⇄GGUF · `c` catalog⇄search · `⏎` open repo / download · `x` cancel download |
| Settings | `⏎` edit · `o` open config in `$EDITOR` |
| Logs | `m` daemon⇄model log · `j/k` scroll · `G` end |

`1`–`5` or `Tab` switch tabs, and `q` quits. Colors come from the terminal palette, so the Omarchy theme applies.

## License

MIT
