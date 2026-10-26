//! XTreme mode (docs/XTREME.md): the graphical desktop stops, the model reloads into the memory
//! the desktop held, and a headless coding agent works through a task list in fresh, bounded
//! sessions whose memory is the task's notes file and its commits (no compaction).
//!
//! The run lives in the daemon (a user service, so it outlives the desktop) as a state machine
//! with its state in ~/.local/state/lmdeck/xtreme/state.json: a daemon restart resumes it at the
//! next session. Root is needed for a few fixed steps only, through `lmdeck-xtreme-root` and its
//! sudoers rule (xtreme/ in the repo).

use crate::backend::Manager;
use crate::config::{expand, state_dir, Config};
use crate::models::Format;
use crate::sys;
use crate::usage::EVENTS;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

pub const HELPER: &str = "/usr/local/libexec/lmdeck-xtreme-root";
const BATTERY: &str = "/sys/class/power_supply/macsmc-battery";
const AC: &str = "/sys/class/power_supply/macsmc-ac/online";
/// The dead-man switch: a user timer that brings the desktop back even if the run has died.
const RETURN_UNIT: &str = "lmdeck-xtreme-return";
/// After the end: this long to add an hour (any key) before the desktop comes back.
const COUNTDOWN_SECS: u64 = 60;
/// How long the dead-man timer waits past the end.
const DEADMAN_SECS: u64 = 15 * 60;

/// Strata's thinking budget per reply while a run holds (0 = the engine's own).
pub static THINKING_BUDGET: AtomicU32 = AtomicU32::new(0);
/// Memory kept free at load while a run holds, in bytes (0 = server.mem_reserve_gib).
pub static RESERVE: AtomicU64 = AtomicU64::new(0);

pub fn dir() -> PathBuf {
    state_dir().join("xtreme")
}
fn state_path() -> PathBuf {
    dir().join("state.json")
}
pub fn report_path() -> PathBuf {
    dir().join("report.md")
}
pub fn task_dir(slug: &str) -> PathBuf {
    dir().join("tasks").join(slug)
}
pub fn progress_path(slug: &str) -> PathBuf {
    task_dir(slug).join("PROGRESS.md")
}
fn worktree_path(slug: &str) -> PathBuf {
    dir().join("worktrees").join(slug)
}
pub fn tasks_path(cfg: &Config) -> PathBuf {
    if cfg.xtreme.tasks.trim().is_empty() {
        crate::config::config_dir_or_home().join("lmdeck/xtreme/tasks.md")
    } else {
        expand(cfg.xtreme.tasks.trim())
    }
}

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Local wall-clock "HH:MM" of an epoch time.
pub fn clock(t: u64) -> String {
    let tt = t as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&tt, &mut tm) };
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

/// "5h12m" / "12m" / "40s".
pub fn span(secs: u64) -> String {
    match secs {
        s if s >= 3600 => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
        s if s >= 60 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

/// The next local time at HH:MM after `from` (today, else tomorrow).
pub fn next_clock(hhmm: &str, from: u64) -> Option<u64> {
    let (h, m) = hhmm.trim().split_once(':')?;
    let (h, m): (i32, i32) = (h.parse().ok()?, m.parse().ok()?);
    if !(0..24).contains(&h) || !(0..60).contains(&m) {
        return None;
    }
    let tt = from as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&tt, &mut tm) };
    tm.tm_hour = h;
    tm.tm_min = m;
    tm.tm_sec = 0;
    tm.tm_isdst = -1;
    let mut t = unsafe { libc::mktime(&mut tm) } as i64;
    if t <= from as i64 {
        t += 86400;
    }
    Some(t as u64)
}

// ---------- the task list ----------

/// One task of tasks.md: a `## title` section. Lines right under the title of the form
/// `repo: ~/Work/x`, `check: cargo test`, `turns: 30`, `minutes: 20` set its options; the rest
/// is the task as the agent reads it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TaskSpec {
    pub slug: String,
    pub title: String,
    pub repo: Option<String>,
    pub check: Option<String>,
    pub turns: Option<u32>,
    pub minutes: Option<u32>,
    pub body: String,
}

pub fn slugify(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
        if out.len() >= 48 {
            break;
        }
    }
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() { "task".into() } else { out }
}

pub fn parse_tasks(text: &str) -> Vec<TaskSpec> {
    let mut tasks: Vec<TaskSpec> = Vec::new();
    let mut in_fields = false;
    for line in text.lines() {
        if let Some(title) = line.strip_prefix("## ") {
            let title = title.trim().trim_start_matches("[ ] ").trim_start_matches("[x] ").trim().to_string();
            let mut slug = slugify(&title);
            let base = slug.clone();
            let mut n = 2;
            while tasks.iter().any(|t| t.slug == slug) {
                slug = format!("{base}-{n}");
                n += 1;
            }
            tasks.push(TaskSpec { slug, title, ..Default::default() });
            in_fields = true;
            continue;
        }
        let Some(t) = tasks.last_mut() else { continue };
        if in_fields {
            if let Some((k, v)) = line.split_once(':') {
                let v = v.trim().to_string();
                let set = match k.trim().to_ascii_lowercase().as_str() {
                    "repo" => { t.repo = Some(v).filter(|v| !v.is_empty()); true }
                    "check" => { t.check = Some(v).filter(|v| !v.is_empty()); true }
                    "turns" => { t.turns = v.parse().ok(); true }
                    "minutes" => { t.minutes = v.parse().ok(); true }
                    _ => false,
                };
                if set {
                    continue;
                }
            }
            if line.trim().is_empty() && t.body.is_empty() {
                continue;
            }
            in_fields = false;
        }
        t.body.push_str(line);
        t.body.push('\n');
    }
    for t in &mut tasks {
        t.body = t.body.trim().to_string();
    }
    tasks
}

/// The section `lmdeck xtreme add` appends.
pub fn task_section(text: &str, repo: Option<&str>) -> String {
    let mut lines = text.trim().lines();
    let title = lines.next().unwrap_or("task").trim();
    let rest: Vec<&str> = lines.collect();
    let mut s = format!("\n## {title}\n");
    if let Some(r) = repo {
        s.push_str(&format!("repo: {r}\n"));
    }
    if !rest.is_empty() {
        s.push('\n');
        s.push_str(rest.join("\n").trim());
        s.push('\n');
    }
    s
}

pub const TASKS_TEMPLATE: &str = "# XTreme tasks\n\
\n\
One task a section. Under the title, optional `repo:` (a git repository: the agent works on a\n\
branch of its own in a worktree), `check:` (the command that proves the task is done),\n\
`turns:` and `minutes:` (per session). The rest is what the agent reads: what to fix or find,\n\
where, and how to tell it worked. The run reads this file again between sessions.\n\
\n\
## example: describe the first task here\n\
repo: ~/Work/example\n\
check: cargo test\n\
\n\
What is wrong, where to look, what done looks like.\n";

// ---------- run state ----------

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    #[default]
    Idle,
    /// Desktop, screen, sleep lock and the other one-time steps.
    Starting,
    /// The model loading into the freed memory.
    Loading,
    Running,
    /// No task left (an open-ended run): the model unloaded until one is added.
    Waiting,
    /// Close to the end: no new session starts.
    WindDown,
    /// The end: the desktop comes back when it runs out (a key adds an hour).
    Countdown,
    Returning,
    Ended,
}

