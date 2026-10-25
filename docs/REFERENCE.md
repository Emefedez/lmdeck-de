# Reference

What lmdeck does in detail. Setup is in [SETUP.md](SETUP.md); measurements are in [RESULTS.md](RESULTS.md).


## The server

```bash
# Ollama clients
OLLAMA_HOST=http://127.0.0.1:11434
# OpenAI clients
OPENAI_BASE_URL=http://127.0.0.1:11434/v1  OPENAI_API_KEY=lmdeck
```

Endpoints:

- Ollama: `/api/chat`, `/api/generate`, `/api/tags`, `/api/ps`, `/api/show`, `/api/pull`, `/api/delete`, `/api/copy`, `/api/embed`, `/api/embeddings`, `/api/version`.
- OpenAI: any `POST /v1/*` is proxied to the model's backend (`chat/completions`, `completions`, `embeddings`). `GET /v1/models` lists local models.
- Anthropic: `/v1/messages` and `/v1/messages/count_tokens`, translated onto the backend's chat API (streaming, tools, thinking), on every backend.
- OpenAI Responses: `/v1/responses` (what Codex speaks), translated the same way.

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

Control endpoints used by the TUI and the bar panel:

- `/lmdeck/status`, plus `/lmdeck/watch?since=<event>` (long-poll). Each `running` entry has `context`: `limit` (the window lmdeck serves), `used` (the request in flight, its prompt plus the reply so far where the backend's log tells, else the last request's prompt + reply), `peak` since the model loaded, `live`, and `last` (`prompt`, `completion`, `ago_secs`). It counts one request: a client that resends its conversation every turn (all chat APIs do) shows that conversation's length. `work` shows the prompt being read (MLX, llama.cpp and Strata logs) and, on Strata, the reply's tokens so far.
- `/lmdeck/load`, `/lmdeck/unload`, `/lmdeck/restart`
- `/lmdeck/pull`, `/lmdeck/cancel`, `/lmdeck/downloads`
- `/lmdeck/config` (GET lists all settings; POST `{key, value}`), `/lmdeck/reload`
- `/lmdeck/search?q=&kind=mlx|gguf`, `/lmdeck/repo?id=`
- `/lmdeck/models`, `/lmdeck/logs`
- `/lmdeck/plan?model=` (the memory planner's decision, without loading), `/lmdeck/override` (per-model settings), `/lmdeck/compat`

Token usage is counted for both Ollama and OpenAI requests. It lives in memory and is flushed at most every 30 s, and only when it changed, to `~/.local/state/lmdeck/usage.json` and the Omarchy agents record.

## Coding agents

Like `ollama launch`, this runs an agent against a local model. Settings are passed through the process's environment or command-line flags, so the agent's own config files are never rewritten:

```bash
lmdeck launch claude   -m mlx-community/Qwen3-4B-Instruct-2507-4bit   # Claude Code via /v1/messages
lmdeck launch codex    -m <model>        # codex --oss against lmdeck's Ollama API
lmdeck launch opencode -m <model>        # OPENCODE_CONFIG_CONTENT with an "lmdeck" provider
lmdeck launch t3       -m <model>        # T3 Code: adds an "lmdeck (local)" provider, opens t1code
lmdeck launch t3 --no-open               # only (re)write the T3 provider; T3 sees it on next start
```

- **Claude Code** gets `ANTHROPIC_BASE_URL`, a dummy token, and every model slot (opus, sonnet, haiku, subagent) pointed at the local model. lmdeck serves the **Anthropic Messages API** (`/v1/messages`, plus `count_tokens`) by translating to the backend's chat API, with streaming, tool use and thinking.
- Requests for `claude-*` names that don't exist locally are answered by the loaded model.
- **T3 Code**: the provider instance uses T3's stock Claude driver plus those environment overrides, and lists your local models as custom models. `settings.json` is backed up first.
- Agent system prompts are large (Claude Code's is about 21k tokens), so the first turn spends most of its time in prefill. That took about 2.5 minutes for a 4B model on an M1 Max. MLX prompt caching makes later turns with the same prefix much faster.

### Prompt size for local models

Claude Code sends every tool it knows, MCP tools included. With a few MCP servers that is easily 140k+ tokens, which at ~60 tokens/s of prefill on a 27B model means an hour before the first answer (and an out-of-memory crash on the way). `server.local_tools` decides what a local model gets on `/v1/messages`:

| Mode | What the model sees | Claude Code request seen here |
|---|---|---|
| `lean` (default) | No MCP tools; essential Claude Code tools only, descriptions cut to their first paragraph, parameter docs to one line. Schemas are untouched, so tool calls work as before. | ~148k → ~6.6k tokens, first answer in ~100 s on Qwen3.8-27B |
| `groups` | `lean` plus two meta tools. `tool_group` lists your MCP servers (split into subgroups like `rea/inspect` when a server has many tools); lmdeck answers it itself with that section of `~/.config/lmdeck/tool-groups/<server>.md`. `call_tool(name, arguments)` is rewritten into the real MCP tool call for Claude Code. The group files are generated from the tool list and can be edited; delete the `lmdeck:auto` line to keep your edits. | MCP tools available at ~6.6k tokens; a hypruse call end to end in ~2 min |
| `core` | Everything except MCP tools. | ~17k tokens |
| `all` | Everything. | as sent |

`server.local_prompt` (default `lean`) also trims Claude Code's prompt for local models: MCP server instructions go away (in `groups` mode they come with that server's group), so do the notes about Claude models, and the memory paragraph, which names a per-project path, moves out of the system prompt so all projects share one cached system prompt. Claude Code's prompt went from ~8.1k to ~4.5k tokens, and a new conversation in a new project from ~104 s to ~25 s on Qwen3.8-27B (~64 s the very first time a system prompt is seen).

Per-tool rules go on top of the mode, for Claude Code, Codex and VS Code Copilot alike. `server.tool_deny` lists tool-name globs a local model never gets (tool groups included). `server.tool_allow` lists globs it always gets, even when the mode would drop them. An allowed MCP tool is then sent directly instead of through a group, and allow wins over deny. For example, `tool_deny = ["mcp__rea__*", "Workflow"]` with `tool_allow = ["mcp__rea__demangle_swift"]` keeps one rea tool. Names are Claude Code's (`Bash`, `mcp__<server>__<tool>`). VS Code's `mcp_<server>_<tool>` and Codex's namespaced tools are matched under that same `mcp__<server>__<tool>` form.

`lmdeck tools` lists the tools each client last sent and what happens to each (`sent`, `sent (short)`, `group`, `dropped`, `denied`, `allowed`). The lists are kept in `~/.local/state/lmdeck/tools-seen.json`. `lmdeck tools allow|deny|default <glob>` edits the rules. `lmdeck tools scan` records Claude Code's list without loading a model: it sends Claude Code's first request to lmdeck with an unknown model name, so the request ends with a 404. `lmdeck config [key] [value]` reads or sets any setting. Both commands reload the running server. If `ctx-trim` (a separate TUI for trimming agent tools and skills) is on your PATH, `t` on the Settings screen opens it on its lmdeck tab.

`server.local_tools`, `server.local_prompt`, `server.tool_allow`, `server.tool_deny` and `mlx.prefix_checkpoints` (on by default) are in Settings, and the first two are also in the bar panel under **Coding agents**.

Every `/v1/messages` request logs the breakdown (system, messages, tools before and after) in `lmdeck.log`. In `groups` mode text and thinking stream as usual; only tool calls wait until the end of the round, so lmdeck can answer `tool_group` itself.

**Decode attention.** omarchy-mlx's fused attention kernel streams only ~5 GB/s of cache when one new token attends a long context, so generation slowed from 12 to 4.8 tok/s between 1k and 12k tokens on Qwen3.8-27B. lmdeck's shim routes single-token decode over 1k+ tokens through two matmuls and a softmax with the GQA heads folded together (`mlx.decode_attention = composed`, the default): 4k context 9.3 → 11.3 tok/s, 12k 4.8 → 8.5 tok/s. Teacher-forced against the fused kernel: top-1 63/64, median KL 1.3e-4 (rounding noise). Prompt reading keeps the fused kernel, which is as fast there.

**Quantized attention cache.** `mlx.kv_bits` 8 or 4 now works: mlx_lm's quantized attention handed omarchy-mlx 5-D broadcast weights its kernel refuses, so the shim folds the GQA repeat into the query rows instead (no copies). Each load checks the result numerically against f16 and falls back if it's wrong: 4-bit computes wrong results on omarchy-mlx 0.7.28 (fixed in 0.7.31). Measured on Qwen3.8-27B at 12k tokens: 8-bit cuts the cache from 0.93 to 0.56 GiB with the same answers but ~10–15% slower generation; 4-bit to 0.37 GiB but answers drift within a few dozen tokens. Use it only when memory, not speed, is the limit.

**Prefix checkpoints.** Hybrid models such as Qwen3.5/3.8 keep part of their state in a cache that can't be rewound, so mlx_lm's single-request path (which caches prompt + answer) never reused anything and every agent turn re-read the whole prompt. lmdeck's shim now saves a checkpoint at the start of each answer, so the next turn (or the next `tool_group` round) only processes what is new: a follow-up turn went from ~99 s to ~13 s. A second checkpoint right after the system prompt (Claude Code's instructions plus tools, the same in every conversation) is also saved to `~/.cache/lmdeck/prompt-cache/` (~0.4 GB each, the 6 most recent per model and runtime), so new conversations and the first prompt after a restart or reload skip it too: a new Claude Code conversation after a restart went from ~104 s to ~50 s on Qwen3.8-27B.

