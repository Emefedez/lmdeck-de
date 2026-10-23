# Working path: Strata usable in the editor chat

Status: adopted 2026-10-23. Step 1 is done; steps 2-5 are implemented in the order below, easiest first.
`PLAN-strata-in-editors.md` stays as the background plan; where the two disagree, this file wins. All numbers here
were measured on the M1 Max (32 GB, Asahi) with Q2_0 and are quoted with their source.

## What is slow: the chat, not Fix

The session that took minutes was Copilot **chat**, not inline Fix. Fix carries the selected file and a narrow
prompt; chat carries Copilot's whole system prompt, the full tool list and the conversation history, on every turn.

| | lmdeck's own conversation (ctrl+d closes it) | VS Code Copilot chat |
|---|---|---|
| endpoint | `POST /api/chat`, `messages` only (`src/main.rs:585`) | `POST /v1/chat/completions` |
| prompt | 84-793 tokens per turn (log, 2026-10-21) | ~30,540 -> ~20,499 after trimming (log, 2026-10-23 11:51) |
| shared start between turns | reused: 48, 79, 172, 660 tokens reported `reused` | 0 reused: lmdeck sends no `strata_prefix` |
| time before the first token | 1.7-10.8 s | ~166 s at chunk 2048 (see the arithmetic below) |

Where the 20.5k goes. `~/.local/state/lmdeck/tools-seen.json` records what Copilot sends: 89 tools, 22,640 tokens.
Seventeen are `mcp_rea_*` (3,274 tokens) and `server.tool_deny` drops them, leaving the `tools 72 -> 72` in the log.
`lmdeck tools` prints the result as `copilot: 89 tools, ~19366 tokens of schemas sent directly`, and the same command
shows the contrast: `claude: 221 tools, ~3729 tokens`, because Claude Code gets `LEAN_DROP` and Copilot does not.
Split by bucket:

| bucket | tools | tokens |
|---|---|---|
| playwright/browser set (`navigate_page`, `run_playwright_code`, the `terminal_*` and `send_to_terminal` helpers) | 17 | 3,559 |
| `activate_group_0..15` and fallbacks (extension onboarding stubs) | 18 | 2,574 |
| file/read/search/edit core (`read_file`, `grep_search`, `replace_string_in_file`, ...) | 7 | 2,440 |
| everything else (`run_in_terminal` 1,880, `memory` 763, `vscode_*`, github, notebook, python/dotnet) | 30 | 10,793 |

`server.local_tools = lean` only shortens descriptions (600 chars) and parameter docs; the drop list that removes
tools (`anthropic::LEAN_DROP`, `src/anthropic.rs:34`) is Claude Code only, so all 72 schemas stay. That is why
trimming saved 10k and still left 20.5k. Two subtleties worth remembering:

- `groups` groups only `mcp_*` tools. `toolrules::openai` drops denied tools *before* the `has_mcp` test
  (`src/server.rs:1531-1556`), so with every MCP server denied, `groups` behaves exactly as `lean`.