impl Phase {
    pub fn active(self) -> bool {
        !matches!(self, Phase::Idle | Phase::Ended)
    }
    pub fn label(self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::Starting => "starting",
            Phase::Loading => "loading the model",
            Phase::Running => "running",
            Phase::Waiting => "waiting for tasks",
            Phase::WindDown => "winding down",
            Phase::Countdown => "back to the desktop soon",
            Phase::Returning => "returning to the desktop",
            Phase::Ended => "ended",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct TaskState {
    pub slug: String,
    pub title: String,
    /// pending | running | done | blocked | stuck | skipped | out of sessions | failed
    pub status: String,
    pub sessions: u32,
    /// Sessions in a row that left neither a commit nor a note.
    pub idle: u32,
    pub commits: u32,
    pub repo: Option<String>,
    pub worktree: Option<String>,
    pub branch: Option<String>,
    pub base: Option<String>,
    pub check: Option<String>,
    pub check_ok: Option<bool>,
    pub check_tail: String,
    /// Why it ended, or the agent's STATUS line.
    pub note: String,
}

impl TaskState {
    pub fn finished(&self) -> bool {
        !matches!(self.status.as_str(), "pending" | "running")
    }
    /// Where the agent works: the worktree, the repository itself (not git), or a scratch dir.
    pub fn workdir(&self) -> PathBuf {
        self.worktree.as_deref().or(self.repo.as_deref()).map(expand).unwrap_or_else(|| task_dir(&self.slug).join("work"))
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Session {
    pub slug: String,
    pub n: u32,
    pub started: u64,
    pub deadline: u64,
    pub turns: u32,
    pub max_turns: u32,
    pub pid: Option<u32>,
    pub log: String,
    pub last_tool: String,
    pub progress_len: u64,
    pub commits_before: u32,
    pub paused_at: Option<u64>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct State {
    pub phase: Phase,
    pub boot_id: String,
    pub started: u64,
    pub ends_at: Option<u64>,
    pub countdown_until: Option<u64>,
    pub away: bool,
    /// Everything but the root steps and the model (for checking the loop itself).
    pub dry_run: bool,
    pub model: String,
    pub arena_gb: Option<f64>,
    pub load_secs: Option<f64>,
    pub tasks_file: String,
    pub agent: String,
    pub agent_bin: String,
    /// PATH of the shell that started the run: the agent's tools (cargo, git, ...) come from it.
    pub path_env: String,
    /// "on" | "off": the panel now.
    pub screen: String,
    pub screen_start: String,
    pub desktop_stopped: bool,
    pub battery_limited: bool,
    pub remote: bool,
    pub watchdog: bool,
    pub paused: bool,
    pub pause_requested: bool,
    pub skip_requested: bool,
    pub stop_requested: bool,
    /// When the stop was asked (the agent gets up to 2 min to finish its turn).
    pub stop_at: Option<u64>,
    pub tasks: Vec<TaskState>,
    pub session: Option<Session>,
    pub sessions: u32,
    pub turns: u32,
    /// What happened, newest last ("HH:MM text").
    pub log: Vec<String>,
    pub ended: Option<u64>,
    pub end_reason: String,
}

impl State {
    fn note(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        sys::log(&format!("xtreme: {msg}"));
        self.log.push(format!("{} {msg}", clock(now())));
        let n = self.log.len();
        if n > 200 {
            self.log.drain(..n - 200);
        }
    }
    fn task_mut(&mut self, slug: &str) -> Option<&mut TaskState> {
        self.tasks.iter_mut().find(|t| t.slug == slug)
    }
    pub fn task(&self, slug: &str) -> Option<&TaskState> {
        self.tasks.iter().find(|t| t.slug == slug)
    }
    /// The task the wall shows: the one in a session, else the next one to run.
    pub fn current(&self) -> Option<&TaskState> {
        self.session.as_ref().and_then(|s| self.task(&s.slug)).or_else(|| self.tasks.iter().find(|t| !t.finished()))
    }
}

pub struct Xtreme {
    st: Mutex<State>,
    wake: tokio::sync::Notify,
    driving: AtomicBool,
    /// The agent of the session in progress.
    child: Mutex<Option<tokio::process::Child>>,
    /// The sleep lock (systemd-inhibit) for the run's length.
    inhibit: Mutex<Option<std::process::Child>>,
}

pub static XT: LazyLock<Xtreme> = LazyLock::new(|| Xtreme {
    st: Mutex::new(std::fs::read(state_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()),
    wake: tokio::sync::Notify::new(),
    driving: AtomicBool::new(false),
    child: Mutex::new(None),
    inhibit: Mutex::new(None),
});

fn boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap_or_default().trim().to_string()
}

fn read_num(p: &str) -> Option<u64> {
    std::fs::read_to_string(p).ok()?.trim().parse().ok()
}

/// (on AC power, battery charge %).
pub fn power() -> (bool, Option<u64>) {
    (read_num(AC).is_none_or(|v| v == 1), read_num(&format!("{BATTERY}/capacity")))
}

impl Xtreme {
    pub fn state(&self) -> State {
        self.st.lock().unwrap().clone()
    }

    /// Change the state, save it, wake the long-polls. (Not the driver: it would spin on its
    /// own writes; requests from outside wake it with `poke`.)
    fn with<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        let r = {
            let mut st = self.st.lock().unwrap();
            let r = f(&mut st);
            save(&st);
            r
        };
        EVENTS.bump();
        r
    }

    fn poke(&self) {
        self.wake.notify_one();
    }

    pub fn active(&self) -> bool {
        self.st.lock().unwrap().phase.active()
    }
}

fn save(st: &State) {
    let _ = std::fs::create_dir_all(dir());
    let tmp = state_path().with_extension("json.tmp");
    if std::fs::write(&tmp, serde_json::to_vec_pretty(st).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, state_path());
    }
}

// ---------- root steps ----------

/// One step of the root helper (sudo -n: never a password prompt). A dry run only logs it.
async fn helper(dry: bool, args: &[&str]) -> Result<(), String> {
    if dry {
        sys::log(&format!("xtreme: dry run: would run {HELPER} {}", args.join(" ")));
        return Ok(());
    }
    let o = tokio::process::Command::new("sudo").arg("-n").arg(HELPER).args(args).output().await.map_err(|e| format!("sudo: {e}"))?;
    if o.status.success() {
        Ok(())
    } else {
        Err(format!("{} {}: {}", HELPER, args.join(" "), String::from_utf8_lossy(&o.stderr).trim()))
    }
}

/// Whether the helper may run without a password (`sudo -n -l` lists the command when allowed).
fn helper_allowed(arg: &str) -> bool {
    std::process::Command::new("sudo")
        .args(["-n", "-l", HELPER, arg])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn user_name() -> String {
    std::env::var("USER").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| {
        let pw = unsafe { libc::getpwuid(libc::getuid()) };
        if pw.is_null() { String::new() } else { unsafe { std::ffi::CStr::from_ptr((*pw).pw_name) }.to_string_lossy().into_owned() }
    })
}

fn systemctl_user(args: &[&str]) {
    let _ = std::process::Command::new("systemctl").arg("--user").args(args).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
}

/// Arm (or re-arm) the dead-man timer: `secs` from now it asks lmdeck to stop the run, and two
/// minutes later turns the screen on and starts the desktop whatever lmdeck did.
fn arm_deadman(base_url: &str, secs: u64) {
    systemctl_user(&["stop", &format!("{RETURN_UNIT}.timer")]);
    systemctl_user(&["reset-failed", &format!("{RETURN_UNIT}.service")]);
    let script = format!(
        "curl -s -m 5 -X POST {base_url}/lmdeck/xtreme/stop >/dev/null 2>&1; sleep 120; sudo -n {HELPER} screen-on; sudo -n {HELPER} desktop-start"
    );
    let r = std::process::Command::new("systemd-run")
        .args(["--user", "--quiet", &format!("--unit={RETURN_UNIT}"), &format!("--on-active={secs}s"), "--timer-property=AccuracySec=10s", "sh", "-c", &script])
        .status();
    if !r.is_ok_and(|s| s.success()) {
        sys::log("xtreme: could not arm the return timer (systemd-run --user)");
    }
}

fn disarm_deadman() {
    systemctl_user(&["stop", &format!("{RETURN_UNIT}.timer")]);
}

fn take_sleep_lock() -> Option<std::process::Child> {
    std::process::Command::new("systemd-inhibit")
        .args(["--what=sleep:idle:handle-lid-switch", "--who=lmdeck", "--why=XTreme run", "--mode=block", "sleep", "infinity"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()
}

// ---------- start ----------

#[derive(Deserialize, Default, Debug)]
#[serde(default)]
pub struct StartReq {
    pub hours: Option<f64>,
    pub until: Option<String>,
    pub away: bool,
    pub dry_run: bool,
    pub tasks: Option<String>,
    pub model: Option<String>,
    pub agent: Option<String>,
    /// The agent's executable, resolved by the CLI (the daemon's PATH is short).
    pub agent_bin: Option<String>,
    pub path_env: Option<String>,
    pub screen: Option<String>,
    pub reserve_gib: Option<f64>,
}

/// The model a run loads: given, else xtreme.model, else the first Strata model, else the first local one.
fn pick_model(mgr: &Manager, cfg: &Config, want: Option<&str>) -> Result<String, String> {
    let want = want.map(str::to_string).or_else(|| Some(cfg.xtreme.model.clone()).filter(|m| !m.is_empty()));
    if let Some(w) = want {
        return mgr.resolve(&w).map(|m| m.name).map_err(|e| format!("{e:#}"));
    }
    let ms = mgr.models();
    ms.iter()
        .find(|m| m.format == Format::Strata && m.name != "strata/mock")
        .or_else(|| ms.iter().find(|m| !m.name.to_lowercase().contains("embed") && m.unsupported.is_none()))
        .map(|m| m.name.clone())
        .ok_or_else(|| "no local model to run (xtreme.model)".into())
}

/// Checks first; nothing on the desktop changes unless every one passes.
pub fn start(mgr: Arc<Manager>, req: StartReq) -> Result<Value, String> {
    let cfg = mgr.cfg();
    if XT.active() {
        return Err("a run is already going (lmdeck xtreme status)".into());
    }
    if mgr.busy() {
        return Err("a model request is running; start when it has finished".into());
    }
    let t = now();
    let ends_at = match (&req.hours, req.until.as_deref().filter(|u| !u.is_empty())) {
        (Some(h), _) if *h > 0.0 => Some(t + (h * 3600.0) as u64),
        (_, Some(u)) => Some(next_clock(u, t).ok_or_else(|| format!("--until {u}: expected HH:MM"))?),
        _ => None,
    };
    if req.away && ends_at.is_some() {
        return Err("--away runs have no end; leave out --hours/--until".into());
    }
    let tasks_file = req.tasks.clone().map(|p| expand(&p)).unwrap_or_else(|| tasks_path(&cfg));
    let text = std::fs::read_to_string(&tasks_file).map_err(|e| format!("{}: {e} (lmdeck xtreme tasks writes one)", tasks_file.display()))?;
    let specs = parse_tasks(&text);
    if specs.is_empty() && ends_at.is_some() {
        return Err(format!("{}: no tasks (a `## title` section each)", tasks_file.display()));
    }
    for s in &specs {
        if let Some(r) = &s.repo {
            if !expand(r).is_dir() {
                return Err(format!("task \"{}\": repo {r} is not a directory", s.title));
            }
        }
    }
    let agent = req.agent.clone().unwrap_or_else(|| cfg.xtreme.agent.clone());
    if !matches!(agent.as_str(), "claude" | "opencode") {
        return Err(format!("agent {agent}: claude or opencode"));
    }
    let agent_bin = req.agent_bin.clone().filter(|b| Path::new(b).is_file()).or_else(|| crate::config::which(&agent).map(|p| p.display().to_string()));
    let Some(agent_bin) = agent_bin else { return Err(format!("{agent} not found on PATH")) };
    let model = pick_model(&mgr, &cfg, req.model.as_deref())?;
    let user = user_name();
    if !Path::new(&format!("/var/lib/systemd/linger/{user}")).exists() {
        return Err(format!("linger is off: run `loginctl enable-linger {user}` (lmdeck must outlive the desktop session)"));
    }
    let (ac, charge) = power();
    if req.away && !ac {
        return Err("away mode needs the charger: plug it in".into());
    }
    if !ac && charge.is_some_and(|c| c < cfg.xtreme.min_battery as u64) {
        return Err(format!("on battery at {}%: plug in (xtreme.min_battery = {}%)", charge.unwrap_or(0), cfg.xtreme.min_battery));
    }
    if !req.dry_run {
        if !Path::new(HELPER).exists() {
            return Err(format!("{HELPER} is not installed (docs/XTREME.md, one-time setup)"));
        }
        let mut need = vec!["desktop-stop", "desktop-start", "screen-off", "screen-on", "wall"];
        if req.away {
            need.extend(["remote-on", "remote-off", "watchdog-on", "watchdog-off"]);
        }
        if let Some(missing) = need.iter().find(|a| !helper_allowed(a)) {
            return Err(format!("sudo does not allow `{HELPER} {missing}` without a password: install the sudoers rule from xtreme/ (docs/XTREME.md)"));
        }
        if req.away && !Path::new("/usr/bin/tailscale").exists() {
            return Err("away mode needs Tailscale (docs/XTREME.md, away mode setup)".into());
        }
    }
    let screen = req.screen.clone().unwrap_or_else(|| cfg.xtreme.screen.clone());
    let reserve = req.reserve_gib.unwrap_or(cfg.xtreme.reserve_gib);
    let st = State {
        phase: Phase::Starting,
        boot_id: boot_id(),
        started: t,
        ends_at,
        away: req.away,
        dry_run: req.dry_run,
        model: model.clone(),
        tasks_file: tasks_file.display().to_string(),
        agent,
        agent_bin,
        path_env: req.path_env.clone().unwrap_or_else(|| std::env::var("PATH").unwrap_or_default()),
        screen: "on".into(),
        screen_start: screen,
        ..Default::default()
    };
    {
        let mut cur = XT.st.lock().unwrap();
        *cur = st;
        merge_tasks(&mut cur, &specs);
        cur.note(format!(
            "run started: {model}, {} task(s){}{}",
            specs.len(),
            ends_at.map(|e| format!(", until {}", clock(e))).unwrap_or_else(|| ", no end".into()),
            if req.dry_run { " (dry run)" } else if req.away { " (away)" } else { "" }
        ));
        save(&cur);
    }
    RESERVE.store((reserve.max(0.0) * (1u64 << 30) as f64) as u64, Ordering::Relaxed);
    EVENTS.bump();
    spawn_driver(mgr);
    Ok(view())
}

/// Bring the task list in line with the file: new tasks join as pending, statuses are kept, the
/// file's order wins; a task gone from the file is dropped unless it ran.
fn merge_tasks(st: &mut State, specs: &[TaskSpec]) {
    let old = std::mem::take(&mut st.tasks);
    for s in specs {
        let mut t = old.iter().find(|t| t.slug == s.slug).cloned().unwrap_or_else(|| TaskState {
            slug: s.slug.clone(),
            status: "pending".into(),
            ..Default::default()
        });
        t.title = s.title.clone();
        t.check = s.check.clone();
        if t.repo.is_none() {
            t.repo = s.repo.clone();
        }
        st.tasks.push(t);
    }
    for t in old {
        if t.sessions > 0 && !st.tasks.iter().any(|n| n.slug == t.slug) {
            st.tasks.push(t);
        }
    }
}

/// When the daemon starts: carry on with a run the last daemon left (state.json), or end it if
/// the machine rebooted (a reboot is a way back to the desktop), except in away mode, which comes
/// back by itself.
pub fn resume(mgr: Arc<Manager>) {
    let st = XT.state();
    if !st.phase.active() {
        return;
    }
    let rebooted = st.boot_id != boot_id();
    if rebooted && !st.away {
        XT.with(|s| {
            s.note("the machine rebooted: the run ends (the desktop came back with the boot)");
            s.desktop_stopped = false;
            s.screen = "on".into();
            s.session = None;
            s.phase = Phase::Returning;
            s.end_reason = "reboot".into();
        });
    } else {
        XT.with(|s| {
            if let Some(sess) = s.session.take() {
                if let Some(t) = s.task_mut(&sess.slug) {
                    t.status = "pending".into();
                }
                s.note(format!("lmdeck restarted: session {} of {} was cut short", sess.n, sess.slug));
            }
            s.paused = false;
            if rebooted {
                s.note("the machine rebooted: away mode resumes");
                s.boot_id = boot_id();
                s.phase = Phase::Starting;
                s.screen = "on".into();
                s.remote = false;
                s.watchdog = false;
            } else if matches!(s.phase, Phase::Running | Phase::Loading | Phase::WindDown) {
                s.phase = Phase::Loading;
            }
        });
        let cfg = mgr.cfg();
        RESERVE.store((cfg.xtreme.reserve_gib.max(0.0) * (1u64 << 30) as f64) as u64, Ordering::Relaxed);
    }
    spawn_driver(mgr);
}

fn spawn_driver(mgr: Arc<Manager>) {
    if XT.driving.swap(true, Ordering::SeqCst) {
        XT.wake.notify_one();
        return;
    }
    tokio::spawn(async move {
        drive(mgr).await;
        XT.driving.store(false, Ordering::SeqCst);
    });
}

// ---------- requests from the wall and the CLI ----------

pub fn control(mgr: Arc<Manager>, action: &str, b: &Value) -> Result<Value, String> {
    let st = XT.state();
    if action == "add" {
        return add_task(&mgr.cfg(), &st, b["text"].as_str().unwrap_or(""), b["repo"].as_str());
    }
    if !st.phase.active() {
        return Err("no XTreme run (lmdeck xtreme start)".into());
    }
    match action {
        "stop" => XT.with(|s| {
            if s.phase == Phase::Countdown || !s.stop_requested {
                s.note("stop asked: back to the desktop");
            }
            s.stop_requested = true;
            s.stop_at.get_or_insert(now());
            s.end_reason = "stopped".into();
        }),
        "pause" => XT.with(|s| {
            s.pause_requested = true;
            s.note("pause: holding after this turn");
        }),
        "resume" => XT.with(|s| {
            s.pause_requested = false;
            s.note("resumed");
        }),
        "skip" => XT.with(|s| {
            if s.session.is_some() {
                s.skip_requested = true;
                s.note("skip: ending this task's session");
            } else if let Some(slug) = s.tasks.iter().find(|t| !t.finished()).map(|t| t.slug.clone()) {
                if let Some(t) = s.task_mut(&slug) {
                    t.status = "skipped".into();
                }
                s.note(format!("skipped {slug}"));
            }
        }),
        "extend" => {
            let secs = b["secs"].as_u64().unwrap_or(3600);
            let base = mgr.cfg().base_url();
            XT.with(|s| {
                let end = s.ends_at.unwrap_or_else(now).max(now()) + secs;
                s.ends_at = Some(end);
                if s.phase == Phase::Countdown {
                    s.phase = Phase::Running;
                    s.countdown_until = None;
                }
                s.stop_requested = false;
                s.stop_at = None;
                s.note(format!("{} more: the desktop comes back at {}", span(secs), clock(end)));
                if !s.dry_run {
                    arm_deadman(&base, end.saturating_sub(now()) + DEADMAN_SECS);
                }
            })
        }
        "screen" => {
            let on = b["on"].as_bool().unwrap_or(st.screen != "on");
            let dry = st.dry_run;
            tokio::spawn(async move {
                if helper(dry, &[if on { "screen-on" } else { "screen-off" }]).await.is_ok() {
                    XT.with(|s| s.screen = if on { "on" } else { "off" }.into());
                }
            });
        }
        other => return Err(format!("unknown action {other}")),
    }
    XT.poke();
    Ok(view())
}

/// Append a task to the run's task list (or the configured one when no run is going).
fn add_task(cfg: &Config, st: &State, text: &str, repo: Option<&str>) -> Result<Value, String> {
    if text.trim().is_empty() {
        return Err("the task is empty".into());
    }
    let file = if st.phase.active() { PathBuf::from(&st.tasks_file) } else { tasks_path(cfg) };
    let _ = std::fs::create_dir_all(file.parent().unwrap_or(Path::new("/")));
    let mut cur = std::fs::read_to_string(&file).unwrap_or_default();
    if cur.is_empty() {
        cur.push_str("# XTreme tasks\n");
    }
    cur.push_str(&task_section(text, repo));
    std::fs::write(&file, cur).map_err(|e| format!("{}: {e}", file.display()))?;
    if st.phase.active() {
        XT.with(|s| s.note(format!("task added: {}", text.lines().next().unwrap_or("").trim())));
        XT.poke();
    }
    Ok(json!({"ok": true, "file": file}))
}

// ---------- the driver ----------

async fn drive(mgr: Arc<Manager>) {
    loop {
        let st = XT.state();
        match st.phase {
            Phase::Idle | Phase::Ended => return,
            Phase::Starting => enter(&mgr, &st).await,
            Phase::Loading => load(&mgr, &st).await,
            Phase::Returning => {
                go_back(&mgr).await;
                return;
            }
            _ => tick(&mgr, &st).await,
        }
        let _ = tokio::time::timeout(Duration::from_secs(1), XT.wake.notified()).await;
    }
}

/// The one-time steps: the sleep lock, the return timer, away mode's extras, then the desktop
/// off, the wall on its console and the panel off.
async fn enter(mgr: &Manager, st: &State) {
    let dry = st.dry_run;
    if !dry {
        let mut lock = XT.inhibit.lock().unwrap();
        if lock.is_none() {
            *lock = take_sleep_lock();
        }
    }
    if let (Some(end), false) = (st.ends_at, dry) {
        arm_deadman(&mgr.cfg().base_url(), end.saturating_sub(now()) + DEADMAN_SECS);
    }
    if st.away {
        let ok = |r: Result<(), String>| r.map_err(|e| XT.with(|s| s.note(format!("away: {e}")))).is_ok();
        let lim = ok(helper(dry, &["battery-limit", "80"]).await);
        let wd = ok(helper(dry, &["watchdog-on"]).await);
        let rem = ok(helper(dry, &["remote-on"]).await);
        XT.with(|s| {
            s.battery_limited = lim;
            s.watchdog = wd;
            s.remote = rem;
        });
    }
    // Let the CLI that started the run print its last lines before the session goes.
    tokio::time::sleep(Duration::from_secs(3)).await;
    if !st.desktop_stopped {
        // After a reboot in away mode, SDDM's autologin is still on its way up.
        if st.away {
            for _ in 0..60 {
                let up = std::process::Command::new("systemctl").args(["is-active", "--quiet", "sddm.service"]).status().is_ok_and(|s| s.success());
                if up || dry {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
        if let Err(e) = helper(dry, &["desktop-stop"]).await {
            XT.with(|s| {
                s.note(format!("could not stop the desktop: {e}"));
                s.end_reason = "the desktop did not stop".into();
                s.phase = Phase::Returning;
            });
            return;
        }
        XT.with(|s| {
            s.desktop_stopped = true;
            s.note(if dry { "dry run: the desktop stays" } else { "desktop stopped" });
        });
    }
    if let Err(e) = helper(dry, &["wall"]).await {
        XT.with(|s| s.note(format!("the wall did not open ({e}); Ctrl+Alt+F2 and `lmdeck xtreme` show it")));
    }
    if st.screen_start == "off" && helper(dry, &["screen-off"]).await.is_ok() {
        XT.with(|s| s.screen = "off".into());
    }
    XT.with(|s| s.phase = Phase::Loading);
}

/// Unload what the busy desktop sized, wait for the freed memory to settle, load the model.
async fn load(mgr: &Manager, st: &State) {
    let cfg = mgr.cfg();
    THINKING_BUDGET.store(cfg.xtreme.thinking_budget, Ordering::Relaxed);
    if st.stop_requested {
        XT.with(|s| s.phase = Phase::Running);
        return;
    }
    if st.dry_run {
        XT.with(|s| {
            s.note("dry run: the model is not loaded");
            s.phase = Phase::Running;
        });
        return;
    }
    if mgr.running().iter().any(|r| r.name == st.model) && st.arena_gb.is_some() {
        XT.with(|s| s.phase = Phase::Running);
        return;
    }
    mgr.unload_all().await;
    // Stopped apps hand their memory back over a few seconds.
    let mut last = sys::mem().1;
    for _ in 0..10 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let a = sys::mem().1;
        if a.abs_diff(last) < (256 << 20) {
            break;
        }
        last = a;
    }
    XT.with(|s| s.note(format!("loading {} with {:.1} GiB free", s.model, sys::mem().1 as f64 / (1u64 << 30) as f64)));
    match mgr.acquire(&st.model, Some("-1")).await {
        Ok((_guard, secs)) => {
            let arena = mgr.resolve(&st.model).ok().and_then(|m| arena_gb(&m));
            XT.with(|s| {
                s.load_secs = Some(secs);
                s.arena_gb = arena;
                s.note(format!("{} loaded in {secs:.0} s{}", s.model, arena.map(|a| format!(", expert arena {a:.2} GB")).unwrap_or_default()));
                s.phase = Phase::Running;
            });
        }
        Err(e) => {
            let e = format!("{e:#}");
            XT.with(|s| {
                s.note(format!("load failed: {}", e.chars().take(300).collect::<String>()));
                s.end_reason = "the model did not load".into();
                s.phase = Phase::Returning;
            });
        }
    }
}

/// The engine log a Strata model's run config names.
fn engine_log(m: &crate::models::LocalModel) -> Option<PathBuf> {
    let v: Value = serde_json::from_slice(&std::fs::read(&m.path).ok()?).ok()?;
    v["log"].as_str().filter(|l| !l.is_empty()).map(PathBuf::from)
}

/// "strata metal stream: expert arena 3.85 GB, ..." — the last load's arena.
pub fn arena_gb(m: &crate::models::LocalModel) -> Option<f64> {
    parse_arena(&sys::read_tail(&engine_log(m)?, 512 * 1024))
}

pub fn parse_arena(text: &str) -> Option<f64> {
    let l = text.lines().rev().find(|l| l.contains("expert arena "))?;
    l.split("expert arena ").nth(1)?.split_whitespace().next()?.parse().ok()
}

/// Decode speed of the last reply in a Strata backend log ("... 5.9 tok/s, 367 s").
pub fn parse_decode(text: &str) -> Option<f64> {
    let l = text.lines().rev().find(|l| l.contains("[strata] ") && l.contains(" tok/s") && !l.contains("reading the prompt"))?;
    let head = l.split(" tok/s").next()?;
    head.rsplit(['(', ' ']).next()?.parse().ok()
}

async fn tick(mgr: &Arc<Manager>, st: &State) {
    let cfg = mgr.cfg();
    let t = now();
    if st.stop_requested && st.phase != Phase::Returning {
        // The agent finishes its turn first (a request in flight is left to end), at most 2 min.
        if st.session.is_some() {
            if mgr.busy() && t < st.stop_at.unwrap_or(t) + 120 {
                return;
            }
            end_session(mgr, "the run stopped").await;
        }
        XT.with(|s| s.phase = Phase::Returning);
        return;
    }
    // The end: countdown, then back.
    if let Some(end) = st.ends_at {
        if st.phase == Phase::Countdown {
            if t >= st.countdown_until.unwrap_or(t) {
                XT.with(|s| {
                    s.stop_requested = true;
                    s.stop_at = Some(t);
                    s.end_reason = "time".into();
                });
            }
            return;
        }
        if t >= end {
            let dry = st.dry_run;
            XT.with(|s| {
                s.phase = Phase::Countdown;
                s.countdown_until = Some(t + COUNTDOWN_SECS);
                s.note("the end: back to the desktop in 60 s (a key adds an hour)");
            });
            if helper(dry, &["screen-on"]).await.is_ok() {
                XT.with(|s| s.screen = "on".into());
            }
            return;
        }
        // A short run keeps three quarters of its time for work.
        let wind = (cfg.xtreme.wind_down_minutes as u64 * 60).min(end.saturating_sub(st.started) / 4);
        if st.phase == Phase::Running && t + wind >= end {
            XT.with(|s| {
                s.phase = Phase::WindDown;
                s.note("winding down: no new session");
            });
        }
    }
    if st.session.is_some() {
        watch_session(mgr, st).await;
        return;
    }
    if st.pause_requested {
        if !st.paused {
            XT.with(|s| s.paused = true);
        }
        return;
    }
    if st.paused {
        XT.with(|s| s.paused = false);
    }
    if st.phase == Phase::WindDown {
        return;
    }
    // Between sessions: the file again (add, reorder, reword freely).
    let specs = parse_tasks(&std::fs::read_to_string(&st.tasks_file).unwrap_or_default());
    let mut merged = st.clone();
    merge_tasks(&mut merged, &specs);
    if merged.tasks != st.tasks {
        XT.with(|s| merge_tasks(s, &specs));
    }
    let st = XT.state();
    let Some(next) = st.tasks.iter().find(|t| !t.finished()).cloned() else {
        if st.ends_at.is_some() {
            // A timed run with everything done gives the machine back early.
            XT.with(|s| {
                s.note("every task has ended");
                s.stop_requested = true;
                s.stop_at = Some(now());
                s.end_reason = "all tasks ended".into();
            });
        } else if st.phase != Phase::Waiting {
            mgr.unload_all().await;
            XT.with(|s| {
                s.arena_gb = None;
                s.phase = Phase::Waiting;
                s.note("no task left: the model is unloaded until one is added (lmdeck xtreme add)");
            });
        }
        return;
    };
    // The engine died, or the run waited with it unloaded: load it again first.
    if !st.dry_run && !mgr.running().iter().any(|r| r.name == st.model) {
        XT.with(|s| {
            if s.phase != Phase::Waiting {
                s.note(format!("{} is not loaded: loading it again", s.model));
            }
            s.arena_gb = None;
            s.phase = Phase::Loading;
        });
        return;
    }
    if st.phase == Phase::Waiting {
        XT.with(|s| s.phase = Phase::Running);
    }
    let spec = specs.into_iter().find(|s| s.slug == next.slug).unwrap_or_default();
    if let Err(e) = start_session(&cfg, &st, &next, &spec).await {
        XT.with(|s| {
            if let Some(t) = s.task_mut(&next.slug) {
                t.status = "failed".into();
                t.note = e.clone();
            }
            s.note(format!("{}: {e}", next.slug));
        });
    }
}

// ---------- sessions ----------

async fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let o = tokio::process::Command::new("git").arg("-C").arg(dir).args(args).output().await.map_err(|e| format!("git: {e}"))?;
    if o.status.success() {
        Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
    } else {
        let err = String::from_utf8_lossy(&o.stderr);
        Err(format!("git {}: {}", args.join(" "), err.trim().lines().last().unwrap_or("")))
    }
}

/// The task's own branch in a worktree next to the run's state, so the agent never touches the
/// checkout (or the uncommitted work) the user has open. A repository that is not git is worked
/// in place; a task without one gets a scratch directory.
async fn workspace(t: &TaskState) -> Result<TaskState, String> {
    let mut t = t.clone();
    if t.worktree.as_deref().is_some_and(|w| Path::new(w).is_dir()) {
        return Ok(t);
    }
    let Some(repo) = t.repo.as_deref().map(expand) else {
        std::fs::create_dir_all(task_dir(&t.slug).join("work")).map_err(|e| e.to_string())?;
        return Ok(t);
    };
    if git(&repo, &["rev-parse", "--git-dir"]).await.is_err() {
        return Ok(t);
    }
    let top = PathBuf::from(git(&repo, &["rev-parse", "--show-toplevel"]).await?);
    let branch = format!("xtreme/{}", t.slug);
    let wt = worktree_path(&t.slug);
    let _ = std::fs::create_dir_all(wt.parent().unwrap());
    let wt_s = wt.display().to_string();
    let exists = git(&top, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).await.is_ok();
    let base = git(&top, &["rev-parse", "HEAD"]).await?;
    // The branch may already have a worktree from an earlier run: carry on there.
    let list = git(&top, &["worktree", "list", "--porcelain"]).await.unwrap_or_default();
    let have = list.split("\n\n").find(|w| w.lines().any(|l| l == format!("branch refs/heads/{branch}"))).and_then(|w| w.lines().next()?.strip_prefix("worktree ").map(str::to_string));
    let wt_s = match have {
        Some(p) if Path::new(&p).is_dir() => p,
        _ => {
            let _ = git(&top, &["worktree", "prune"]).await;
            if exists {
                git(&top, &["worktree", "add", &wt_s, &branch]).await?;
            } else {
                git(&top, &["worktree", "add", "-b", &branch, &wt_s, &base]).await?;
            }
            wt_s
        }
    };
    if t.base.is_none() {
        t.base = Some(if exists { git(&top, &["merge-base", "HEAD", &branch]).await.unwrap_or(base) } else { base });
    }
    t.worktree = Some(wt_s);
    t.branch = Some(branch);
    Ok(t)
}

async fn commit_count(t: &TaskState) -> u32 {
    let (Some(wt), Some(base)) = (&t.worktree, &t.base) else { return 0 };
    git(Path::new(wt), &["rev-list", "--count", &format!("{base}..HEAD")]).await.ok().and_then(|n| n.parse().ok()).unwrap_or(0)
}

/// The rules every session gets, the same text each time (part of the shared prefix).
pub const RULES: &str = "You are working unattended in lmdeck's XTreme mode: nobody reads your messages until the \
morning, so never ask a question; decide, and write down why. Each session is fresh and bounded: what earlier sessions \
did is in the task's notes file and in the git history, nowhere else.\n\
Rules:\n\
- Work only in the directory you are started in. In a git repository you are on the task's own branch: commit each \
step that works (git add -A && git commit -m ...). Never push, never switch branches, never rewrite history.\n\
- Run the task's check when there is one; a task is done only when it passes.\n\
- Before you stop, append to the notes file, under a heading `## Session N`: what you did, what you learned, what is \
left, in a few short lines.\n\
- When the task is done (and the check passes), add the line `STATUS: done` to the notes. If it cannot be done, add \
`STATUS: blocked: <why>`.\n\
- Keep thinking short and turns small. Stop early rather than run out of turns or time.";

pub fn session_prompt(spec: &TaskSpec, t: &TaskState, n: u32, turns: u32, minutes: u32, notes: &str) -> String {
    let mut p = format!("TASK: {}\n\n{}\n\n", spec.title, if spec.body.is_empty() { "(no details: the title is the task)" } else { &spec.body });
    p.push_str(&format!("WHERE: {}", t.workdir().display()));
    if let (Some(b), Some(base)) = (&t.branch, &t.base) {
        p.push_str(&format!(" (git branch {b}, based on {})", &base[..base.len().min(10)]));
    }
    p.push('\n');
    if let Some(c) = &t.check {
        p.push_str(&format!("CHECK: {c}\n"));
    }
    p.push_str(&format!("SESSION: {n}, at most {turns} turns and {minutes} minutes\n"));
    p.push_str(&format!("NOTES: {}\n", progress_path(&t.slug).display()));
    let notes = notes.trim();
    if notes.is_empty() {
        p.push_str("\nThe notes are empty: this is the first session.\n");
    } else {
        // The end of the notes is what the last session left; keep the prompt bounded.
        let mut cut = notes.len().saturating_sub(6000);
        while !notes.is_char_boundary(cut) {
            cut += 1;
        }
        let tail = if cut > 0 { format!("...\n{}", &notes[cut..]) } else { notes.to_string() };
        p.push_str(&format!("\nThe notes so far:\n{tail}\n"));
    }
    p
}

async fn start_session(cfg: &Config, st: &State, t: &TaskState, spec: &TaskSpec) -> Result<(), String> {
    let t = workspace(t).await?;
    let slug = t.slug.clone();
    let n = t.sessions + 1;
    let tdir = task_dir(&slug);
    std::fs::create_dir_all(&tdir).map_err(|e| e.to_string())?;
    let notes_p = progress_path(&slug);
    if !notes_p.exists() {
        let _ = std::fs::write(&notes_p, format!("# {}\n\n", spec.title));
    }
    let notes = std::fs::read_to_string(&notes_p).unwrap_or_default();
    let turns = spec.turns.unwrap_or(cfg.xtreme.max_turns);
    let minutes = spec.minutes.unwrap_or(cfg.xtreme.session_minutes);
    let prompt = session_prompt(spec, &t, n, turns, minutes, &notes);
    let log = tdir.join(format!("session-{n}.log"));
    let raw = tdir.join(format!("session-{n}.jsonl"));
    let mut cmd = tokio::process::Command::new(&st.agent_bin);
    cmd.current_dir(t.workdir()).env("PATH", &st.path_env).env_remove("ANTHROPIC_API_KEY");
    match st.agent.as_str() {
        "opencode" => {
            cmd.env("OPENCODE_CONFIG_CONTENT", crate::launch::opencode_config(cfg, &st.model).to_string())
                .args(["run", "--auto", "--format", "json", "-m", &format!("lmdeck/{}", st.model)])
                .args(&cfg.xtreme.agent_args)
                .arg(format!("{RULES}\n\n{prompt}"));
        }
        _ => {
            cmd.envs(crate::launch::claude_env(cfg, &st.model))
                .args(["-p", &prompt, "--output-format", "stream-json", "--verbose", "--dangerously-skip-permissions"])
                .args(["--max-turns", &turns.to_string(), "--append-system-prompt", RULES])
                .args(&cfg.xtreme.agent_args);
        }
    }
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).process_group(0);
    let mut child = cmd.spawn().map_err(|e| format!("{}: {e}", st.agent_bin))?;
    let pid = child.id();
    let (out, err) = (child.stdout.take(), child.stderr.take());
    *XT.child.lock().unwrap() = Some(child);
    let commits = commit_count(&t).await;
    let progress_len = notes.len() as u64;
    let t2 = t.clone();
    XT.with(|s| {
        if let Some(cur) = s.task_mut(&slug) {
            *cur = TaskState { status: "running".into(), sessions: n, commits, ..t2 };
        }
        s.sessions += 1;
        s.session = Some(Session {
            slug: slug.clone(),
            n,
            started: now(),
            deadline: now() + minutes as u64 * 60,
            turns: 0,
            max_turns: turns,
            pid,
            log: log.display().to_string(),
            last_tool: String::new(),
            progress_len,
            commits_before: commits,
            paused_at: None,
        });
        s.note(format!("#{} {}: session {n}", s.tasks.iter().position(|x| x.slug == slug).map(|i| i + 1).unwrap_or(0), spec.title));
    });
    let _ = std::fs::write(&log, format!("== {} session {n} of {}\n", sys::now_rfc3339(), spec.title));
    if let Some(out) = out {
        tokio::spawn(read_transcript(out, raw, log.clone()));
    }
    if let Some(err) = err {
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut lines = tokio::io::BufReader::new(err).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                append(&log, &format!("stderr: {l}\n"));
            }
        });
    }
    Ok(())
}

/// Paths under $HOME shown with ~.
fn tilde(s: &str) -> String {
    let home = crate::config::home().display().to_string();
    s.replace(&home, "~")
}

fn append(p: &Path, s: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
        let _ = f.write_all(s.as_bytes());
    }
}

/// The agent's output: kept raw (session-N.jsonl), and as readable lines (session-N.log) that the
/// wall's LOG pane and `l` show; turns and the last tool go to the run state.
async fn read_transcript(out: tokio::process::ChildStdout, raw: PathBuf, log: PathBuf) {
    use tokio::io::AsyncBufReadExt;
    let mut lines = tokio::io::BufReader::new(out).lines();
    let mut seen_ids = std::collections::HashSet::new();
    while let Ok(Some(l)) = lines.next_line().await {
        append(&raw, &format!("{l}\n"));
        let Ok(v) = serde_json::from_str::<Value>(&l) else {
            append(&log, &format!("{l}\n"));
            continue;
        };
        let ev = transcript_event(&v);
        if let Some(text) = &ev.text {
            append(&log, &format!("{}\n", text));
        }
        let new_turn = ev.message_id.as_ref().is_some_and(|id| seen_ids.insert(id.clone()));
        if new_turn || ev.tool.is_some() {
            XT.with(|s| {
                if new_turn {
                    s.turns += 1;
                    if let Some(sess) = s.session.as_mut() {
                        sess.turns += 1;
                    }
                }
                if let Some(tool) = &ev.tool {
                    if let Some(sess) = s.session.as_mut() {
                        sess.last_tool = tool.clone();
                    }
                    s.log.push(format!("{} {tool}", clock(now())));
                    let n = s.log.len();
                    if n > 200 {
                        s.log.drain(..n - 200);
                    }
                }
            });
        }
    }
}

#[derive(Default, Debug, PartialEq)]
pub struct Event {
    /// A readable line for session-N.log.
    pub text: Option<String>,
    /// A tool call, "bash  cargo test".
    pub tool: Option<String>,
    /// An assistant message (one per turn).
    pub message_id: Option<String>,
}

/// One line of Claude Code's stream-json (opencode's json is kept as it comes).
pub fn transcript_event(v: &Value) -> Event {
    let mut e = Event::default();
    match v["type"].as_str() {
        Some("assistant") => {
            e.message_id = v["message"]["id"].as_str().map(str::to_string);
            let mut parts = Vec::new();
            for c in v["message"]["content"].as_array().into_iter().flatten() {
                match c["type"].as_str() {
                    Some("text") => parts.push(c["text"].as_str().unwrap_or("").trim().to_string()),
                    Some("tool_use") => {
                        let name = c["name"].as_str().unwrap_or("tool");
                        let i = &c["input"];
                        let arg = ["command", "file_path", "path", "pattern", "url", "description"]
                            .iter()
                            .find_map(|k| i[*k].as_str())
                            .unwrap_or("")
                            .lines()
                            .next()
                            .unwrap_or("")
                            .to_string();
                        let arg: String = arg.chars().take(100).collect();
                        let line = format!("{:<10} {}", name.to_lowercase(), tilde(&arg));
                        parts.push(format!("> {line}"));
                        e.tool = Some(line);
                    }
                    _ => {}
                }
            }
            let text = parts.into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>().join("\n");
            e.text = Some(text).filter(|t| !t.is_empty());
        }
        Some("user") => {
            for c in v["message"]["content"].as_array().into_iter().flatten() {
                if c["type"] == "tool_result" {
                    let body = match &c["content"] {
                        Value::String(s) => s.clone(),
                        Value::Array(a) => a.iter().filter_map(|x| x["text"].as_str()).collect::<Vec<_>>().join("\n"),
                        _ => String::new(),
                    };
                    let lines: Vec<&str> = body.lines().filter(|l| !l.trim().is_empty()).collect();
                    let shown = lines.iter().rev().take(3).rev().map(|l| format!("  {}", l.chars().take(160).collect::<String>())).collect::<Vec<_>>();
                    e.text = Some(format!("  -> {} line(s){}{}", lines.len(), if shown.is_empty() { "" } else { "\n" }, shown.join("\n")));
                }
            }
        }
        Some("result") => {
            e.text = Some(format!(
                "== result: {} after {} turn(s), {:.0} s",
                v["subtype"].as_str().unwrap_or("?"),
                v["num_turns"].as_u64().unwrap_or(0),
                v["duration_ms"].as_f64().unwrap_or(0.0) / 1000.0
            ));
        }
        _ => {}
    }
    e
}

fn signal_group(pid: Option<u32>, sig: i32) {
    if let Some(p) = pid {
        unsafe { libc::killpg(p as i32, sig) };
    }
}

/// While a session runs: its end, its time limit, pause, skip.
async fn watch_session(mgr: &Manager, st: &State) {
    let Some(sess) = st.session.clone() else { return };
    let exited = XT.child.lock().unwrap().as_mut().map(|c| c.try_wait().ok().flatten().is_some()).unwrap_or(true);
    if exited {
        finish_session(mgr.cfg(), None).await;
        return;
    }
    let t = now();
    if st.skip_requested {
        end_session(mgr, "skipped").await;
        return;
    }
    // Pause between model requests (the agent is running a tool or about to send the next turn).
    if st.pause_requested && !st.paused && !mgr.busy() {
        signal_group(sess.pid, libc::SIGSTOP);
        XT.with(|s| {
            s.paused = true;
            if let Some(x) = s.session.as_mut() {
                x.paused_at = Some(t);
            }
            s.note("paused (the model stays loaded)");
        });
        return;
    }
    if !st.pause_requested && st.paused {
        signal_group(sess.pid, libc::SIGCONT);
        XT.with(|s| {
            s.paused = false;
            if let Some(x) = s.session.as_mut() {
                // Paused time does not count against the session.
                x.deadline += t.saturating_sub(x.paused_at.take().unwrap_or(t));
            }
        });
        return;
    }
    if !st.paused && t >= sess.deadline {
        end_session(mgr, "out of time").await;
    }
}

/// End the session in progress: SIGTERM to the agent's process group, SIGKILL after 10 s.
async fn end_session(mgr: &Manager, why: &str) {
    let pid = XT.state().session.and_then(|s| s.pid);
    // Only while the child is not reaped: its pid (the group's id) cannot be reused before that.
    let gone = || XT.child.lock().unwrap().as_mut().map(|c| c.try_wait().ok().flatten().is_some()).unwrap_or(true);
    if !gone() {
        signal_group(pid, libc::SIGCONT);
        signal_group(pid, libc::SIGTERM);
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if gone() {
                break;
            }
        }
        if !gone() {
            signal_group(pid, libc::SIGKILL);
        }
    }
    finish_session(mgr.cfg(), Some(why)).await;
}