### Codex and the Responses API

`POST /v1/responses` (what Codex speaks) is translated onto the backend's Chat Completions, on MLX and GGUF alike. Text and reasoning stream; tool calls come back as whole items. Codex's `apply_patch` (a free-form "custom" tool) is offered to the model as a function with one `input` string. `previous_response_id` works for the last 32 responses (kept in memory). As on `/v1/messages`, `server.local_tools` other than `all` drops the MCP tools: Codex sends each MCP server as a `namespace`, and with 9 servers that was 284 tools, ~83k tokens, against ~8k without them. lmdeck reports itself as Ollama 0.14.0, since Codex 0.16x refuses older versions. `lmdeck launch codex` on Qwen3.5-4B: a shell tool call and the answer, first turn 23 s, follow-up 3 s.

### VS Code Copilot

Copilot's chat requests (`/v1/chat/completions`, recognised by their user agent) get the same `server.local_tools`
treatment: in `lean` and `core` MCP tools (`mcp_<server>_<tool>`) are dropped; in `groups` they are replaced by
`tool_group` (answered by lmdeck from `~/.config/lmdeck/tool-groups/`) and `call_tool` (handed to VS Code as the real
MCP tool call), with text streamed as it comes and only tool calls held to the end of the round. In `lean`/`groups`
each tool description is cut to its first paragraph and each parameter doc to one line. The server log records the request's
size before and after.

