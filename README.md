# lmdeck-DE for Asahi

Local language models on Apple Silicon under Asahi Linux, with the GPU doing the work. The main piece is
**lmdeck**: one Rust binary that is a terminal UI and an Ollama/OpenAI/Anthropic-compatible server on
`127.0.0.1:11434`. It has three backends:

| Backend | Runs | On |
|---|---|---|
| MLX | MLX safetensors models | [omarchy-mlx](https://github.com/joshuaswarren/omarchy-mlx) on the Apple GPU (Honeykrisp Vulkan) |
| GGUF | GGUF models | llama.cpp built with Vulkan |
| Strata | Qwen3.8-Flash-Next, 125B mixture of experts, experts streamed from the SSD | [strata-asahi](https://github.com/Emefedez/strata-asahi), a Vulkan port of Strata's Metal engine, on [honeykrisp-strata](https://github.com/Emefedez/honeykrisp-strata)'s driver build |

"DE" is the desktop part: Omarchy menu rows, a bar panel, a launcher entry and a VS Code fallback. All of it is
optional.

Everything was built and measured on one machine: a MacBook Pro M1 Max with 32 GB, Asahi Linux 7.1.12 and Omarchy.
Other machines are untested (see [limitations](docs/LIMITATIONS.md)).

## Repositories

| Repo | What |
|---|---|
| **lmdeck-de** (this one) | lmdeck (TUI + server), the Omarchy bar panel, the VS Code extension, the docs |
| [strata-asahi](https://github.com/Emefedez/strata-asahi) | Fork of [Niko1221/Strata](https://github.com/Niko1221/Strata). Branch `vulkan-asahi` adds `vulkan/`, the engine for Apple GPUs on Linux |
| [honeykrisp-strata](https://github.com/Emefedez/honeykrisp-strata) | Builds the Honeykrisp (Mesa) driver as a private, per-process Vulkan ICD with a 90% GPU heap and bf16 cooperative matrices, plus the GPU and SSD probes behind the port |

## Quick start

```bash
./install.sh             # build, install ~/.local/bin/lmdeck, launcher entry, Omarchy menu rows
lmdeck                   # the TUI; it starts the server when it needs it
lmdeck service install   # optional: the server starts at login
```

In the TUI, **Backends** (4) installs and updates MLX, llama.cpp and Strata. **Get models** (3) downloads models.
**Models** (2) loads them. Clients then point at `http://127.0.0.1:11434` (Ollama) or `.../v1` (OpenAI).
[docs/SETUP.md](docs/SETUP.md) has the full setup, Strata included.

## The TUI

Six screens. Each one has an action bar at the top listing exactly the keys it handles. An action that does not
apply right now is dimmed, and pressing its key says why. Only `1`-`6`, `Tab`, `g`, `?` and `q` work everywhere,
and `?` lists the keys of the screen you are on.

| Screen | For |
|---|---|
| 1 Home | Server on/off, what is loaded and how full its context is (the last request's prompt + reply against the window, live while a prompt is read, with tokens/s and time left), memory (free, kept free, GPU heap, what models can use), backends, usage, downloads; `f` opens the memory manager (reserve and heap sliders, processes, make room) |
| 2 Models | Local models with fit (the server's planner decides), grade and details; favourites (`f`, listed first), load, chat, per-model settings, make room, delete |
| 3 Get models | The omarchy-mlx catalog and a Hugging Face search, with a memory estimate and fit for each variant |
| 4 Backends | MLX, GGUF and Strata: state, and actions that show their plan and ask before they run |
| 5 Settings | Every setting, with its description, default and when a change applies |
| 6 Logs | The server's log and the loaded model's log |

Drawn from the real TUI on the M1 Max (`cargo test screenshots -- --ignored`), Tokyo Night palette:

| | |
|---|---|
| ![Home](docs/screenshots/home.svg) | ![Models, sorted by the grader](docs/screenshots/models.svg) |
| Home | Models, sorted by the grader (Balanced) |
| ![Backends](docs/screenshots/backends.svg) | ![Memory manager](docs/screenshots/memory.svg) |
| Backends: each action's plan before it runs | The memory manager (`f`); process list trimmed to the desktop's own |
| ![Get models](docs/screenshots/get-models.svg) | ![Settings](docs/screenshots/settings.svg) |
| Get models: the omarchy-mlx catalog | Settings: description, default, when it applies |

### Model grader (`g`)

`g` sorts Models by a grade for a goal (Balanced, Smartest, Fastest, Efficient; `g` again for the next goal).
`lmdeck grade [goal]` prints the same table. The grade is built from whatever models you have, when you ask for it.
Nothing polls, and no model names are hardcoded:

- **Intel**: an estimate from the model's own metadata, meaning total and active parameters (config.json or GGUF
  keys) and bits per weight. It cannot tell generations or fine-tunes apart. `~/.config/lmdeck/grades.toml` takes a
  real score when you have one:
  ```toml
  [models."mlx-community/Qwen3.8-27B-4bit"]
  intelligence = 34
  ```
- **Speed**: an estimate from the weights each model reads per token. With `usage.perf = true` (off by default,
  performance first), lmdeck measures decode tok/s as you use a model and calibrates the estimate on it.
- **Power**: with `usage.perf = true`, the SMC's total system power, sampled only while a request is being served,
  which gives J/token. That costs one sensor read every 2 s during generation and about 10 KB of memory.
- **Fit**: the server's planner answers, which is the same check a load runs. Without the server, a rough estimate
  stands in, and it is marked as one.

Details and the formula are in [docs/REFERENCE.md](docs/REFERENCE.md#model-grader).

## XTreme mode (planned)

For long unattended runs (a night of bug fixing, batches of reverse-engineering questions), XTreme mode turns the
laptop into a headless model server. The graphical desktop stops and the panel turns off. Strata reloads into the
freed memory, and coding agents work through a task list in fresh, bounded sessions. At the end the desktop comes
back with a report. The design is [docs/XTREME.md](docs/XTREME.md). The one-time setup is done on this machine;
the mode itself is not implemented yet.

**Why.** Strata keeps as many experts in memory as are free when it loads, and that sets decode speed:

| Free at load | Expert arena | Decode |
|---|---|---|
| 12.6 GiB (desktop in use) | 1.0 GB | 4.3-5.1 tok/s |
| 16.3 GiB (desktop in use) | 3.85 GB | 6.4 tok/s |
| (standalone run, 2026-10-21) | 8 GB | 9.5-13 tok/s |
| ~25 GiB, XTreme (estimate) | ~15 GB | ~10-15 tok/s (estimate) |

The desktop holds about 10 GB (Chromium alone 5.2 GB), and with it gone nothing else uses the GPU.

**What a run does.**
1. Checks that no Strata request is running.
2. Blocks sleep, idle and the lid switch for the run's length.
3. Stops SDDM, so nothing renders, not even the greeter.
4. Logs in on a console by itself and opens the wall there (Terminus 16x32 font, ~189x61 characters).
5. Turns the panel off; any key on the wall turns it back on.
6. Reloads Strata and runs the tasks.

Each task gets fresh agent sessions (Claude Code or opencode, headless, through lmdeck). The session's memory is the
task's `PROGRESS.md` and its commits, never a compacted context. Thinking is capped and a watchdog moves past
stuck turns.

**Back to the desktop, on a timer.** A run has an end from the start (`--hours 8` or `--until 07:30`), and the
wall counts down to it.
1. 15 minutes before the end, no new session starts.
2. At the end the screen comes on with `Back to the desktop in 60 s`. Any key adds an hour; Enter or `X` goes now;
   nobody there, it goes when the countdown ends.
3. The agents stop, Strata unloads (the desktop needs that memory back), SDDM starts and autologin brings Omarchy
   back. A notification opens the night's report.

A dead-man switch is armed at the start: a separate user timer that brings the screen and the desktop back 15
minutes after the end even if the run or lmdeck itself has died. You can also go back early with `X` on the wall,
`lmdeck xtreme stop` (over ssh or on Ctrl+Alt+F2), or a reboot.

**The wall.** One panel in three places: full screen on the text console during a run, as tab 7 of the TUI in any
terminal (also over ssh), and as a summary card with *Start XTreme...* in the bar panel.

```
 __  _______ ____  _____ __  __ _____
 \ \/ /_   _|  _ \| ____|  \/  | ____|   strata/q2_0          02:14 of 08:00
  >  <  | | | |_) |  _| | |\/| |  _|     arena 14.8 GB        12.1 tok/s  prompt 128 tok/s
 /_/\_\ |_| |_| \_\_____|_|  |_|_____|   desktop off          RAM 26.1 / 30.9 GB
 -----------------------------------------------------------------------------------------
 TASKS                   3 of 7 done | NOW  #4 panel: scroll on hover   session 2, turn 9/40
 [x] 1 lmdeck: grader NaN  3 commits | context  [#######...]  21.3k / 32k
 [>] 4 panel: scroll ...   ...       | last tool  cargo test -> 93 passed
 -----------------------------------------------------------------------------------------
 t tasks  p progress  d diff  l log  e edit  space pause  n skip  s screen off  q leave  X end
```

(An illustration of the layout; the numbers are not measurements.)

| Key | Command | Does |
|---|---|---|
| `t` / `p` | `lmdeck xtreme tasks` / `progress` | edit the task list / the current task's notes (read again between sessions) |
| `d` / `l` | `diff` / `log` | the task branch's diff / the agent's transcript, live |
| `e` | `edit` | the task's repository in your editor (the task pauses first) |
| space / `n` | `pause` / `skip` | hold after this turn / go to the next task |
| `s` / `q` / `X` | `screen off\|on` / - / `stop` | toggle the panel fully off / leave the wall (the run goes on) / end the run, desktop back |
| `o` / `!` | `open [FILE]` / - | pick a file and open it in nvim / a shell |
| - | `start`, `status`, `watch`, `add "task"`, `report` | start, one line of state, the wall as plain text, append a task, the last report |

**Using the machine during a run.** Only the graphical session stops. The network, sshd, tmux and the other consoles
stay, so the machine works as a headless dev box on its own:
- `o` picks a file from the task repositories and opens it in nvim (LazyVim config measured at 35 MB, only while it
  is open; language servers stay off unless asked with `O`). Syntax colouring works in the console's 16 colours.
- `!` opens a shell.
- The wall shows the LAN address and the listening ports for apps you host. ufw is on, so open a port first.
- `s` toggles the screen fully off; it also turns off after 2 minutes without a key.

It barely slows the model: the expert arena is allocated at load, and XTreme keeps 2 GB out of it for interactive
work. Large builds started by hand run niced, so they wait for the model.

**Light.** The wall is the same lmdeck binary in its TUI mode. Measured, it holds 9.3 MB, of which 2.8 MB is its own
and the rest is the binary's code, shared with the daemon (8.4 MB). It redraws only when something changes and
nothing while the screen is off. `lmdeck xtreme watch` is the plain-text version for ssh and scripts. The run lives
in the daemon, so it survives the desktop stopping. The largest cost after the model is the agent process (Claude
Code: 343 MB), so sessions run one at a time.

**Setup (done here, 2026-10-26).**
- Linger is on.
- `/usr/local/libexec/lmdeck-xtreme-root`, a root-owned helper, has four steps: `desktop-stop`, `desktop-start`,
  `screen-off` and `screen-on`. `/etc/sudoers.d/lmdeck-xtreme` allows those four arguments only, without a password.
  Sources and install steps are in `xtreme/` and the plan.
- `w3m`, `r2ghidra` and `terminus-font` are installed for working on the text console.
- Still to add with the build: the helper's fifth step, `wall` (open the wall on its own console as the user), and
  its argument in the sudoers rule.

## Docs

- [SETUP.md](docs/SETUP.md): install everything, step by step
- [RESULTS.md](docs/RESULTS.md): what was measured, with what
- [NOT-WORKED.md](docs/NOT-WORKED.md): what was tried and did not help, and why
- [IDEAS.md](docs/IDEAS.md): what could come next
- [XTREME.md](docs/XTREME.md): XTreme mode (planned): desktop off, screen off, the freed memory to Strata, and coding
  agents in fresh bounded sessions for an unattended night
- [LIMITATIONS.md](docs/LIMITATIONS.md): what does not work, or was never tested
- [MODELS.md](docs/MODELS.md): what runs, and how to tell before downloading
- [PLAN-strata-in-editors.md](docs/PLAN-strata-in-editors.md): the plan to make Strata fast in VS Code and coding agents
- [WORKING-PATH-strata-in-editors.md](docs/WORKING-PATH-strata-in-editors.md): the adopted order for it, easiest first,
  with the request sizes that make the editor chat slow
- [TERNARY-PRISM.md](docs/TERNARY-PRISM.md): Prism's PQ2_0/PTQ1_0 ternary GGUFs, the second llama.cpp build, and how
  lmdeck picks it per model
- [SESSION-2026-10-23.md](docs/SESSION-2026-10-23.md): what changed in the panel, the editors and the GGUF path on
  that day, with the numbers
- [REFERENCE.md](docs/REFERENCE.md): the server's APIs, coding agents, system prompts, memory planning, settings,
  files

## License

MIT. Strata is MIT (Niko1221 and contributors). Mesa is MIT.