/// The last `STATUS:` line of the notes: ("done" | "blocked", detail).
pub fn notes_status(notes: &str) -> Option<(String, String)> {
    let l = notes.lines().rev().find(|l| l.trim_start().trim_start_matches(['-', '*', ' ']).to_ascii_uppercase().starts_with("STATUS:"))?;
    let rest = l.split_once(':').map(|x| x.1).unwrap_or("").trim();
    let word = rest.split([':', ' ']).next().unwrap_or("").to_ascii_lowercase();
    let detail = rest[word.len().min(rest.len())..].trim_start_matches([':', ' ']).trim();
    Some((word, detail.to_string()))
}

async fn run_check(cmd: &str, dir: &Path, minutes: u32, path_env: &str) -> (bool, String) {
    let child = tokio::process::Command::new("nice")
        .args(["-n", "10", "sh", "-c", cmd])
        .current_dir(dir)
        .env("PATH", path_env)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(Duration::from_secs(minutes as u64 * 60), child).await {
        Ok(Ok(o)) => {
            let text = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
            let lines: Vec<&str> = text.lines().collect();
            (o.status.success(), lines[lines.len().saturating_sub(15)..].join("\n"))
        }
        Ok(Err(e)) => (false, e.to_string()),
        Err(_) => (false, format!("timed out after {minutes} min")),
    }
}