## Context guard

Every chat request (`/api/chat`, `/api/generate`, `/v1/chat/completions`, `/v1/messages`, `/v1/responses`) is checked against the window lmdeck advertises for the model: the per-model context (Models → `e`), else `mlx.max_context` / `llama.ctx_size`, never above the model's own maximum. For Strata the window is the run config's `--max-context`; a context set in Models → `e` is passed to the engine
instead, in a copy of the run config (the file itself is not changed). A prompt over the window is refused at once with a clear error ("prompt is too long" on `/v1/messages`, which Claude Code offers to `/compact`; `context_length_exceeded` on the OpenAI paths), and `max_tokens` is trimmed to the room left. The size is an estimate: ~3.8 characters per token, an inline image counted as ~1k tokens. Measured with Qwen3.5's tokenizer on a Codex request, text runs 4.3 characters per token and tool JSON 4.0. An MLX backend has no limit of its own: before this guard, an 82k-token Codex prompt on a 4B model filled the whole GPU heap (19 GiB) while it was read.

**MLX buffer cache.** MLX keeps freed GPU buffers for reuse, by default up to its whole memory limit, so one long prompt's temporaries stayed allocated until the model unloaded. `mlx.cache_limit_gib` (default 1) caps that cache. Prompt caches are separate (`mlx.prompt_cache_gib`). The MLX log prints `MLX memory … in use, … cached, … peak` whenever the numbers change. A Codex run on Qwen3.5-4B ended at 3.45 GiB in use, 0.83 cached, 3.71 peak.

