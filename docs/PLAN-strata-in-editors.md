# Plan: Strata fast and usable from editors (VS Code Fix, chat, coding agents)

Status: plan only, nothing below is implemented unless it says so. Written 2026-10-21.

## The goal

Strata (Qwen3.8-Flash-Next, 125B MoE) answering VS Code Copilot's inline Fix and chat, and coding agents, with a
wait comparable to the MLX models and a context large enough for real files: 32k at least, 64k if memory allows.

## Where it stands

| | Now | Why |
|---|---|---|
| Decode | 11-13 tok/s | experts streamed from the SSD, 8-12 GB arena |
| Prompt reading | ~10 tok/s | prompts go through decode windows (`STRATA_METAL_PROMPT_GEMM=0`) |
| A Copilot Fix request | tens of thousands of tokens before trimming; ~33k refused at the 32k window | Copilot sends its system prompt, every tool (MCP included) and the file context |
| Repeated requests | each one read from the start | the shared start (system prompt + tools) is not reused |
| Context | 32k (the run config's `--max-context`) | never measured higher on Asahi |

At 10 tok/s a 10k-token request takes ~17 minutes before the first answer token. The parts below attack the three
causes: the speed of reading, what is re-read, and how much is sent.

## Part 0: measure first (needs a model run, ask before)

1. Restart the lmdeck server so the Copilot trimming is active (`server.local_tools`, commit e2e4072 and e63e5cd).
2. One Fix and one chat request from VS Code on `strata/q2_0`. Record from `lmdeck.log` the line
   `VS Code request ~N -> ~M tokens, tools A -> B`, and from `strata-q2_0.log` the `prompt N tokens = R reused + U read
   in X ms` line.
3. Check the `file tier I/O ... the OS read 26304.5 MB ... for 88.5 MB of expert reads` line seen on 2026-10-21:
   whether it counts the arena fill since start or real per-request reads. If it is per request, it is a bug to fix
   before anything else (it would cap prompt speed far below the SSD).

Output: the real request sizes and the current costs, in docs/RESULTS.md.

## Part 1: the prompt path on (strata-asahi)

The kernels are ported and pass parity (prompt, gdn_prompt, qsa_prompt, dense_mm, moe_mm). What remains:

1. **First end-to-end run.** Steps in strata-asahi's `vulkan/README.md` ("Trying the prompt path"). Compare against
   the decode-window path on the same prompt: prompt tok/s, first-token time, and the answer (greedy, should agree
   for the first tokens; fp16 operands differ slightly from the window's).
2. **Breakdown.** `STRATA_VK_TIMING=1`: GPU time per chunk against SSD time per chunk. Expected bound: one pass over
   the layer's missing experts per chunk, ~26 GB at ~7 GB/s, about 4 s a 1024-row chunk, so a ceiling in the low
   hundreds of tok/s (estimate, not measured).
3. **Tune the chunk.** `STRATA_METAL_PROMPT_CHUNK` 1024 / 2048 / 4096: a longer chunk makes fewer passes over the
   experts but its buffers come out of the expert arena (4.3 GB at 4096 on the Mac). Pick by prompt tok/s and by the
   decode speed that remains.
4. **GPU sharing.** A chunk is submitted part by part (per layer), so the desktop keeps the GPU between parts. Check
   the longest single part while the desktop is in use; if a part holds the GPU too long, split it (per GEMM).
5. **Defaults.** Once it is right: `setup_asahi.py` stops writing `STRATA_METAL_PROMPT_GEMM=0`; lmdeck gets a setting
   `strata.prompt_path` (on / off, default on after validation) that sets the variable for the engine, so it can be
   turned off without editing the run config.

Done when: a 10k-token prompt is read at least 10x faster than today with the same first answer tokens, and the
desktop stays responsive during it.

## Part 2: reuse the shared start of each request (lmdeck + strata-asahi)

Copilot's system prompt and tool list are the same in every request; only the code and the question change.

1. **Verify what Strata already does on Vulkan.** Its server keeps turn checkpoints (the state at the last
   `<|im_start|>`) and supports `strata_prefix` (a request marks its first messages as a shared, read-only prefix that
   is held and reused). Send the same long request twice; the second should report most tokens as `reused`.
2. **lmdeck marks the prefix.** For requests to a Strata model (Copilot, Claude Code, Codex), lmdeck adds
   `"strata_prefix": {"messages": K}` where K covers the system message (and the tool list, which the template puts
   before the first user turn). Setting: `strata.prefix_reuse` (auto / off, default auto).
3. **Keep the prefix stable.** Anything lmdeck changes in it must be the same in every request: the system prompt
   files it prepends, the trimmed tool list (deterministic order, which the trimming already keeps), no dates or
   per-request text in the prefix (`{date}` in a system prompt file breaks reuse once a day, acceptable).
4. **Memory.** A held prefix costs the model's state at that point (GDN state of every layer plus the attention
   cells). Measure it, cap how many are held, and count them in lmdeck's memory plan.

Done when: a second Fix in the same file starts answering in seconds (only the new part is read).

## Part 3: send less (lmdeck)

1. **Tools: done.** `lean` drops MCP tools and shortens descriptions; `groups` offers MCP tools on demand through
   `tool_group` / `call_tool` (commits e2e4072, e63e5cd). Verify with Part 0's numbers, and that tool calls still work
   in Copilot's agent mode.
2. **A lean Copilot system prompt.** As `server.local_prompt = lean` does for Claude Code (8.1k -> 4.5k tokens):
   identify the sections of Copilot's prompt a local model does not need (notes about Copilot-only features, long
   tool-use etiquette) and drop or shorten them, keeping the instructions Fix relies on (the edit format). Setting:
   reuse `server.local_prompt` for Copilot too. Needs one captured Copilot request to design (lmdeck can write the
   last VS Code request to `logs/last-vscode.json` when a debug setting is on; off by default).