/// After a session: what it left (commits, notes, STATUS), the check, and whether the task goes on.
async fn finish_session(cfg: Config, why: Option<&str>) {
    let st = XT.state();
    let Some(sess) = st.session.clone() else { return };
    XT.child.lock().unwrap().take();
    let Some(t) = st.task(&sess.slug).cloned() else {
        XT.with(|s| s.session = None);
        return;
    };
    let commits = commit_count(&t).await;
    let notes = std::fs::read_to_string(progress_path(&t.slug)).unwrap_or_default();
    let noted = notes.len() as u64 > sess.progress_len;
    let status = notes_status(&notes);
    let claims_done = status.as_ref().is_some_and(|(w, _)| w == "done");
    let mut check = None;
    if let Some(c) = &t.check {
        if claims_done || commits > sess.commits_before {
            check = Some(run_check(c, &t.workdir(), cfg.xtreme.check_minutes, &st.path_env).await);
        }
    }
    if let (true, Some((false, tail))) = (claims_done, &check) {
        append(&progress_path(&t.slug), &format!("\n- lmdeck: the check failed after session {}, so the task is not done:\n```\n{tail}\n```\n", sess.n));
    }
    // A session the run itself cut short says nothing about the task.
    let cut_short = matches!(why, Some("the run stopped" | "the run ended"));
    let made_progress = noted || commits > sess.commits_before || cut_short;
    XT.with(|s| {
        let max = cfg.xtreme.max_sessions;
        let skip = s.skip_requested;
        s.skip_requested = false;
        s.session = None;
        s.paused = false;
        let Some(task) = s.task_mut(&t.slug) else { return };
        task.commits = commits;
        if let Some((ok, tail)) = &check {
            task.check_ok = Some(*ok);
            task.check_tail = tail.clone();
        }
        task.idle = if made_progress { 0 } else { task.idle + 1 };
        let (status, note) = match &status {
            _ if skip => ("skipped".to_string(), "skipped by hand".to_string()),
            Some((w, d)) if w == "done" && check.as_ref().is_none_or(|c| c.0) => ("done".into(), d.clone()),
            Some((w, d)) if w == "blocked" => ("blocked".into(), d.clone()),
            _ if task.idle >= 2 => ("stuck".into(), "two sessions in a row without a commit or a note".into()),
            _ if task.sessions >= max => ("out of sessions".into(), format!("{max} sessions")),
            _ => ("pending".into(), why.map(str::to_string).unwrap_or_default()),
        };
        task.status = status.clone();
        task.note = note;
        let title = task.title.clone();
        s.note(format!(
            "{title}: session {} ended{}, {} commit(s){} -> {status}",
            sess.n,
            why.map(|w| format!(" ({w})")).unwrap_or_default(),
            commits,
            check.as_ref().map(|c| if c.0 { ", check passed" } else { ", check failed" }).unwrap_or("")
        ));
    });
    write_report(&XT.state());
}