## System prompts

lmdeck can add a system prompt of its own to every chat request: Ollama `/api/chat` and `/api/generate`, OpenAI `/v1/chat/completions` and Anthropic `/v1/messages`. The prompts are plain files, so you can see and edit them all in one editor (`code ~/.config/lmdeck/prompts`):

| File | Applies to |
|---|---|
| `~/.config/lmdeck/prompts/mlx.md`, `llama.md`, `strata.md` | every MLX / GGUF / Strata model |
| `~/.config/lmdeck/prompts/models/<model>.md` (`/` in the name becomes `--`) | that model, instead of its backend's. An empty file means no prompt for that model |

- In the TUI, Models → `e` (Model settings) → **System prompt** opens the model's file in your editor. A model without its own file gets one filled in with the backend's prompt, so you see what applies now; left unchanged, it is removed again. `x` on that row removes the model's file, so it uses the backend's prompt again. The Details pane says which one applies.
- In Settings, Enter on **System prompts → mlx/llama/strata.system_prompt** opens the backend file. The bar panel shows and edits them as one line, with `\n` for newlines.
- The editor is `ui.editor` (Settings → Integration), else `$VISUAL`, `$EDITOR`, then `nvim`. VS Code (`code`, `codium`, `cursor`) gets `--wait`, so lmdeck comes back when you close the tab.
- **Mode** (`mlx.system_prompt_mode`, `llama.system_prompt_mode`, or `overrides."<model>".system_prompt_mode`): `prepend` (default) puts the prompt before the client's system prompt, `replace` drops the client's, `fallback` is used only when the client sends none.
- `{model}`, `{backend}` and `{date}` (UTC) are filled in. Files are read on every request, so an edit applies to the next request without reloading the model.

## Memory planning and performance

- **MLX decode fast path** (`mlx.fast_path`, on by default). Stock `mlx_lm.server` 0.31 calls `logprobs[token].item()` on every token, even when nobody asked for logprobs. On omarchy-mlx that extra synchronous GPU round trip halves decode. Its batched generator is also about 40% slower per stream on this backend. lmdeck starts the server through a small launcher (`src/mlx_fast_server.py`) that only does that read when logprobs are requested, and routes requests through the single-stream path. Set `mlx.batching = true` if you serve several clients at once. Unknown mlx-lm versions run unpatched.

  Measured on an M1 Max (Qwen3-0.6B-4bit, 256 tokens, greedy, legacy omarchy-mlx wheel):

  | | decode tok/s |
  |---|---|
  | stock `mlx_lm.server` | 34–35 |
  | lmdeck fast path | 62–81 |
  | `mlx_lm.generate` (no server) | 62–77 |

- **Cache settings**:
  - `mlx.prompt_cache_size` / `mlx.prompt_cache_gib` set mlx_lm's KV prompt cache, reused across requests.
  - `llama.cache_type_k`/`_v` take `q8_0` to halve KV memory, or `q4_0`.
  - `llama.cache_reuse` reuses KV for shifted prompts; it defaults to 256 and helps agent loops.
  - Changing any of these marks loaded models stale. Reload from the panel, or from Home in the TUI.
