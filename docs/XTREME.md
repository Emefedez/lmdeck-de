# XTreme mode (plan)

Status: planned 2026-10-26; the one-time setup is done, the mode itself is not implemented yet. This page is the design and the reasons for it; the numbers
are measured on this machine (M1 Max, 32 GB, Asahi) unless marked as an estimate.

XTreme mode turns the laptop into a headless model server for a long unattended run (overnight bug fixing, batch
reverse-engineering questions): the graphical desktop stops, the panel turns off, Strata gets the memory and the GPU
the desktop was using, and a task loop runs coding agents in fresh, bounded sessions. When it ends, the desktop comes
back as it was.

```
lmdeck xtreme start --tasks tasks.md --hours 8      # desktop off, screen off, model loaded, task loop running
lmdeck xtreme status                                 # from a text console or over ssh
lmdeck xtreme stop                                   # tasks stop, desktop back (also at the end of --hours)
```

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

- `tasks.md`: one task a section (what to fix or find, where, how to check it: the test or command that proves it).
- For each task, a headless agent through lmdeck (`lmdeck launch claude -p --max-turns N`, or opencode), whose prompt is
  the task, the task's `PROGRESS.md` and fixed rules: work on a branch, run the check, commit what works, append what
  was learned and what is left to `PROGRESS.md`, stop.
- Each session is bounded by turns and wall time. Reaching the context window just ends the session; the next one
  starts from `PROGRESS.md` and the commits.
- Restarts are cheap: every session shares one system prompt and tool list, and `strata.prefix_reuse = system` pins
  that prefix, so a new session reads only its task and notes.
- Thinking is capped for the run (2-4k tokens; Strata's default allows ~15-23k, an hour at decode speed).
- A watchdog ends a turn that thinks past its cap or a session past its time, reloads the engine if it dies, and moves
  on after two sessions in a row without a commit or a note.

### Stop (the end of `--hours`, `lmdeck xtreme stop`, or a key at the console)

The agents stop at the end of their turn, the screen comes back (`fb0/blank` = 0), SDDM starts again and autologin
brings the desktop back, the sleep lock goes. `~/.local/state/lmdeck/xtreme/report.md` lists each task with its
commits, its check's result and the last notes, plus the run's numbers: arena, decode and prompt speed, turns, time.

If anything goes wrong in the night, Ctrl+Alt+F2 gives a text login with the run still going; `lmdeck xtreme stop`
from there (or a reboot) restores the desktop.

## The wall: one panel, three places

XTreme has one panel, drawn from one source (`GET /lmdeck/xtreme`, the daemon's run state) in three places:

| Where | When | What it is |
|---|---|---|
| the **wall**, on the text console | during a run (the desktop is off) | the XTreme tab of lmdeck's TUI, full screen, ASCII-safe |
| the **XTreme tab** (7) of the lmdeck TUI | any time, any terminal, also over ssh | the same panel with the rest of the TUI around it: plan tasks, start, read the last report |
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
| `s` | `screen off` | the panel off now; any key on the wall turns it back on |
| `q` | - | leave the wall; the run goes on (`lmdeck xtreme` brings it back) |
| `X` | `stop` | end the run and bring the desktop back (asks first) |
| - | `start`, `status`, `add "task"`, `report` | start a run, one line of state, append a task, the last report |

The editor is lmdeck's (`ui.editor`, then `$VISUAL`, `$EDITOR`, nvim), the pager `$PAGER` or `less`; both run in the
wall's terminal and give it back when they exit, as the TUI's system-prompt editing does today.

### Light on resources

- The wall is the lmdeck binary (7.7 MB) in the TUI mode it already has: no browser, no compositor, no GPU (the
  kernel console draws text on the CPU into the framebuffer). Expected around 10-20 MB of memory (to measure).
- It redraws when something changes: it waits on the daemon's existing long-poll (`/lmdeck/watch`) plus a clock
  tick every 10 s, instead of the TUI's 1 s poll. With the screen off it draws nothing.
- The run itself lives in the daemon (a user service, so it survives the desktop going), as a state machine with its
  state in `~/.local/state/lmdeck/xtreme/state.json`: a daemon restart resumes at the next session.
- One agent process at a time: Claude Code measured 343 MB here; that is the largest cost after the model, and the
  reason sessions run one after another, not side by side.

### On the console

- After SDDM stops, the wall needs a terminal of its own on the display. Either the root helper gains a fifth step
  that opens it on a free console (`openvt` running `lmdeck xtreme` as the user), or the wall is what you get by
  logging in on a console (Ctrl+Alt+F2) and typing `lmdeck xtreme`. The first is automatic; the second widens nothing.
- The console has no font set (`/etc/vconsole.conf`), so the kernel's default draws a 3024x1964 panel: at 16x32 that
  is about 189x61 characters. The wall is laid out for 80x24 and up and uses a bigger font where the
  `terminus-font` package is installed (`setfont ter-132n`).
- `TERM=linux` means 16 colours and a limited set of glyphs: the wall picks ASCII borders and bars there, and the
  Unicode ones in a desktop terminal or over ssh.
- Waking the screen: the kernel console may not undo a blank done through `fb0/blank` on a keypress, so the wall
  turns the screen back on itself (`screen-on`) on any key while it is off. To check on the first run.

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
- **Root for exactly four steps**, through one root-owned helper, `/usr/local/libexec/lmdeck-xtreme-root`
  (`desktop-stop`, `desktop-start`, `screen-off`, `screen-on`; anything else is refused), allowed without a password
  by `/etc/sudoers.d/lmdeck-xtreme` for those arguments only. Sources in `xtreme/` (the sudoers file has `%USER%` for
  the user name). This is what lets the run restore the desktop by itself at the end of `--hours`.
  `screen-off` writes `4` to `fb0/blank` and to the panel backlight's `bl_power`; `screen-on` undoes both.
- **TUI tools**: `w3m` and `r2ghidra` (pacman, extra) installed; `r2` loads the plugin (`pdg` decompiles).

Install elsewhere:

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

## What the first run measures

The arena at load and decode tok/s against the table above; prompt speed and storage reads per prompt; whether
`fb0/blank` powers the panel down on Asahi (DCP) and a keypress brings it back; whether a large arena behaves on
Asahi. kapitsa measured every command buffer 0.5-3.5 s slower on a 48 GB M4 Pro once its process passed ~45% of RAM in
GPU memory (macOS); an XTreme arena would be past that here, so it is worth watching. Also whether prefix reuse and
the kept sessions (`strata.sessions`) show up in the engine log as `reused`.

## Not in this plan

Daytime use (the desktop is the point then), and tasks that need more than ~25k tokens in view at once: those still
reach the window and are better split by hand.