// ---------- back to the desktop ----------

async fn go_back(mgr: &Manager) {
    let st = XT.state();
    if st.session.is_some() {
        end_session(mgr, "the run ended").await;
    }
    let dry = st.dry_run;
    XT.with(|s| s.note("returning to the desktop"));
    THINKING_BUDGET.store(0, Ordering::Relaxed);
    RESERVE.store(0, Ordering::Relaxed);
    // Its arena was sized for an empty machine; the desktop needs that memory back.
    if !dry {
        mgr.unload_all().await;
    }
    let _ = helper(dry, &["screen-on"]).await;
    if st.away {
        if st.remote {
            let _ = helper(dry, &["remote-off"]).await;
        }
        if st.watchdog {
            let _ = helper(dry, &["watchdog-off"]).await;
        }
    }
    if st.battery_limited {
        let _ = helper(dry, &["battery-limit", "100"]).await;
    }
    if st.desktop_stopped {
        if let Err(e) = helper(dry, &["desktop-start"]).await {
            XT.with(|s| s.note(format!("the desktop did not start: {e}")));
        }
    }
    if let Some(mut c) = XT.inhibit.lock().unwrap().take() {
        let _ = c.kill();
        let _ = c.wait();
    }
    if !dry {
        disarm_deadman();
    }
    XT.with(|s| {
        s.phase = Phase::Ended;
        s.ended = Some(now());
        s.desktop_stopped = false;
        s.screen = "on".into();
        s.battery_limited = false;
        s.remote = false;
        s.watchdog = false;
        s.stop_requested = false;
        s.stop_at = None;
        s.pause_requested = false;
        s.paused = false;
        s.note("run ended");
    });
    let st = XT.state();
    write_report(&st);
    if !dry && st.end_reason != "reboot" {
        tokio::task::spawn_blocking(move || notify_report(&st));
    }
}