- Strata never truncates a prompt. A real prompt over `--max-context` is a hard 400 ("requests are never
  truncated", `Strata/serve/server.py:4504`), and lmdeck's context guard estimates with 3.8 chars/token
  (`estimate_tokens`, `src/server.rs:1019`), so the 20,499 figure is an estimate, not the tokenizer's count.

At the measured prompt rates that estimate is the whole wait: 20,499 / 109 tok/s = 188 s (chunk 1024), / 123.8 =
166 s (2048), / 134.7 = 152 s (4096), / 14.7 = 23.2 min (prompt path off). Fix would be much cheaper; chat is the
worst case, so the path below optimises chat.

## The order

| # | step | effort | effect on a 20.5k chat turn |
|---|---|---|---|
| 1 | `strata.prompt_path = 2048` | one config line; **done** | 188 s -> 166 s; same energy, less wall clock |
| 2 | split chat/Fix models in the extension (PLAN Part 5) | ~80 lines, no model run | an escape hatch: chat can leave Strata entirely until 3-5 land |
| 3 | a Copilot tool drop list (PLAN Part 3.1, extended) | one list + a rule, ~40 lines | 20.5k -> ~4k; ~166 s -> ~35 s |
| 4 | a lean Copilot system prompt (PLAN Part 3.2) | needs one captured request, then text | ~4k -> ~3k; ~35 s -> ~25 s |
| 5 | prefix reuse (PLAN Part 2) | lmdeck sends `strata_prefix` + a memory cap | repeat turn reads only the new part: seconds |

Impact and effort do not line up: 3 and 5 are what actually make chat usable with Strata, 2 is what makes it usable
today. Doing them in the order above keeps a working editor at every step.

### Step 1 - chunk 2048 (done)

`~/.config/lmdeck/config.toml`: `strata.prompt_path = "2048"` (was `"1024"`). lmdeck writes
`STRATA_METAL_PROMPT_CHUNK=2048` into the run config's copy (`src/backend.rs:1320`), so it applies at the next model
load; a running engine keeps the old chunk until it is unloaded.

Why 2048 and not 4096: 124 vs 135 tok/s on the 10,609-token prompt, but 2.55 vs 4.46 GB of arena held by the chunk
buffers (`RESULTS.md`). On 32 GB that leaves ~10.4 GB of arena for decode at 2048, ~8.5 GB at 4096. Heat: the prompt
path is the same amount of GPU work either way, so a faster chunk means fewer seconds at full power, not more work.

Done when: after a reload, the engine log says `prompts on the matrix units in chunks of 2048`.

### Step 2 - chat and Fix on different models (PLAN Part 5)

Today both `inlineChat.defaultModel` and `chat.defaultModel` get `vscode.model` (`KEYS`,
`vscode-extension/extension.js:14`). Add `vscode.inline_model` and `vscode.chat_model` (empty = `vscode.model`):

- `src/config.rs`: two `String` fields next to `vscode.model`, their `Setting` rows, get/set arms, and the
  `/lmdeck/vscode` payload already returning `suggested_model` (add both fields beside it).
- `vscode-extension/extension.js`: `applyLocal` sets `inlineChat.defaultModel` from `inline_model` and
  `chat.defaultModel` from `chat_model`; `restore()` already saves and undoes each key separately.
- `src/tui/settings.rs` and the bar panel: show both, arrow keys to choose among models that can call tools.

This is relief, not a fix: a small model in chat means giving up Strata there. It also puts Fix on a fast model
(Qwen3.5-4B reads prompts at ~467 tok/s), where Strata's 20k-token requests are wasted anyway.

Done when: lmdeck's Log shows `applied strata/q2_0` for one key and the small model for the other, and switching a
VS Code window from Fix to chat does not need a reload.

### Step 3 - drop the tools a local model does not need

Add a Copilot drop list next to `LEAN_DROP` and apply it to VS Code requests in `trim_openai_tools`
(`src/anthropic.rs:91`), keyed on the client like `toolrules::status` already does (`src/toolrules.rs:76`).

Keep by default: `file_search`, `grep_search`, `read_file`, `list_dir`, `replace_string_in_file`,
`insert_edit_into_file`, `create_file`, `get_errors`, `fetch_webpage`, `view_image` (~2.8k raw tokens, 10 tools).
Drop by default: the playwright set, `activate_*`, notebook, github, `vscode_*` meta, `memory`,
`manage_todo_list`, `runSubagent`, `session_store_sql`, `run_in_terminal` (1,880 tokens on its own), python/dotnet
setup. Everything dropped stays available through `server.tool_allow` (the glob list already exists,
`src/config.rs:786`), so agent mode can be re-enabled per tool instead of paying for all 72.

Expected: 19,366 raw tool tokens -> ~2.8k, request ~20.5k -> ~4k, and that is the difference between ~166 s and ~35 s
to the first token at chunk 2048. Verify from lmdeck's log: `VS Code request ~A -> ~B tokens, tools 72 -> N`.

Risk: dropping `run_in_terminal` breaks build/test loops, so it has to be allow-listed to come back. Make the list a
setting (`server.copilot_drop`, globs) rather than a constant, and let `server.tool_allow` win over it the way
`toolrules::status` already orders denies, allows and the mode (`src/toolrules.rs:75`), so this stays reversible.

### Step 4 - a lean Copilot system prompt (PLAN Part 3.2)

Needs one captured request first: write the last VS Code body to `logs/last-vscode.json` from `v1_proxy` behind a
debug setting (off by default). Then trim the sections a local model does not need (Copilot-only feature notes,
tool-use etiquette) while keeping the edit format Fix and chat rely on, the way `server.local_prompt = lean` does
for Claude Code (8.1k -> 4.5k tokens, `RESULTS.md`).

Note the plumbing: `strata.system_prompt_mode = "prepend"`, so lmdeck's 1.6 KB `prompts/strata.md` is added in front
of Copilot's prompt today; a lean mode has to replace, not prepend. The captured body settles which sections cost
what, so this step waits for the capture rather than guessing.

Done when: the request is ~3k tokens with the system prompt lean and chat still edits and reads files correctly.

### Step 5 - reuse the shared start (PLAN Part 2)

Strata already supports it: a request sends `"strata_prefix": {"messages": K}` and its server resolves it to
`{"tokens": N}`, passes `pin=N` to the engine, and pins that checkpoint so "N questions cost one prefix"
(`Strata/serve/server.py:4541-4607`, `1400-1420`). The lmdeck conversation proves the checkpoint reuse works: its
turns report 48, 79, 172, 660 tokens `reused`.

- `src/server.rs`, `v1_proxy` for Strata models: add `strata_prefix` when `strata.prefix_reuse = auto`, with K =
  the system message plus the stable preamble (the template renders the tool list before the first user turn, so
  K=1 should cover system + tools; confirm from the resolved token count in the engine log before relying on it).
- Do it for the Anthropic and Responses paths too, or chat and Codex stay uncached.
- Memory: a pinned prefix holds the state at that boundary (GDN state of every layer plus the attention cells).
  Measure it, cap how many are held (`prefix_reuse` off / one), and count it in the admission plan; drop the pin
  when free memory is near the floor.

Expected: the second chat turn in the same thread reads `reused` for the whole system prompt, tool list and history,
so a repeat costs seconds instead of ~35 s (step 3) or ~166 s (today). This is the step that makes chat feel local.

## How every step is verified

- lmdeck log: `strata/q2_0: VS Code request ~A -> ~B tokens, tools N -> M`, then
  `VS Code <model>: tin in / tout out, finish=..., Xs` for the turn.
- engine log (`strata-q2_0.log`): `prompt X tokens = R reused + U read in T ms` - R is what step 5 moves.
- `lmdeck tools`: per-tool status (sent/short/dropped) and the per-client total ("copilot: 89 tools, ~19366 tokens");
  steps 3 and 4 are done when that total is what is expected without a model run.
- `POST /v1/chat/completions -> 200 (... ms to headers)` is only the SSE head; time to the first delta is the
  number that matters, and it appears once a request completes.
- Target: a chat turn under ~60 s to the first token cold, under ~10 s on a repeat, with the desktop staying
  usable; Fix under ~15 s.

## Constraints that do not change

- 32k window, `--max-context 32768`; a real prompt over it is a 400, never a truncation. Step 3 is also what keeps
  a long chat under the wall.
- Engine load is 15-17 s, and the first request pays a ~10 s warm-up (the 53-token start-up probe logs 5 tok/s).
- Do not suspend the machine during a Strata run (GPU firmware hang, `LIMITATIONS.md`).
- The prompt path saturates the GPU for the whole read. The per-layer submits keep the desktop shareable between
  parts, but a 20k read is 2-3 minutes of that; steps 3-5 cut the minutes, not the sharing.