3. **Context guard accuracy.** The ~3.8 chars/token estimate refused a ~33k request against a 32k window. Once the
   engine reports real counts, calibrate per model (Strata's tokenizer) so requests near the limit are not refused
   needlessly.

Done when: a Copilot Fix request is under ~8k tokens after trimming.

## Part 4: context size (strata-asahi + lmdeck)

1. **Measure.** Load at 32k, 64k and 128k (Models -> e -> Context size; lmdeck passes `--max-context` through a copy
   of the run config, implemented in commit e2e4072). For each: free memory after load, the arena size the engine
   picks, decode tok/s on the story prompt. Ask before these runs.
2. **Plan for it in lmdeck.** The Strata admission floor (9 GiB) does not include context-dependent memory; add the
   measured per-token cost so the planner and the grader's Fit stay right.
3. **Default.** Raise the default only if decode loses little; otherwise keep 32k and let Models -> e raise it per
   use.

## Part 5: route by task (lmdeck VS Code extension)

Copilot keeps separate default models for inline chat (Fix) and chat (`inlineChat.defaultModel`,
`chat.defaultModel`). Today lmdeck's extension sets both to `vscode.model`.

1. Settings `vscode.inline_model` and `vscode.chat_model` (empty = `vscode.model`), so Fix can use a fast MLX model
   (Qwen3.5-4B reads prompts at ~467 tok/s) while chat uses Strata, until Parts 1-2 make Strata fast enough for both.
2. The bar panel and Settings show both, with ←/→ choices among local models that can call tools.

## Toggles and defaults

Everything above is a setting in lmdeck (Settings, the bar panel), each with a safe default:

| Setting | Default | Part |
|---|---|---|
| `strata.prompt_path` | off until Part 1 is validated, then on | 1 |
| `strata.prefix_reuse` | auto | 2 |
| `server.local_tools` | lean (yours: groups) | 3, done |
| `server.local_prompt` | lean, applied to Copilot too | 3 |
| `vscode.inline_model`, `vscode.chat_model` | empty (= `vscode.model`) | 5 |
| per-model context (Models -> e) | the run config's | 4, done |

Performance first: none of these adds work while lmdeck is idle; debug captures and speed/power recording stay
opt-in (`usage.perf`).

## Order and what needs a model run

1. Part 0 (model run).
2. Part 5 (no model run; immediate relief for Fix).
3. Part 1 (model runs).
4. Part 2 (lmdeck side without a run; verification with a run).
5. Part 3.2 (needs one captured request).
6. Part 4 (model runs).

Model runs follow the usual rules: ask first, nothing compiling, do not suspend during a run, close large GPU users,
watch free memory, stop if it drops low.