/// The night's result on the desktop once its notification daemon is up; "Open" opens the report.
fn notify_report(st: &State) {
    for _ in 0..90 {
        let up = std::process::Command::new("busctl")
            .args(["--user", "status", "org.freedesktop.Notifications"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if up {
            break;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    let o = std::process::Command::new("notify-send")
        .args(["-a", "lmdeck", "-A", "open=Open the report", "--wait", "XTreme run ended", &summary_line(st)])
        .output();
    if o.is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "open") {
        let _ = std::process::Command::new("systemd-run").args(["--user", "--quiet", "xdg-open"]).arg(report_path()).status();
    }
}

pub fn summary_line(st: &State) -> String {
    let done = st.tasks.iter().filter(|t| t.status == "done").count();
    let commits: u32 = st.tasks.iter().map(|t| t.commits).sum();
    let secs = st.ended.unwrap_or_else(now).saturating_sub(st.started);
    format!("{done} of {} tasks done, {commits} commit(s), {} session(s) in {}", st.tasks.len(), st.sessions, span(secs))
}

pub fn write_report(st: &State) {
    let _ = std::fs::write(report_path(), report(st));
}

pub fn report(st: &State) -> String {
    let mut r = format!("# XTreme run, {}\n\n", sys::rfc3339(std::time::UNIX_EPOCH + Duration::from_secs(st.started)));
    r.push_str(&format!("{}.\n\n", summary_line(st)));
    r.push_str(&format!("- model: {}{}\n", st.model, st.arena_gb.map(|a| format!(", expert arena {a:.2} GB")).unwrap_or_default()));
    if let Some(l) = st.load_secs {
        r.push_str(&format!("- load: {l:.0} s\n"));
    }
    r.push_str(&format!("- from {} to {}{}\n", clock(st.started), st.ended.map(clock).unwrap_or_else(|| "(running)".into()), if st.end_reason.is_empty() { String::new() } else { format!(" ({})", st.end_reason) }));
    r.push_str(&format!("- sessions {}, turns {}, agent {}\n", st.sessions, st.turns, st.agent));
    if st.dry_run {
        r.push_str("- dry run: the desktop, the screen and the model were left alone\n");
    }
    for (i, t) in st.tasks.iter().enumerate() {
        r.push_str(&format!("\n## {}. {} — {}\n\n", i + 1, t.title, t.status));
        if !t.note.is_empty() {
            r.push_str(&format!("{}\n\n", t.note));
        }
        r.push_str(&format!("- sessions: {}, commits: {}\n", t.sessions, t.commits));
        if let (Some(b), Some(base)) = (&t.branch, &t.base) {
            r.push_str(&format!("- branch `{b}` from `{}` (worktree {})\n", &base[..base.len().min(10)], t.worktree.as_deref().unwrap_or("-")));
            if let Some(wt) = &t.worktree {
                let o = std::process::Command::new("git").args(["-C", wt, "log", "--oneline", &format!("{base}..HEAD")]).output();
                if let Ok(o) = o {
                    for l in String::from_utf8_lossy(&o.stdout).lines().take(30) {
                        r.push_str(&format!("  - {l}\n"));
                    }
                }
            }
        }
        if let Some(c) = &t.check {
            r.push_str(&format!("- check `{c}`: {}\n", match t.check_ok { Some(true) => "passed", Some(false) => "failed", None => "not run" }));
            if t.check_ok == Some(false) && !t.check_tail.is_empty() {
                r.push_str(&format!("```\n{}\n```\n", t.check_tail));
            }
        }
        let notes = std::fs::read_to_string(progress_path(&t.slug)).unwrap_or_default();
        let lines: Vec<&str> = notes.lines().filter(|l| !l.trim().is_empty()).collect();
        if lines.len() > 1 {
            r.push_str("- last notes:\n");
            for l in &lines[lines.len().saturating_sub(8)..] {
                r.push_str(&format!("  > {l}\n"));
            }
        }
    }
    r
}

// ---------- the view (GET /lmdeck/xtreme, and `xtreme` in /lmdeck/status) ----------

/// TCP ports something listens on beyond localhost (for the wall's header).
pub fn listening_ports() -> Vec<u16> {
    let mut ports = std::collections::BTreeSet::new();
    for f in ["/proc/net/tcp", "/proc/net/tcp6"] {
        for l in std::fs::read_to_string(f).unwrap_or_default().lines().skip(1) {
            let cols: Vec<&str> = l.split_whitespace().collect();
            if cols.len() < 4 || cols[3] != "0A" {
                continue;
            }
            let Some((addr, port)) = cols[1].split_once(':') else { continue };
            let loopback = addr == "0100007F" || addr == "00000000000000000000000001000000";
            if let (false, Ok(p)) = (loopback, u16::from_str_radix(port, 16)) {
                ports.insert(p);
            }
        }
    }
    ports.into_iter().collect()
}

pub fn view() -> Value {
    let st = XT.state();
    let t = now();
    let cur = st.current().cloned();
    let progress_tail: Vec<String> = cur
        .as_ref()
        .map(|c| {
            let notes = std::fs::read_to_string(progress_path(&c.slug)).unwrap_or_default();
            let lines: Vec<String> = notes.lines().filter(|l| !l.trim().is_empty() && !l.starts_with("# ")).map(str::to_string).collect();
            lines[lines.len().saturating_sub(12)..].to_vec()
        })
        .unwrap_or_default();
    let (ac, charge) = power();
    let mut state = serde_json::to_value(&st).unwrap_or_default();
    if let Some(l) = state["log"].as_array_mut() {
        let n = l.len();
        l.drain(..n.saturating_sub(40));
    }
    json!({
        "state": state,
        "phase_label": st.phase.label(),
        "active": st.phase.active(),
        "now": t,
        "elapsed_secs": if st.started == 0 { 0 } else if st.phase.active() { t.saturating_sub(st.started) } else { st.ended.unwrap_or(t).saturating_sub(st.started) },
        "remaining_secs": st.ends_at.map(|e| e.saturating_sub(t)),
        "countdown_secs": st.countdown_until.map(|c| c.saturating_sub(t)),
        "current": cur.map(|c| c.slug),
        "progress_tail": progress_tail,
        "ports": listening_ports(),
        "power": {"ac": ac, "charge": charge},
        "summary": summary_line(&st),
        // Before a run: the titles in the task list, for the wall and the TUI tab.
        "planned": if st.phase.active() { vec![] } else {
            parse_tasks(&std::fs::read_to_string(tasks_path(&Config::load().unwrap_or_default())).unwrap_or_default()).into_iter().map(|t| t.title).collect::<Vec<_>>()
        },
        "decode_tps": parse_decode(&sys::read_tail(&crate::config::log_dir().join(format!("{}.log", st.model.replace(['/', ':'], "_"))), 16 * 1024)),
        "tasks_file": if st.tasks_file.is_empty() { tasks_path(&Config::load().unwrap_or_default()).display().to_string() } else { st.tasks_file.clone() },
        "report": report_path(),
    })
}

/// Request hints while a run holds: Strata's thinking capped (unless the client set its own).
/// The reserve a load keeps free: the run's while one holds, else server.mem_reserve_gib.
pub fn reserve_bytes(cfg: &Config) -> u64 {
    match RESERVE.load(Ordering::Relaxed) {
        0 => (cfg.server.mem_reserve_gib * (1u64 << 30) as f64) as u64,
        r => r,
    }
}

pub fn request_hints(m: &crate::models::LocalModel, body: &mut Value) {
    let budget = THINKING_BUDGET.load(Ordering::Relaxed);
    if budget > 0 && m.format == Format::Strata && body.is_object() && body.get("reasoning_budget_tokens").is_none() {
        body["reasoning_budget_tokens"] = json!(budget);
    }
}

// ---------- what the wall's keys and the matching commands open ----------

/// The editor for a file during a run: lmdeck's (ui.editor, $VISUAL, $EDITOR, nvim). nvim starts
/// with its language servers off unless asked (`lsp`: they can take a gigabyte), in 16 colours on
/// the text console, and with `listen` a socket the wall uses to save its session before the
/// screen goes off.
pub fn editor_argv(cfg: &Config, file: &Path, lsp: bool, listen: Option<&Path>) -> Vec<String> {
    let mut argv = crate::sysprompt::editor_cmd(cfg, file);
    let exe = Path::new(&argv[0]).file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
    if exe == "nvim" {
        let path = argv.pop().unwrap_or_default();
        if !lsp {
            argv.extend(["--cmd".into(), "lua vim.lsp.start = function() end; vim.lsp.enable = function() end".into()]);
        }
        if std::env::var("TERM").is_ok_and(|t| t == "linux") {
            argv.extend(["-c".into(), "set notermguicolors".into()]);
        }
        if let Some(sock) = listen {
            argv.extend(["--listen".into(), sock.display().to_string()]);
        }
        argv.push(path);
    }
    argv
}

/// The task branch against its base (commits and what is not committed yet), in git's pager.
pub fn diff_argv(t: &TaskState) -> Option<Vec<String>> {
    let (wt, base) = (t.worktree.as_ref()?, t.base.as_ref()?);
    Some(vec!["git".into(), "-C".into(), wt.clone(), "--paginate".into(), "diff".into(), "--stat".into(), "--patch".into(), base.clone()])
}

/// The newest session transcript of a task, followed live (`less +F`; Ctrl+C, then q).
pub fn last_log(slug: &str) -> Option<PathBuf> {
    let mut logs: Vec<(u32, PathBuf)> = std::fs::read_dir(task_dir(slug))
        .ok()?
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_str()?.strip_prefix("session-")?.strip_suffix(".log")?.parse().ok()?;
            Some((n, e.path()))
        })
        .collect();
    logs.sort();
    logs.pop().map(|l| l.1)
}

pub fn log_argv(slug: &str) -> Option<Vec<String>> {
    last_log(slug).map(|p| vec!["less".into(), "-R".into(), "+F".into(), p.display().to_string()])
}

/// A shell for the user, niced so a build waits for the model instead of the other way round.
pub fn shell_argv(dir: &Path) -> Vec<String> {
    let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "bash".into());
    let mut v: Vec<String> = vec!["nice".into(), "-n".into(), "19".into()];
    if crate::config::which("ionice").is_some() {
        v.extend(["ionice".into(), "-c".into(), "3".into()]);
    }
    v.extend(["sh".into(), "-c".into(), format!("cd '{}' 2>/dev/null; exec {shell}", dir.display().to_string().replace('\'', "'\\''"))]);
    v
}

