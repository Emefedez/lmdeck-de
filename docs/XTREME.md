# XTreme mode

Status: implemented 2026-10-26 (`src/xtreme.rs`, `src/tui/xtreme.rs`, the helper in `xtreme/`). The task loop, the
controls, the countdown and the report are checked with dry runs (a fake agent, no desktop or model changes; see
[Checked so far](#checked-so-far)). Not yet run for real: stopping the desktop, the panel off, the wall on its console,
a model load into the freed memory, away mode. The numbers on this page are measured on this machine (M1 Max, 32 GB,
Asahi) unless marked as an estimate.

XTreme mode turns the laptop into a headless model server for a long unattended run (overnight bug fixing, batch
reverse-engineering questions): the graphical desktop stops, the panel turns off, Strata gets the memory and the GPU
the desktop was using, and a task loop runs coding agents in fresh, bounded sessions. When it ends, the desktop comes
back as it was.

```
lmdeck xtreme tasks                                  # write the task list (~/.config/lmdeck/xtreme/tasks.md)
lmdeck xtreme start --hours 8                        # desktop off, screen off, model loaded, task loop running
lmdeck xtreme start --dry-run --hours 1              # the same loop with the desktop, screen and model left alone
lmdeck xtreme status                                 # one line, from a text console or over ssh
lmdeck xtreme stop                                   # tasks stop, desktop back (also at the end of --hours)
```

## Server mode: no tasks, a model server for other computers

A run does not need tasks. `lmdeck xtreme start --serve`, or any start without a task list (or with an empty one),
is a server run: the desktop and the screen go off as usual, the model loads into the freed memory and **stays
loaded** for whoever connects, and nothing ends the run early. Tasks can still come later (`a` on the wall,
`lmdeck xtreme add`, or the task list): the agent works on them and the run goes back to serving. In server mode
thinking is not capped (the clients set their own budget).

**Other computers** reach the model APIs on a second listener, `0.0.0.0:11435` (`xtreme.lan_port`), opened for the
run and closed when it ends (`--lan` / `--no-lan`; on by default in server mode):

- only the model APIs: Ollama (`/api/chat`, `/api/generate`, `/api/tags`, ...), OpenAI (`/v1/...`) and Anthropic
  (`/v1/messages`). Nothing that changes lmdeck is on it: no `/lmdeck/*` (settings, hooks, runs), no pull, delete or
  copy. lmdeck's own listener stays on localhost; steer the run over ssh.
- a token (`xtreme.lan_auth = token`, the default): every request carries it as its API key, `Authorization: Bearer
  <token>` or `x-api-key: <token>`; anything else gets 401. It is kept in `~/.local/state/lmdeck/xtreme/lan-token`
  (0600) and reused from run to run, so the other computer's settings keep working. `xtreme.lan_auth = none` drops it.
- the firewall: ufw is active here, so the run opens the port (helper step `lan-on PORT`) and closes it again on
  stop (`lan-off PORT`).

`lmdeck xtreme ip` (and `i` on the wall) prints the addresses, the token and ready-made client settings:

```
Claude Code:  ANTHROPIC_BASE_URL=http://192.168.0.36:11435 ANTHROPIC_AUTH_TOKEN=lmx-... ANTHROPIC_MODEL=strata/q2_0 claude
OpenAI API:   OPENAI_BASE_URL=http://192.168.0.36:11435/v1 OPENAI_API_KEY=lmx-...
```

The wall's header shows the address and the token; its NOW pane, the request in flight, the last request's speed
and today's requests.

## The run's profile: everything at its maximum (but the prompt)

While a run holds, lmdeck lays a profile over its settings, in memory only (`config.toml` is not touched, and the
status and Settings show the values in force): the model stays resident (`keep_alive -1`, one model at a time), the
GPU heap is `auto` (all the safe free memory, the expert arena for Strata), every cache is kept (Strata's
conversation sessions and shared system prefix, MLX prompt cache 16 entries and prefix checkpoints, a 2 GB MLX
buffer cache, llama.cpp cache reuse), uclamp boost on. The prompt path, the prompt groups and the system prompts stay
as you set them.

**The context grows.** Strata runs with `--max-context 32768` normally; in a run it gets `xtreme.context`, by default
262144, the engine's maximum. That is cheap on this model: only its long-range attention layers keep per-token state,
about 64 MiB a layer at 262K (Strata's `session.cpp`), so roughly a gigabyte of a ~15 GB arena; the arena the load
ends up with is in the run's log. If the grown window does not load, the run loads the model's own once. MLX and GGUF
contexts stay as set (their KV cache is not cheap).

## Brightness without a desktop

Off the desktop, logind lets nobody set the brightness, so a sudoers line allows `brightnessctl` without a password
(`xtreme/lmdeck-xtreme.sudoers`). On the wall, the Mac's brightness and keyboard-light keys work: the helper's `wall`
step maps their keycodes to F13-F16 in the console keymap (only those four keys, only on the text consoles; the
desktop reads the keyboard itself), so they reach the wall as keys; no listener runs. F1/F2 and F5/F6 (with Fn) and
`[` `]` `{` `}` do the same. `lmdeck xtreme brightness screen|kbd up|down|PERCENT` works from any console or ssh, with
or without a run. The keyboard's light goes off with the panel and comes back with it.

## Online help: delegation

`xtreme.delegate` (or `start --delegate claude|codex|custom`) lets the local agent ask a stronger online model when it
is stuck: the session rules gain a line about `lmdeck xtreme delegate "question"`, which runs, in the task's
directory, Claude Code with your own login (`claude -p`, model `xtreme.delegate_model`, e.g. `opus`; without its
Edit, Write and Bash tools), Codex (`codex exec --sandbox read-only`) or `xtreme.delegate_cmd`, and prints the answer.
It reads the files, changes nothing, and does not see the session: the question has to say what is known. lmdeck's
variables (`ANTHROPIC_BASE_URL` and the rest) are removed first, so it reaches the online model, not the local one.
`xtreme.delegate_max` (10) questions a run; each shows on the wall and in the log. Off by default.

## The task list

`lmdeck xtreme tasks` opens it (`xtreme.tasks`, default `~/.config/lmdeck/xtreme/tasks.md`; written from a template
the first time). One task a `## title` section; the lines right under the title set its options, the rest is what
the agent reads:

```
## lmdeck: grader shows NaN for MoE models
repo: ~/Work/lmdeck
check: cargo test grade
turns: 30
minutes: 30

The grader's intelligence column is NaN for strata/q2_0. Look at src/grade.rs ...
```

| Option | Means |
|---|---|
| `repo:` | a git repository: the task gets its own branch `xtreme/<task>` in a worktree under `~/.local/state/lmdeck/xtreme/worktrees/`, so the agent never touches your checkout or its uncommitted work. A directory that is not git is worked in place; no repo, a scratch directory. |
| `check:` | the command that proves the task is done (run niced in the worktree after a session that committed or claimed done; `xtreme.check_minutes`, 10) |
| `turns:`, `minutes:` | per session (default `xtreme.max_turns` 40, `xtreme.session_minutes` 45) |

`lmdeck xtreme add "title" [--repo DIR]` appends a task (the repository defaults to the git checkout you are in); the
first line of the text is the title, the rest its body. The run reads the file again between sessions, so tasks can
be added, reordered and reworded during a run, from the wall (`t`) or over ssh.

Each task keeps its notes in `~/.local/state/lmdeck/xtreme/tasks/<task>/PROGRESS.md`, and its sessions' transcripts
next to it (`session-N.log` readable, `session-N.jsonl` as the agent wrote it).

## Why: free memory is decode speed

Strata streams the experts it cannot keep from the SSD. How many it keeps (the expert arena) is fixed when the model
loads, from the memory free at that moment. Free memory is the single biggest factor in decode speed here:

| Load | Free at load | Expert arena | Decode |
|---|---|---|---|
| 2026-10-25 22:00, desktop in use | 12.6 GiB | 1.0 GB | 4.3-5.1 tok/s |
| 2026-10-25 23:03, desktop in use | 16.3 GiB | 3.85 GB | 6.4 tok/s |
| 2026-10-21, standalone engine run | - | 8 GB | 9.5-13 tok/s |
| estimate: XTreme | ~25 GiB | ~15 GB | ~10-15 tok/s |

For comparison (vendor benchmarks, other Macs): kapitsa decodes the same Q2_0 model at 16.8 tok/s on a 48 GB M4 Pro
with a 12 GB expert store; Strata on macOS reports 7.9 tok/s on an M1 Max. The estimate row is an extrapolation, not a
measurement; the first XTreme run measures it.

What the desktop holds (2026-10-26, from the user cgroups):

| What | Memory |
|---|---|
| graphical apps (`app-graphical.slice`) | 7.9 GB, of which Chromium 5.2 GB (4.7 GB shared memory) |
| Hyprland (`wayland-wm@hyprland.desktop.service`) | 0.95 GB |
| VS Code, opencode and the rest | ~0.8 GB |
| desktop GPU buffers not counted above (frame buffers, textures) | part of the 18 GB of `Shmem` |

About 10 GB in all, plus the compositor's GPU work: with no desktop nothing renders, so the GPU runs only Strata.

The prompt path does not get faster (it is GPU-bound at ~126-130 tok/s, see strata-asahi `vulkan/RESULTS.md`), but
a bigger arena means fewer SSD reads per prompt too.

## What it does

### Start

1. **Checks.** No Strata request in flight (`/lmdeck/status`), linger on (below), the task file readable, the agent
   command present. Anything missing: it says what and stops before touching the desktop.
2. **Keeps the machine awake.** A `systemd-inhibit --what=sleep:idle:handle-lid-switch --mode=block` lock for the
   run's length. Needed: logind's lid switch is at its default (suspend), and a suspend during a Strata run crashed
   the GPU firmware on 2026-10-21 (`LIMITATIONS.md`).
3. **Moves itself out of the desktop.** The run continues as a transient user service
   (`systemd-run --user --unit=lmdeck-xtreme`), so stopping the session does not stop it.
4. **Stops the graphical desktop.** The session is SDDM autologin into `omarchy.desktop` through uwsm
   (`/etc/sddm.conf.d/autologin.conf`, no `Relogin`). Ending the session would bring up SDDM's greeter, which renders
   too, so XTreme stops SDDM itself (`systemctl stop sddm`): Hyprland, the shell, the apps and the greeter all go.
   Chromium restores its tabs when it is opened again.
5. **Turns the panel off.** With no compositor the kernel console owns the display; writing `4` (powerdown) to
   `/sys/class/graphics/fb0/blank` blanks it through the DRM fbdev helper (panel `eDP-1`, backlight
   `apple-panel-bl`). Default: off. `--screen on` keeps a text console visible instead. A keypress on the console
   brings the screen back.
6. **Loads the model into the freed memory.** Unload Strata if it was loaded (its arena was sized for the busy
   desktop), then load it; the log line `expert arena N GB` is checked and reported.
7. **Runs the task loop** (next section).

### The task loop: fresh sessions, memory in files

A 32k window at ~6-15 tok/s makes compaction the worst part of a long run: summarizing a full window re-reads it
(~4 min) and writes the summary at decode speed (a Copilot compaction took over 10 minutes on 2026-10-25), and every
compaction loses detail. XTreme does not compact. Each task runs in fresh sessions that keep their memory in files:

- `tasks.md`: one task a section (above).
- For each task, a headless agent through lmdeck: Claude Code (`claude -p --output-format stream-json --max-turns N
  --dangerously-skip-permissions`, with lmdeck's endpoint and model as `lmdeck launch claude` sets them) or opencode
  (`opencode run --auto`; `xtreme.agent`). It runs in the task's worktree with the PATH of the shell that started the
  run. Its prompt is the task, where it works, the check, the session's limits and the notes so far; the fixed rules
  go in the system prompt (`--append-system-prompt`, the same text every session): work only here, commit each step
  that works, never push or switch branches, run the check, append `## Session N` notes before stopping, and end the
  notes with `STATUS: done` or `STATUS: blocked: <why>`.
- Each session is bounded by turns and wall time (paused time does not count). Reaching the context window just ends
  the session; the next one starts from `PROGRESS.md` and the commits.
- After a session: a task is **done** when the notes say `STATUS: done` and the check (if any) passes; a failed check
  is written into the notes and the task goes on. `STATUS: blocked` ends it as **blocked**; two sessions in a row
  without a commit or a note, **stuck**; `xtreme.max_sessions` (6) sessions, **out of sessions**. Then the next task.
- Restarts are cheap: every session shares one system prompt and tool list, and `strata.prefix_reuse = system` pins
  that prefix, so a new session reads only its task and notes.
- Thinking is capped for the run: lmdeck adds `reasoning_budget_tokens` (`xtreme.thinking_budget`, 3072) to every
  Strata request that does not set its own (Strata's default allows ~15-23k, an hour at decode speed).
- If the engine dies, the next session loads it again first; if lmdeck restarts, the run resumes from `state.json`
  at the next session (the session cut short goes back to pending).

### Back to the desktop: the timer

A run has an end from the start: `--hours 8` or `--until 07:30`. The wall's header counts down to it
(`desktop back in 5h12m`).

1. **Wind-down.** 15 minutes before the end (`xtreme.wind_down_minutes`, at most a quarter of the run) no new session
   starts; the one running goes on.
2. **The countdown.** At the end the screen comes on and the wall shows `Back to the desktop in 60 s`: any key adds
   an hour (again and again), Enter or `X` goes now. Nobody there: it goes when the countdown ends.
   A timed run whose tasks have all ended goes back early, the same way.
3. **The return.** The agents stop at the end of their turn (up to 2 minutes, then they are stopped), Strata unloads (its arena was sized for an empty
   machine; the desktop needs that memory back, and the next request loads it again the normal way), the screen
   comes on, SDDM starts and autologin brings Omarchy back, the sleep lock goes. Chromium and the other closed apps
   are not reopened by XTreme; they restore their own sessions when opened.
4. **The report.** A notification on the desktop gives the night's result and opens
   `~/.local/state/lmdeck/xtreme/report.md`.

**A dead-man switch.** At the start XTreme also arms a transient user timer, independent of the run
(`systemd-run --user --on-active=<end + 15 min> --unit=lmdeck-xtreme-return`): it asks lmdeck to stop the run, and two
minutes later turns the screen on and starts SDDM whatever happened, even if the daemon or the run has died. `+`
(one more hour) moves it; a clean return cancels it. Linger keeps user timers running without a
login, so the desktop comes back on time whatever happened in the night.

**Earlier, by hand:** `X` on the wall (it asks first), `lmdeck xtreme stop` over ssh or on another console
(Ctrl+Alt+F2), or a reboot (SDDM's autologin starts the desktop as on any boot).

### Runs without an end

`--hours` and `--until` are optional. Without them a run has no end: no countdown, no wind-down, no return timer. It
works through the tasks, then waits with the model unloaded (its memory free, nothing running) until a task is added
(`lmdeck xtreme add "..."`, over ssh too), and stops only when told (`X`, `lmdeck xtreme stop`). Liveness replaces the
timer: systemd restarts lmdeck if it dies, the run reloads the engine if it dies, and the hardware watchdog (below)
reboots a hung machine.

### Away mode: the machine on its own for days, reached over Tailscale

`lmdeck xtreme start --away` is the open-ended run plus what a week without anyone at the machine needs. Use case: the
MacBook stays home, plugged in, and is used from another laptop (the ThinkPad) over Tailscale, both as a box to ssh
into and as a model server.

- **Tailscale only while away.** `tailscaled` stays disabled normally; away mode starts it and a normal stop stops it
  again (helper steps `remote-on` / `remote-off`). From the other laptop, `ssh m1-max` (the tailnet name) gives the
  wall (`lmdeck xtreme`, the same panel over ssh), the shell and nvim. `tailscale serve` exposes lmdeck's model
  APIs (`/v1`, `/api` on port 11434) to the tailnet only, so agents on the ThinkPad can use the MacBook's models;
  lmdeck itself keeps listening on localhost, and its control API stays local (steer the run over ssh). ufw lets in only `tailscale0` (`ufw allow in on tailscale0`), nothing from the LAN.
- **Power.** Away mode refuses to start on battery. It sets the charge limit to 80%
  (`/sys/class/power_supply/macsmc-battery/charge_control_end_threshold`, now 100) for the week and restores it on
  stop (helper step `battery-limit 80|100`).
- **A hung machine reboots.** `/dev/watchdog0` exists, but systemd's runtime watchdog is off (`RuntimeWatchdogSec=0`),
  so a kernel or GPU hang would stay hung. Away mode turns it on for the run (60 s, set live on systemd's
  `RuntimeWatchdogUSec` through D-Bus: helper step `watchdog-on`, `watchdog-off` on stop; no config file changes).
- **After a reboot it comes back by itself.** The run's `state.json` survives the reboot; when lmdeck starts (linger,
  at boot) it sees an away run and enters it again: tailscaled on, SDDM stopped once autologin has started it, the
  model reloaded, the tasks resumed. (A run that is not away ends with a reboot: that is one of the ways back.) A power cut needs the Mac to power on by itself
  when power returns: an Apple setting (`pmset autorestart` in macOS), not verified under Asahi.
- **The lid** can stay closed (the lid switch is inhibited); the machine then cools through the keyboard deck, so a
  hard surface, not a bag or a sofa.
- **Nothing updates while away**: no system updates, no lmdeck or Strata updates.

**One-time setup** (needs the password, so it is yours to type):

```
sudo pacman -S tailscale
sudo systemctl start tailscaled && sudo tailscale up     # prints a login URL: log in once
sudo systemctl disable --now tailscaled                  # off by default; away mode turns it on
sudo ufw allow in on tailscale0
```

The helper's steps for it (`remote-on`, `remote-off`, `battery-limit 80|100`, `watchdog-on|off`) are in `xtreme/`;
reinstall the helper and the sudoers rule once (below). `tailscale serve` forwards only `/v1` and `/api` (the
model APIs) on port 11434 of the tailnet; lmdeck's control API (`/lmdeck/*`, which can change settings, run hooks
and start runs) is not forwarded. Not tried yet (Tailscale is not installed here).

### Stop

Every way back above ends the same: agents stopped, Strata unloaded, screen on, SDDM started, sleep lock gone. `~/.local/state/lmdeck/xtreme/report.md` lists each task with its
commits, its check's result and the last notes, plus the run's numbers: arena, decode and prompt speed, turns, time.

If anything goes wrong in the night, Ctrl+Alt+F2 gives a text login with the run still going; `lmdeck xtreme stop`
from there (or a reboot) restores the desktop.

## The wall: one panel, three places

XTreme has one panel, drawn from one source (`GET /lmdeck/xtreme`, the daemon's run state) in three places:

| Where | When | What it is |
|---|---|---|
| the **wall**, on the text console | during a run (the desktop is off) | the XTreme tab of lmdeck's TUI, full screen, ASCII-safe |
| the **XTreme tab** (7) of the lmdeck TUI | any time, any terminal, also over ssh | the same panel with the rest of the TUI around it: add and plan tasks, start (end, server, network, away, dry run), read the last report. `X` on Home and *lmdeck XTreme* in the Omarchy menu open it. |
| a **card in the bar panel** (`LmdeckSection.qml`) | on the desktop, before and after a run | the last run's summary, and *Start XTreme...*, which opens the TUI's tab (no second implementation) |

When the desktop comes back, a notification gives the night's result and opens the report. So the GUI only starts
and summarizes; seeing and steering the work live is the TUI, the same code on the console and in a desktop
terminal.

```
 __  _______ ____  _____ __  __ _____
 \ \/ /_   _|  _ \| ____|  \/  | ____|   strata/q2_0          02:14 of 08:00
  >  <  | | | |_) |  _| | |\/| |  _|     arena 14.8 GB        12.1 tok/s  prompt 128 tok/s
 /_/\_\ |_| |_| \_\_____|_|  |_|_____|   desktop off          RAM 26.1 / 30.9 GB  SSD 3.1 TB read
 ----------------------------------------------------------------------------------------------
 TASKS                        3 of 7 done   | NOW  #4 panel: scroll on hover   session 2, turn 9/40
 [x] 1 lmdeck: grader NaN       3 commits   | context  [#######...]  21.3k / 32k
 [x] 2 strata: tail chunk       1 commit    | reading the prompt 4.1k / 6.0k, 15 s left
 [x] 3 t1code: sidebar width    2 commits   | last tool  cargo test -> 93 passed
 [>] 4 panel: scroll on hover   ...         |
 [ ] 5 r2: find the CRC in fw.bin           |
 ----------------------------------------------------------------------------------------------
 PROGRESS #4 (last lines)                   | LOG
 - hover handler is in LmdeckSection.qml    | 02:13 read_file  LmdeckSection.qml
 - Flickable jumps on resize, try ...       | 02:13 bash       qmllint LmdeckSection.qml
 ----------------------------------------------------------------------------------------------
 t tasks  p progress  d diff  l log  e edit  space pause  n skip  s screen off  q leave  X end
```

### Keys and commands

One key on the wall, one word on the command line, the same action:

| Key | `lmdeck xtreme ...` | Does |
|---|---|---|
| `t` | `tasks` | open `tasks.md` in the editor (the run reads it again between sessions: add, reorder, reword freely) |
| `p` | `progress [N]` | open the current task's `PROGRESS.md` (the next session starts from what is there) |
| `d` | `diff [N]` | the task branch's diff against its base, in the pager |
| `l` | `log [N]` | the agent's transcript, followed live |
| `e` | `edit [N]` | the task's repository in the editor; pauses the task first so the agent and you never edit at once |
| space | `pause` / `resume` | finish the current turn, then hold (the model stays loaded) |
| `n` | `skip` | end this task's session and go to the next |
| `s` | `screen off` / `screen on` | toggle the panel fully off and on (also off by itself after 2 min without a key) |
| `o` / `O` | `open [FILE]` | pick a file from the task repositories and open it in nvim (`O`: with LSP) |
| `!` | - | a shell (niced), `exit` returns |
| `q` | - | leave the wall; the run goes on (`lmdeck xtreme` brings it back) |
| `X` | `stop` | end the run and bring the desktop back (asks first) |
| `a` | `add "task"` | add a task (title and repository), typed in on the wall or the TUI tab |
| `i` | `ip` | where other computers connect, with the token and client settings (server runs) |
| `+` | `extend` | one more hour before the desktop comes back |
| - | `start [--hours N \| --until HH:MM \| --away \| --serve \| --lan/--no-lan \| --dry-run]`, `status`, `watch`, `report`, `plan` | start a run (no end unless given; `--away` = remote, unattended; `--serve` = a model server, tasks optional), one line of state, the wall as plain text, the last report, the TUI on its XTreme tab |

The editor is lmdeck's (`ui.editor`, then `$VISUAL`, `$EDITOR`, nvim), the pager `$PAGER` or `less`; both run in the
wall's terminal and give it back when they exit, as the TUI's system-prompt editing does today.

### Light on resources

- The wall is the lmdeck binary (7.7 MB) in the TUI mode it already has: no browser, no compositor, no GPU (the
  kernel console draws text on the CPU into the framebuffer). Measured 2026-10-26: the TUI holds 9.3 MB, of which
  2.8 MB its own memory and 6.5 MB the binary's code, which is the same file the daemon (8.4 MB) already has in the
  page cache. A separate, smaller binary for the wall would save a megabyte or two and duplicate code, so there is
  one binary.
- For ssh, scripts and the smallest footprint there is a plain CLI view as well: `lmdeck xtreme status` prints one
  line, `lmdeck xtreme watch` reprints the wall as plain text when something changes (no full-screen TUI, no key
  handling), and every key's action is a command (below).
- It redraws when something changes: it waits on the daemon's existing long-poll (`/lmdeck/watch`) plus a clock
  tick every 10 s, instead of the TUI's 1 s poll. With the screen off it draws nothing.
- The run itself lives in the daemon (a user service, so it survives the desktop going), as a state machine with its
  state in `~/.local/state/lmdeck/xtreme/state.json`: a daemon restart resumes at the next session.
- One agent process at a time: Claude Code measured 343 MB here; that is the largest cost after the model, and the
  reason sessions run one after another, not side by side.

### On the console

- **Automatic login onto the wall.** After SDDM stops, the root helper's step `wall` sets the font on VT 8, starts
  `lmdeck xtreme wall` there as the user who ran sudo, as a system unit of its own (`systemd-run
  --unit=lmdeck-xtreme-wall --uid=<user> -p TTYPath=/dev/tty8`, so it outlives a restart of lmdeck's service), and
  switches to it (`chvt 8`): no password, no typing, the wall is what the display shows. VT 8 is above the six that
  logind gives gettys, so nothing else claims it. `desktop-start` stops the unit. Logging in by hand (Ctrl+Alt+F2,
  `lmdeck xtreme`) still works, over ssh too. On the console, the wall leaves by itself once the run has ended.
- **Font:** `terminus-font` is installed (4.49.1); the wall's console uses `ter-132n` (16x32), about 189x61 characters
  on the 3024x1964 panel. Nothing changes in `/etc/vconsole.conf`: the font is set for the wall's console only.
- `TERM=linux` means 16 colours and a limited set of glyphs: the wall picks ASCII borders and bars there, and the
  Unicode ones in a desktop terminal or over ssh.
- Waking the screen: the kernel console may not undo a blank done through `fb0/blank` on a keypress, so the wall
  turns the screen back on itself (`screen-on`) on any key while it is off. To check on the first run.

## Using the machine during a run (a headless dev box)

XTreme stops only the graphical session. Everything a CLI-only Linux has stays: the network (NetworkManager), sshd
(enabled and running here), ufw, tmux, systemd user services, the other consoles (Ctrl+Alt+F3...). So the machine can
be used on its own, without another computer, to vibe-code, review the agents' work and fix things by hand.

- **The editor on demand.** `o` on the wall opens a file picker over the task repositories (git status first, then
  recent files, typing filters), and the chosen file opens in nvim; quitting nvim gives the wall back. Nothing runs
  while you do not edit. Measured: nvim with this machine's LazyVim config 35 MB, `nvim --clean` 12 MB; vim is the
  fallback (`ui.editor`). The cost to watch is language servers: rust-analyzer can take a gigabyte or more on a big
  crate, so XTreme starts nvim with LSP off unless asked (`O` opens with it).
- **Colours.** The text console has 16 colours (`TERM=linux`): nvim runs with `notermguicolors` and a 16-colour
  scheme there, so syntax colouring works, in a smaller palette. If that is too little, an optional console made of
  `cage` + `foot` (foot is installed, cage is not) gives true colour and the usual fonts for as long as the editor is
  open, then exits; it draws only when something changes.
- **A shell.** `!` on the wall opens `$SHELL` in the current task's directory, under `nice -n 19` and `ionice -c 3`;
  `exit` comes back.
- **Hosting.** The wall's header shows the LAN address and the ports your processes listen on (`ss -ltn`), and
  `lmdeck xtreme ip` prints them. ufw is active, so a port has to be opened (`sudo ufw allow PORT`) before another
  machine reaches it.
- **The screen, and what goes with it.** `s` toggles the panel fully off (framebuffer powerdown and backlight off)
  and on. With no key for 2 minutes (`--screen-idle`) it goes off by itself; any key brings it back. Off means nothing
  draws and nothing interactive stays loaded:
  - nvim saves its session (open files, cursor, window layout: `:mksession`) and keeps unsaved changes in its swap
    files (`:preserve`), then is killed (a clean quit would delete the swap files), and its language servers with it;
    the next key reopens it on that session, and nvim offers to recover the unsaved changes from the swap files
    (`R`). Nothing is written to your files that you did not save. (The wall talks to nvim over the socket it opens
    with `--listen`; it notices "no key" from the console's access time, as `w` does.)
  - an idle shell from `!` is closed; a shell running something (a server you host, a build) is left alone, so
    hosted apps stay reachable while the screen is off.
  - the wall stops drawing (it already draws nothing while the screen is off).

### Does it slow the model?

Mostly not, for the reason the arena works the way it does. Strata's expert arena is sized and allocated at load;
what starts afterwards does not shrink it. An editor, a shell and ssh are a few tens of MB and idle CPU. What does
compete:

- **Memory beyond the arena.** Strata reads the experts it does not hold through the page cache (the log's "file
  tier"), and the n-gram table is memory-mapped too. Something large started later (a language server, a big build)
  takes page cache from it, and decode reads more from the SSD. XTreme therefore keeps an interactive reserve out of
  the arena when it loads (`--reserve`, default 2 GB), so editing and small builds never press on it.
- **CPU.** The engine's host turns between layers are short and latency-sensitive. A full-speed `cargo build` while
  the model writes slows both; the shell from `!` runs with `nice -n 19` and `ionice -c 3`, so a build waits for
  the model instead of the other way round.
- **The agents' own builds and tests** do not compete: while an agent runs a tool the model is idle, waiting for
  the result.

## Working on the machine in TUI mode

XTreme keeps no desktop, so whatever the user wants during the run is terminal-only (a text console, or ssh from
another machine):

- reverse engineering: radare2 is installed; `r2pm -ci r2ghidra` adds Ghidra's decompiler inside r2. Ghidra itself
  needs its GUI, though its `analyzeHeadless` runs without a display.
- docs and the web: w3m or lynx (not installed). Terminal browsers built on a full Chromium (carbonyl, browsh) would
  take back the memory XTreme frees.
- the agents need none of these: they work through their tools, rea's binary-analysis tools included (in lmdeck's
  `groups` tool mode). rea's browser and native-UI capture tools need a desktop and are left out of XTreme runs.

## One-time setup (done on this machine 2026-10-26)

- **Linger** on (`loginctl enable-linger m1-max`): the user's services, lmdeck included, keep running when the last
  login session ends, which stopping SDDM does.
- **Root for a fixed list of steps**, through one root-owned helper, `/usr/local/libexec/lmdeck-xtreme-root`, allowed
  without a password by `/etc/sudoers.d/lmdeck-xtreme` for exactly these arguments: `desktop-stop`, `desktop-start`,
  `screen-off`, `screen-on`, `wall`, for away mode `remote-on`, `remote-off`, `battery-limit 80|100`,
  `watchdog-on`, `watchdog-off`, and for the network listener `lan-on PORT`, `lan-off PORT` (a ufw rule for that
  port, only when ufw is active); anything else is refused. The same file allows `/usr/bin/brightnessctl`. Sources in `xtreme/` (the sudoers file has `%USER%` for
  the user name). This is what lets the run restore the desktop by itself at the end of `--hours`.
  `screen-off` writes `4` to `fb0/blank` and to the panel backlight's `bl_power`; `screen-on` undoes both.
  The installed copy (2026-10-26) has only the first four steps: **install it again** with the commands below, then
  `lmdeck xtreme start` checks every step it needs with `sudo -n -l` before touching anything.
- **TUI tools**: `w3m` and `r2ghidra` (pacman, extra) installed; `r2` loads the plugin (`pdg` decompiles).

Install (again) or elsewhere:

```
sudo install -Dm755 -o root -g root xtreme/lmdeck-xtreme-root /usr/local/libexec/lmdeck-xtreme-root
sed "s/%USER%/$USER/" xtreme/lmdeck-xtreme.sudoers > /tmp/x && sudo visudo -cf /tmp/x && sudo install -m440 -o root -g root /tmp/x /etc/sudoers.d/lmdeck-xtreme
loginctl enable-linger "$USER"
```

## Safety rules

- Never while a Strata request runs; the desktop stops before the model loads, never during a load or a request.
- No suspend for the whole run (the inhibitor above); XTreme refuses to start on battery below a set charge.
- Everything it changes it records first and restores on stop, including after a crash of the run (the next
  `lmdeck` start sees the record and offers to restore).
- It never kills apps one by one: it stops the display manager, the way logging out does.

## Settings

`[xtreme]` in the config (also in the TUI's Settings, section XTreme, and `lmdeck config xtreme.<key>`). `lan_port`
(11435) and `lan_auth` (`token` / `none`) set the network listener of server runs.

| Key | Default | Means |
|---|---|---|
| `tasks` | `~/.config/lmdeck/xtreme/tasks.md` | the task list |
| `model` | the first Strata model | what a run loads |
| `agent`, `agent_args` | `claude`, none | the headless agent (`opencode` also works) and extra arguments, e.g. `--strict-mcp-config` to leave the MCP servers out |
| `max_turns`, `session_minutes`, `max_sessions` | 40, 45, 6 | the bounds of a session, and sessions per task |
| `thinking_budget` | 3072 | thinking tokens per Strata reply during a run (0 = the engine's) |
| `reserve_gib` | 2.0 | memory kept out of the arena for editing and builds; replaces `server.mem_reserve_gib` while a run holds |
| `screen`, `screen_idle_secs` | `off`, 120 | the panel at the start, and the idle time before the wall turns it off |
| `context` | 262144 | Strata's context window during a run (0 = the model's own) |
| `delegate`, `delegate_model`, `delegate_max`, `delegate_cmd` | `off`, none, 10, none | online help for the agent |
| `wind_down_minutes`, `check_minutes`, `min_battery` | 15, 10, 50 | (config file only) |

## Checked so far

2026-10-26, with dry runs (`--dry-run`: the root steps are only logged, the model is not loaded) on a second daemon
(its own port and state directories) and a fake agent that writes Claude Code's stream-json:

- a task that commits, passes its check and writes `STATUS: done` ends **done**, with its branch, worktree, commit,
  check and notes in the report; a task that leaves nothing twice ends **stuck**; a timed run whose tasks have all
  ended returns early;
- pause stops the agent's whole process group (`T`) between model requests and resume continues it; skip ends the
  session (SIGTERM to the group); an open-ended run with no task left waits, and `lmdeck xtreme add` (from inside a
  repository) gives it the next one; stop returns;
- a timed run winds down, counts down 60 s, `extend` adds the time and re-arms the countdown, and the return runs the
  steps in order (screen on, desktop start);
- the wall in a 150x44 terminal with `TERM=linux` (ASCII only): blank while the panel is "off", the first key turns
  it on, `X` asks, `X` again ends the run; tab 7 and its start form.

Server mode (same day, dry run, test port 11536): a start with no task list serves; without the token or with a
wrong one the network port answers 401, with it (Bearer or `x-api-key`) 200; `/lmdeck/status` and `/api/pull` are
404 there; a task added while serving runs and the run goes back to serving; stop closes the port.

Not checked yet: anything the root helper does (the installed helper predates the `wall` and away-mode steps), the
model load into the freed memory, nvim's session save on screen-off, opencode as the agent, away mode.

## What the first run measures

The arena at load and decode tok/s against the table above; prompt speed and storage reads per prompt; whether
`fb0/blank` powers the panel down on Asahi (DCP) and a keypress brings it back; whether a large arena behaves on
Asahi. kapitsa measured every command buffer 0.5-3.5 s slower on a 48 GB M4 Pro once its process passed ~45% of RAM in
GPU memory (macOS); an XTreme arena would be past that here, so it is worth watching. Also whether prefix reuse and
the kept sessions (`strata.sessions`) show up in the engine log as `reused`.

## Not in this plan

Daytime use (the desktop is the point then), and tasks that need more than ~25k tokens in view at once: those still
reach the window and are better split by hand.
