# XTreme mode (plan)

Status: planned 2026-10-26, nothing implemented yet. This page is the design and the reasons for it; the numbers
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