/// A task by its number in the list (1-based), else the current one.
pub fn task_n(st: &State, n: Option<usize>) -> Option<TaskState> {
    match n {
        Some(n) => st.tasks.get(n.checked_sub(1)?).cloned(),
        None => st.current().cloned(),
    }
}

// ---------- the command line: `lmdeck xtreme ...` ----------

#[derive(clap::Subcommand)]
pub enum XtCmd {
    /// Start a run: desktop and screen off, the model in the freed memory, the agent on the tasks
    Start {
        /// End after this many hours (then a 60 s countdown and the desktop comes back)
        #[arg(long)]
        hours: Option<f64>,
        /// End at this local time (HH:MM)
        #[arg(long)]
        until: Option<String>,
        /// No end, unattended: Tailscale on, 80% charge limit, hardware watchdog, resumes after a reboot
        #[arg(long)]
        away: bool,
        /// Everything but the root steps and the model load (checks the task loop)
        #[arg(long)]
        dry_run: bool,
        /// Task list (default xtreme.tasks)
        #[arg(long)]
        tasks: Option<String>,
        #[arg(long, short)]
        model: Option<String>,
        /// claude | opencode (default xtreme.agent)
        #[arg(long)]
        agent: Option<String>,
        /// The panel at the start (default xtreme.screen)
        #[arg(long, value_parser = ["on", "off"])]
        screen: Option<String>,
        /// GiB kept out of the arena for editing and builds (default xtreme.reserve_gib)
        #[arg(long)]
        reserve: Option<f64>,
    },
    /// One line of state
    Status {
        #[arg(long)]
        json: bool,
    },
    /// End the run and bring the desktop back
    Stop,
    /// The wall as plain text, reprinted when something changes
    Watch,
    /// The wall, full screen (what the console shows during a run)
    Wall,
    /// The TUI on its XTreme tab: plan the tasks, start, read the last report
    Plan,
    /// Append a task; the first line is its title (the repository: --repo, else the git checkout you are in)
    Add {
        text: Vec<String>,
        #[arg(long)]
        repo: Option<String>,
    },
    /// The last run's report
    Report,
    /// Edit the task list (the run reads it again between sessions)
    Tasks,
    /// Edit a task's notes, PROGRESS.md (N = its number; default the current task)
    Progress { n: Option<usize> },
    /// The task branch's diff against its base, in the pager
    Diff { n: Option<usize> },
    /// The agent's transcript, followed live
    Log { n: Option<usize> },
    /// The task's repository in the editor (the task pauses meanwhile)
    Edit { n: Option<usize> },
    /// Hold after the current turn (the model stays loaded)
    Pause,
    Resume,
    /// End this task's session and go to the next task
    Skip,
    /// Panel off or on
    Screen {
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
    /// Open a file in the editor (language servers off unless --lsp)
    Open {
        file: String,
        #[arg(long)]
        lsp: bool,
    },
    /// LAN addresses and the ports apps listen on
    Ip,
    /// More time before the desktop comes back
    Extend {
        #[arg(long, default_value_t = 60)]
        minutes: u64,
    },
}

async fn get_view(d: &crate::client::Daemon) -> anyhow::Result<Value> {
    let r = d.http().get(format!("{}/lmdeck/xtreme", d.base)).timeout(Duration::from_secs(5)).send().await
        .map_err(|_| anyhow::anyhow!("the lmdeck server is not running (lmdeck service start)"))?;
    Ok(r.json().await?)
}

pub async fn post(d: &crate::client::Daemon, action: &str, body: Value) -> anyhow::Result<Value> {
    let r = d.http().post(format!("{}/lmdeck/xtreme/{action}", d.base)).json(&body).timeout(Duration::from_secs(30)).send().await
        .map_err(|_| anyhow::anyhow!("the lmdeck server is not running (lmdeck service start)"))?;
    let ok = r.status().is_success();
    let v: Value = r.json().await.unwrap_or_default();
    if !ok {
        anyhow::bail!("{}", v["error"].as_str().unwrap_or("request failed"));
    }
    Ok(v)
}

fn view_state(v: &Value) -> State {
    serde_json::from_value(v["state"].clone()).unwrap_or_default()
}

/// `lmdeck xtreme status`: one line.
pub fn status_line(v: &Value) -> String {
    let st = view_state(v);
    if !st.phase.active() {
        return match st.ended {
            Some(e) => format!("XTreme: no run; the last ended at {} ({}): {}", clock(e), st.end_reason, summary_line(&st)),
            None => "XTreme: no run yet (lmdeck xtreme start)".into(),
        };
    }
    let mut parts = vec![format!("XTreme {}{}", st.phase.label(), if st.paused { ", paused" } else if st.pause_requested { ", pausing" } else { "" })];
    let el = span(v["elapsed_secs"].as_u64().unwrap_or(0));
    parts.push(match st.ends_at {
        Some(e) => format!("{el} of {} (desktop back at {})", span(e.saturating_sub(st.started)), clock(e)),
        None => format!("{el}, no end"),
    });
    let done = st.tasks.iter().filter(|t| t.status == "done").count();
    parts.push(format!("{done} of {} done", st.tasks.len()));
    if let Some(sess) = &st.session {
        let i = st.tasks.iter().position(|t| t.slug == sess.slug).map(|i| i + 1).unwrap_or(0);
        let title = st.task(&sess.slug).map(|t| t.title.clone()).unwrap_or_default();
        parts.push(format!("#{i} {title}: session {}, turn {}/{}", sess.n, sess.turns, sess.max_turns));
    }
    let mut m = st.model.clone();
    if let Some(a) = st.arena_gb {
        m.push_str(&format!(" arena {a:.1} GB"));
    }
    if let Some(r) = v["decode_tps"].as_f64() {
        m.push_str(&format!(" {r:.1} tok/s"));
    }
    parts.push(m);
    parts.join(" · ")
}

fn run(argv: &[String]) -> anyhow::Result<()> {
    let st = std::process::Command::new(&argv[0]).args(&argv[1..]).status().map_err(|e| anyhow::anyhow!("{}: {e}", argv[0]))?;
    if !st.success() {
        anyhow::bail!("{} exited with {st}", argv[0]);
    }
    Ok(())
}

/// The task list file, written from the template the first time.
pub fn ensure_tasks_file(cfg: &Config, st: &State) -> PathBuf {
    let p = if st.phase.active() && !st.tasks_file.is_empty() { PathBuf::from(&st.tasks_file) } else { tasks_path(cfg) };
    if !p.exists() {
        let _ = std::fs::create_dir_all(p.parent().unwrap_or(Path::new("/")));
        let _ = std::fs::write(&p, TASKS_TEMPLATE);
    }
    p
}

pub async fn cli(cfg: &Config, d: &crate::client::Daemon, cmd: Option<XtCmd>) -> anyhow::Result<()> {
    use anyhow::{bail, Context};
    let cmd = match cmd {
        None | Some(XtCmd::Wall) => return crate::tui::run_wall(cfg.clone()).await,
        Some(XtCmd::Plan) => return crate::tui::run_xtreme_tab(cfg.clone()).await,
        Some(c) => c,
    };
    match cmd {
        XtCmd::Start { hours, until, away, dry_run, tasks, model, agent, screen, reserve } => {
            if !d.up().await {
                crate::client::start_daemon()?;
                for _ in 0..50 {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    if d.up().await {
                        break;
                    }
                }
            }
            let agent_name = agent.clone().unwrap_or_else(|| cfg.xtreme.agent.clone());
            let body = json!({
                "hours": hours, "until": until, "away": away, "dry_run": dry_run,
                "tasks": tasks.map(|t| std::fs::canonicalize(&t).map(|p| p.display().to_string()).unwrap_or(t)),
                "model": model, "agent": agent,
                "agent_bin": crate::config::which(&agent_name).map(|p| p.display().to_string()),
                "path_env": std::env::var("PATH").unwrap_or_default(),
                "screen": screen, "reserve_gib": reserve,
            });
            let v = post(d, "start", body).await?;
            let st = view_state(&v);
            println!("XTreme run started: {}, {} task(s){}", st.model, st.tasks.len(), st.ends_at.map(|e| format!(", the desktop comes back at {}", clock(e))).unwrap_or_else(|| ", no end (lmdeck xtreme stop ends it)".into()));
            if dry_run {
                println!("dry run: the desktop, the screen and the model are left alone; `lmdeck xtreme watch` follows it");
            } else {
                println!("the desktop stops in a few seconds; the wall opens on its own console (or: Ctrl+Alt+F2, then lmdeck xtreme)");
            }
            Ok(())
        }
        XtCmd::Status { json } => {
            let v = get_view(d).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                println!("{}", status_line(&v));
            }
            Ok(())
        }
        XtCmd::Stop => {
            post(d, "stop", json!({})).await?;
            println!("stopping: the agent finishes its turn, the model unloads, the desktop comes back");
            Ok(())
        }
        XtCmd::Pause | XtCmd::Resume | XtCmd::Skip => {
            let action = match cmd { XtCmd::Pause => "pause", XtCmd::Resume => "resume", _ => "skip" };
            let v = post(d, action, json!({})).await?;
            println!("{}", status_line(&v));
            Ok(())
        }
        XtCmd::Extend { minutes } => {
            let v = post(d, "extend", json!({"secs": minutes * 60})).await?;
            println!("{}", status_line(&v));
            Ok(())
        }
        XtCmd::Screen { state } => {
            post(d, "screen", json!({"on": state == "on"})).await?;
            Ok(())
        }
        XtCmd::Add { text, repo } => {
            let text = text.join(" ");
            let repo = repo.or_else(|| {
                let o = std::process::Command::new("git").args(["rev-parse", "--show-toplevel"]).output().ok()?;
                o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
            });
            let repo = repo.map(|r| r.replace(&crate::config::home().display().to_string(), "~"));
            let v = match post(d, "add", json!({"text": text, "repo": repo})).await {
                Ok(v) => v,
                // No server: write the file directly.
                Err(_) => add_task(cfg, &State::default(), &text, repo.as_deref()).map_err(anyhow::Error::msg)?,
            };
            println!("added to {}{}", v["file"].as_str().unwrap_or(""), repo.map(|r| format!(" (repo: {r})")).unwrap_or_default());
            Ok(())
        }
        XtCmd::Report => {
            let text = std::fs::read_to_string(report_path()).context("no report yet (a run writes it)")?;
            print!("{text}");
            Ok(())
        }
        XtCmd::Watch => watch(d).await,
        XtCmd::Ip => {
            for (i, ip) in sys::lan_ips() {
                println!("{i}: {ip}");
            }
            let ports = listening_ports();
            println!("listening (not localhost): {}", if ports.is_empty() { "none".into() } else { ports.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(" ") });
            println!("ufw is active here: another machine reaches a port after `sudo ufw allow PORT`");
            Ok(())
        }
        XtCmd::Open { file, lsp } => run(&editor_argv(cfg, Path::new(&file), lsp, None)),
        XtCmd::Tasks => {
            let st = get_view(d).await.map(|v| view_state(&v)).unwrap_or_default();
            run(&editor_argv(cfg, &ensure_tasks_file(cfg, &st), false, None))
        }
        XtCmd::Progress { n } | XtCmd::Diff { n } | XtCmd::Log { n } | XtCmd::Edit { n } => {
            let st = view_state(&get_view(d).await?);
            let Some(t) = task_n(&st, n) else { bail!("no such task (lmdeck xtreme status lists them)") };
            match cmd {
                XtCmd::Progress { .. } => run(&editor_argv(cfg, &progress_path(&t.slug), false, None)),
                XtCmd::Diff { .. } => run(&diff_argv(&t).context("this task has no branch (no git repository, or no session yet)")?),
                XtCmd::Log { .. } => run(&log_argv(&t.slug).context("no session of this task yet")?),
                _ => {
                    let paused = st.phase.active() && !st.pause_requested;
                    if paused {
                        post(d, "pause", json!({})).await?;
                        eprintln!("paused the run while you edit (resumes when the editor exits)");
                    }
                    let r = run(&editor_argv(cfg, &t.workdir(), false, None));
                    if paused {
                        post(d, "resume", json!({})).await?;
                    }
                    r
                }
            }
        }
        XtCmd::Wall | XtCmd::Plan => unreachable!(),
    }
}