- **GPU memory guard**: Asahi gives the GPU about half of RAM (Honeykrisp heap). Overcommitting it can hang the whole machine, so every GPU load is planned first (`/lmdeck/plan?model=` shows the decision without loading anything):
  - `server.gpu_heap = auto` (the default) raises `HK_SYSMEM` for that one backend process only when the model needs more. The heap is set to the whole safe cap, free RAM minus `server.mem_reserve_gib` (desktop) and `server.vm_reserve_gib` while a muvm VM runs, since it's a ceiling and long prompts need the room for their KV cache. It never applies globally and ends when the model unloads.
  - `stock` never raises the heap. A number sets a fixed size in GiB.
  - A load that doesn't fit is refused with an explanation and a recommended model that does fit.
  - If a killed backend hasn't exited (for example a hung GPU), further loads are blocked so they can't stack another model's memory on top.
  - MLX needs are planned exactly: weights from the safetensors headers, attention cache from the model's config.json for 32k tokens (only the full-attention layers count, so hybrid models such as Qwen3.5/3.6/3.8 need little), plus ~1.1 GiB for runtime and prompt-cache checkpoints and a 0.5 GiB margin. If that doesn't fit, the planner retries for 8k tokens before refusing. On a 32 GB Mac this admits ~19 GiB models such as Qwen3.6-35B-A3B-4bit (≈20.3 GiB at 8k, ≈20.8 GiB at 32k) when about 23 GiB is free; use the memory popup's **Make room** first. The prompt-cache budget is capped to what the heap leaves free, and the Discover fit marker counts a raised heap as "tight" rather than "too big".
  - MLX weights are sized from the safetensors headers, rounded the way the selected runtime's allocator rounds GPU buffers (pages; a power of two for mlx-omarchy dev builds of 2026-10-05 to 2026-10-09; 8 classes per power of two from v0.7.32).
  - GGUF models that don't fit whole are loaded with only as many layers on the GPU as fit (`llama.auto_offload`, layer count read from the GGUF). The rest run on the CPU from the memory-mapped file. A per-model `gpu_layers` override wins over both.
  - If an MLX backend's generator thread dies while loading (for example Vulkan out of memory) the load fails right away with that error instead of waiting for the timeout.
- **Making room**: when a load is refused for memory, the TUI opens a memory popup. It shows what the planner said, opt-in actions (close the muvm VM, drop Hyprland blur/shadows/animations/transparency, disable optional Omarchy shell plugins and restore them later, stop tmux) and your processes grouped by name, with VS Code extensions and node/bun scripts on their own rows. `Enter` quits a row (SIGTERM), `X` kills it, `l` retries the load. `p` (make room automatically) goes least disruptive first: lower reserves, Hyprland effects, shell plugins, then the VM, re-checking the plan after each step, and loads the model once it fits. It never kills other apps. **Free memory** (`f`) on Home opens the process list on its own; **Make room** (`m`) on Models opens it for the selected model. The desktop reserve, VM reserve and GPU heap are sliders in Settings → Memory (`←`/`→`).
- **Long-prompt memory**: `mlx.prefill_step_size` (default 512) bounds prefill memory, and the MLX prompt cache is capped by size (default: a quarter of the GPU heap).
- **Compatibility**: lmdeck probes which MLX quantization modes the installed omarchy-mlx can run on the GPU. On 0.7.28 that's affine only; nvfp4, mxfp4 and mxfp8 fail. Combined with the omarchy-mlx catalog, every model gets a marker: ✓ qualified, ? untested, ✗ won't run. Each ✗ comes with a recommended alternative, and the per-model **Run on CPU (discouraged)** option is there as a last resort.
- **One model resident by default** (`server.max_loaded = 1`). Unified memory makes a second big model a swap risk. The least-recently-used idle model is evicted when another is requested.
- **Memory admission**: a load is refused with a clear error when weights plus overhead plus KV estimate exceed `MemAvailable − server.mem_reserve_gib`.
- **Load progress**: `/lmdeck/status` → `load` reports the phase (starting, loading weights, warming up), a percentage, bytes uploaded and an ETA. Weights are measured as GPU shmem growth against the model's size; warm-up uses the backend's prefill counter. The TUI, the bar panel and `lmdeck load` all show it.
- **Warm-up on load**: MLX models get a 1-token request before they are marked ready. That loads the weights and compiles the Vulkan pipelines, so the first real request doesn't pay for it.
- `server.uclamp_boost` runs backends under `uclampset -m 1024`. It keeps schedutil from parking the CPU thread that feeds the GPU, which matters on power-saver profiles. There's no difference on `asahi-performance`.
- `hooks.on_load` and `hooks.on_idle` can switch tuned profiles, e.g. `tuned-adm profile asahi-performance` and `tuned-adm profile asahi-balanced`.
- The daemon is a 2-worker tokio runtime, the proxy streams without buffering, and the TUI redraws only on events plus a 1 s status poll.