/// `lmdeck xtreme watch`: the wall as plain text, reprinted on every change (the daemon's
/// long-poll) and every 10 s for the clock.
async fn watch(d: &crate::client::Daemon) -> anyhow::Result<()> {
    let mut since = 0u64;
    loop {
        let url = format!("{}/lmdeck/watch?since={since}&timeout=10", d.base);
        let s: Value = match d.http().get(url).timeout(Duration::from_secs(20)).send().await {
            Ok(r) => r.json().await.unwrap_or_default(),
            Err(_) => {
                println!("lmdeck server not reachable; retrying");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };
        since = s["event"].as_u64().unwrap_or(0);
        let (w, h) = crossterm::terminal::size().map(|(w, h)| (w as usize, h as usize)).unwrap_or((100, 40));
        let lines = crate::tui::wall_text(&s["xtreme"], Some(&s), w, h.saturating_sub(1), true, "Ctrl+C leaves; the run goes on");
        print!("\x1b[H\x1b[2J{}", lines.join("\n"));
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tasks_parse_with_options_and_unique_slugs() {
        let text = "# XTreme tasks\nintro text\n\n## lmdeck: grader NaN\nrepo: ~/Work/lmdeck\ncheck: cargo test grade\nturns: 30\n\nThe grader shows NaN.\nrepo: in the body stays body\n\n## [ ] r2: find the CRC\nminutes: 20\nIn fw.bin.\n## lmdeck: grader NaN\n";
        let t = parse_tasks(text);
        assert_eq!(t.len(), 3);
        assert_eq!(t[0].slug, "lmdeck-grader-nan");
        assert_eq!(t[0].repo.as_deref(), Some("~/Work/lmdeck"));
        assert_eq!(t[0].check.as_deref(), Some("cargo test grade"));
        assert_eq!(t[0].turns, Some(30));
        assert_eq!(t[0].body, "The grader shows NaN.\nrepo: in the body stays body");
        assert_eq!(t[1].title, "r2: find the CRC");
        assert_eq!(t[1].minutes, Some(20));
        assert_eq!(t[1].body, "In fw.bin.");
        assert_eq!(t[2].slug, "lmdeck-grader-nan-2");
    }

    #[test]
    fn added_tasks_parse_back() {
        let mut text = String::from("# XTreme tasks\n");
        text.push_str(&task_section("panel: scroll on hover\nThe Flickable jumps.", Some("~/Work/lmdeck")));
        let t = parse_tasks(&text);
        assert_eq!(t[0].title, "panel: scroll on hover");
        assert_eq!(t[0].repo.as_deref(), Some("~/Work/lmdeck"));
        assert_eq!(t[0].body, "The Flickable jumps.");
        assert_eq!(parse_tasks(TASKS_TEMPLATE).len(), 1);
    }

    #[test]
    fn merge_keeps_status_and_follows_the_file_order() {
        let mut st = State::default();
        merge_tasks(&mut st, &parse_tasks("## a\n## b\n"));
        st.tasks[0].status = "done".into();
        st.tasks[0].sessions = 2;
        merge_tasks(&mut st, &parse_tasks("## c\n## b\n"));
        let order: Vec<&str> = st.tasks.iter().map(|t| t.slug.as_str()).collect();
        assert_eq!(order, ["c", "b", "a"]); // a ran, so it stays (at the end)
        assert_eq!(st.tasks[2].status, "done");
        assert_eq!(st.tasks[0].status, "pending");
    }

    #[test]
    fn notes_status_takes_the_last_line() {
        assert_eq!(notes_status("## Session 1\n- did x\nSTATUS: blocked: needs a device\n## Session 2\n- STATUS: done"), Some(("done".into(), "".into())));
        assert_eq!(notes_status("status: blocked: no access"), Some(("blocked".into(), "no access".into())));
        assert_eq!(notes_status("nothing yet"), None);
    }

    #[test]
    fn engine_log_numbers() {
        let log = "strata metal stream: expert arena 3.85 GB, 2762 slots in 1 size classes\n[strata] thinking: 2089 of budget 9808 (max 13077) tokens, 5.9 tok/s, 367 s\n[strata] done: 2091 tokens in 368 s (6.2 tok/s) (stop, cancel=False)\n";
        assert_eq!(parse_arena(log), Some(3.85));
        assert_eq!(parse_decode(log), Some(6.2));
        assert_eq!(parse_decode("[strata] thinking: 81 of budget 23022 (max 30695) tokens, 6.7 tok/s, 31 s"), Some(6.7));
    }

    #[test]
    fn claude_stream_events() {
        let a = json!({"type": "assistant", "message": {"id": "m1", "content": [{"type": "text", "text": "Running the tests."}, {"type": "tool_use", "name": "Bash", "input": {"command": "cargo test\n--quiet"}}]}});
        let e = transcript_event(&a);
        assert_eq!(e.message_id.as_deref(), Some("m1"));
        assert_eq!(e.tool.as_deref(), Some("bash       cargo test"));
        assert!(e.text.unwrap().starts_with("Running the tests.\n> bash"));
        let r = json!({"type": "user", "message": {"content": [{"type": "tool_result", "content": "a\n\nb\ntest result: ok. 93 passed"}]}});
        assert!(transcript_event(&r).text.unwrap().contains("93 passed"));
        let done = json!({"type": "result", "subtype": "success", "num_turns": 9, "duration_ms": 61000});
        assert_eq!(transcript_event(&done).text.as_deref(), Some("== result: success after 9 turn(s), 61 s"));
    }

    #[test]
    fn clocks_and_spans() {
        assert_eq!(span(5 * 3600 + 12 * 60), "5h12m");
        assert_eq!(span(125), "2m");
        let t = now();
        let n = next_clock("07:30", t).unwrap();
        assert!(n > t && n <= t + 86400);
        assert_eq!(clock(n), "07:30");
        assert!(next_clock("25:00", t).is_none());
    }

    #[test]
    fn prompt_carries_task_place_and_notes() {
        let spec = parse_tasks("## fix it\ncheck: make test\n\nThe bug.\n").remove(0);
        let t = TaskState { slug: spec.slug.clone(), check: spec.check.clone(), worktree: Some("/w".into()), branch: Some("xtreme/fix-it".into()), base: Some("0123456789abcdef".into()), ..Default::default() };
        let p = session_prompt(&spec, &t, 2, 40, 45, "# fix it\n\n## Session 1\n- tried x\n");
        assert!(p.contains("TASK: fix it\n\nThe bug."));
        assert!(p.contains("WHERE: /w (git branch xtreme/fix-it, based on 0123456789)"));
        assert!(p.contains("CHECK: make test"));
        assert!(p.contains("SESSION: 2, at most 40 turns"));
        assert!(p.contains("- tried x"));
    }
}