## Model grader

`g` in the TUI (Models sorted by the grade) and `lmdeck grade [balanced|smartest|fastest|efficient]` rank the local
models. The grade is computed when you ask, from:

| Column | Source | Marked |
|---|---|---|
| Intel | `~/.config/lmdeck/grades.toml` if it has the model, else an estimate: 100 x (1 - loss) x ln(1 + eff) / ln(1001), with eff = total^0.7 x active^0.3 (parameters in billions, from config.json, the GGUF metadata or the name, e.g. `A3B`) and the quantization loss 1 - q(bits) (q = 1 at 4+ bits, 0.93 at 3, 0.85 at 2, 0.75 at 1.58) scaled by √(30 / eff) between 0.3 and 1, since big models lose less. Only the order is meant to be right: 125B-A6.6B > 27B dense > 35B-A3B > 4B > 0.6B, as public indexes rank the Qwen3.x models of those shapes | `*` yours, `~` estimate |
| tok/s | with `usage.perf = true` (off by default), measured: a moving average of decode speed over requests that generated 32+ tokens. Otherwise an estimate: 1 / (active bytes / bandwidth + 4.7 ms), with the bandwidth fitted to your measured MLX/GGUF models (190 GB/s until one is measured, which fits the M1 Max runs in RESULTS.md). Strata only gets measured numbers | `~` estimate |
| W, J/tok | with `usage.perf = true`, the SMC's Total System Power, sampled every 2 s only while a request is served (one sysfs read; at most 600 samples, ~10 KB), averaged over each generation. Unmeasured models get the median of the measured ones | `~` estimate |
| Fit | the server's planner (`/lmdeck/plan`, the check a load runs: exact weights, attention cache, GPU heap); a rough estimate only when the server is down | |

Score = the weighted geometric mean of each metric relative to the best model in the list. Speed counts as
1 - e^(-tok/s / 15): about 12 tok/s is half of it, and past ~30 tok/s more speed adds little. A missing metric counts as half the best. Models
that do not fit right now go last. The weights for intelligence / speed / efficiency are: Balanced 0.5 / 0.3 /
0.2, Smartest 0.8 / 0.2 / 0, Fastest 0.3 / 0.7 / 0, Efficient 0.4 / 0.1 / 0.5.

```toml
# ~/.config/lmdeck/grades.toml: any score you trust, on one 0-100 scale for all the models you list
[models."mlx-community/Qwen3.8-27B-4bit"]
intelligence = 34
```


## Omarchy bar panel

`./install.sh --plugin` installs **Agents + lmdeck** (`omarchy-plugin/m1-max.agents`, symlinked into `~/.config/omarchy/plugins`) in place of Omarchy's stock `omarchy.agents` widget. It keeps the Claude/Codex/Fireworks tabs and adds an **lmdeck** tab on the far right. That tab is selected automatically when the panel opens while the server runs, and has:

- **Server**: the endpoint and LAN IP (marked when exposed), plus Start/Restart, Stop, **Force stop** (SIGKILL of the service cgroup, backends included) and Open TUI.
- **Loaded**: each model with RSS, its keep-alive countdown or "generating", Unload, and **Reload** when settings changed since it was loaded.
- **Memory**: a unified-memory meter, the last request's tok/s, prefill and TTFT, and today's token totals.
- **Downloads**: live progress, a Cancel button for each active download, and clear. Cancelled downloads keep partial files; downloading the same variant resumes.
- **Cache & memory**: keep-alive, max models, MLX prompt cache, GGUF context, GGUF KV cache type (f16/q8_0/q4_0), KV reuse, MLX fast path, memory check, and local/LAN listen.
- **Strata**: prompt path (off / 1024 / 2048 / 4096) with its measured speed, decode-cache and heat ratings, plus Reload when the setting changed; writes `strata.prompt_path` (see [WORKING-PATH-strata-in-editors.md](WORKING-PATH-strata-in-editors.md)).
- **Models**: every local model; click one to load or unload it. A ternary GGUF that runs on the Prism build is prefixed `prism ·`; one that needs it but has no build configured shows `✗ prism build` (see [TERNARY-PRISM.md](TERNARY-PRISM.md)).
- **Hugging Face**: search MLX or GGUF by name, multiple words, repo ID or Hugging Face URL; sort by relevance, popularity, likes or latest update. Expand a repo for variants, estimated memory, fit with current free RAM, and a recommended variant. Click one to download.

Usage records are also written to `~/.local/state/omarchy/agents/usage/lmdeck.json`, so "tokens by day/model" works in the panel (and in the stock widget, if you keep it).

The panel is built to cost almost nothing:

| State | Cost |
|---|---|
| Panel closed, server up | one long-poll request (`/lmdeck/watch`) parked in the daemon, answered only when something changes (at most one a minute when idle) |
| Server down | one refused localhost connect every 20 s |
| Panel open | a status poll every 1.5 s |

The panel never spawns processes except for button presses.

Only `Panel.qml` is forked. It is generated by `sync-from-stock.py` from `/usr/share/omarchy/shell/plugins/agents/Panel.qml`, and the data layer is imported from the stock directory. After an Omarchy update, re-run the script and `omarchy restart shell`.

## VS Code / Copilot fallback

`./install.sh --vscode` installs the **lmdeck VS Code extension** (`vscode-extension/`, packaged without vsce).

lmdeck registers itself as an **Ollama** model provider in VS Code (`chatLanguageModels.json`), so local models show up in Copilot's model picker. Two details:

- Each model is listed under its full name, which keeps quants of one repo apart.
- It advertises a capped context window (`mlx.max_context`, default 64k) instead of the native 262k, which would exhaust GPU memory.

**Nothing polls.** The extension checks the Copilot quota only when VS Code starts and when its window regains focus (at most every `lmdeck.focusCheckMinutes`, default 15), or when you ask. A check is one request to lmdeck, which reads VS Code's recorded `chat.quotaResumeState` (read-only sqlite).

- While the quota is **blocked** (mode `auto`), the extension sets `inlineChat.defaultModel` and `chat.defaultModel` to a local tool-calling model through VS Code's settings API, so inline **Fix** and chat keep working.
- When the quota returns, it restores your previous values exactly.
- The status bar shows `Copilot` or the local model. Click it for: check now, Auto / Always local / Off, the automatic-check toggle, and the log.
- The mode is lmdeck's `vscode.fallback`, shared with the TUI and the bar panel. `vscode.model` picks the model; left empty, lmdeck uses the loaded model if it can call tools, otherwise the smallest local model that can.
- `lmdeck vscode` shows the status.

## Files

| Path | What |
|---|---|
| `~/.config/lmdeck/config.toml` | settings (all editable in the Settings tab) |
| `~/.local/state/lmdeck/logs/` | `lmdeck.log` plus one log per model backend |
| `~/.local/share/lmdeck/` | llama.cpp build, MLX launcher shim |
| `~/.config/systemd/user/lmdeck.service` | optional user service |
| `~/.config/lmdeck/grades.toml` | your intelligence scores for the grader (optional) |
| `~/.local/state/lmdeck/usage.json` | token counts, and the measured speed and power per model |
