//! XTreme: tab 7, and the wall (the same panel full screen on the text console during a run,
//! `lmdeck xtreme wall`). One renderer, `wall_text`, also prints `lmdeck xtreme watch`.
//!
//! The wall redraws on the daemon's long-poll and a 10 s clock tick, draws nothing while the
//! panel is off, turns the panel off after `xtreme.screen_idle_secs` without a key, and before
//! that saves and closes an open nvim (and an idle shell) so nothing interactive stays loaded.

use super::widgets::Act;
use super::{Action, Msg, Tab, Ui};
use crate::xtreme::{self as xt, Phase, State, TaskState};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const LOGO: [&str; 4] = [
    r" __  _______ ____  _____ __  __ _____",
    r" \ \/ /_   _|  _ \| ____|  \/  | ____|",
    r"  >  <  | | | |_) |  _| | |\/| |  _|",
    r" /_/\_\ |_| |_| \_\_____|_|  |_|_____|",
];
const WALL_KEYS: &str = "a add  t tasks  p progress  d diff  l log  e edit  space pause  n skip  s screen  o open  ! shell  q leave  X end";
const HOURS: [u32; 6] = [0, 1, 2, 4, 8, 12];

#[derive(Default)]
pub(super) struct XtUi {
    /// Full screen on the console: every key is the wall's.
    pub wall: bool,
    pub picker: Option<Picker>,
    pub start: Option<StartForm>,
    pub confirm_stop: bool,
    /// `a`: a task typed in: (title, repo, which field has the cursor: 0 title, 1 repo).
    pub add: Option<(String, String, usize)>,
    pub last_key: Option<Instant>,
    /// The run was paused for `e`; resume when the editor exits.
    pub resume_after: bool,
}

pub(super) struct Picker {
    pub query: String,
    pub files: Vec<(String, PathBuf)>,
    pub sel: usize,
    pub lsp: bool,
}

pub(super) struct StartForm {
    pub hours: usize,
    pub away: bool,
    pub dry: bool,
    pub serve: bool,
    pub lan: bool,
}

impl StartForm {
    /// Server mode is the default when there is no task to do.
    fn new(planned: usize) -> Self {
        Self { hours: 0, away: false, dry: false, serve: planned == 0, lan: planned == 0 }
    }
}

/// Something the wall runs in its terminal: an editor (whose session it may save and close when
/// the panel goes off), a shell (closed then if it runs nothing), or a pager.
#[derive(Clone, Debug)]
pub struct Job {
    pub argv: Vec<String>,
    pub kind: JobKind,
    /// Seconds without input before the panel goes off (0 = never) and the daemon to tell.
    pub idle_secs: u64,
    pub base: String,
}

#[derive(Clone, Debug)]
pub enum JobKind {
    Plain,
    /// nvim with a socket; `reopen` restores the session it saves.
    Editor { sock: PathBuf, session: PathBuf, reopen: Vec<String> },
    Shell,
}

/// An editor closed by the panel going off, reopened by the next key.
static REOPEN: Mutex<Option<Job>> = Mutex::new(None);

pub fn ascii() -> bool {
    std::env::var("TERM").is_ok_and(|t| t == "linux")
}

fn st_of(x: &Value) -> State {
    serde_json::from_value(x["state"].clone()).unwrap_or_default()
}

/// At most `w` characters.
fn fit(s: &str, w: usize) -> String {
    if s.chars().count() <= w {
        return s.to_string();
    }
    let mut out: String = s.chars().take(w.saturating_sub(1)).collect();
    out.push('~');
    out
}

fn pad(s: &str, w: usize) -> String {
    let s = fit(s, w);
    let n = s.chars().count();
    format!("{s}{}", " ".repeat(w.saturating_sub(n)))
}

/// `left` and `right` on one line of width `w`, right flush.
fn spread(left: &str, right: &str, w: usize) -> String {
    let r = fit(right, w / 2);
    let lw = w.saturating_sub(r.chars().count() + 1);
    format!("{} {r}", pad(left, lw))
}

fn k(n: u64) -> String {
    if n >= 1000 { format!("{:.1}k", n as f64 / 1000.0) } else { n.to_string() }
}

fn bar(frac: f64, w: usize, ascii: bool) -> String {
    let n = ((frac.clamp(0.0, 1.0) * w as f64).round() as usize).min(w);
    let (on, off) = if ascii { ('#', '.') } else { ('█', '░') };
    format!("[{}{}]", on.to_string().repeat(n), off.to_string().repeat(w - n))
}

fn mark(t: &TaskState, current: Option<&str>) -> &'static str {
    match t.status.as_str() {
        "done" => "[x]",
        "running" => "[>]",
        "blocked" | "failed" => "[!]",
        "stuck" => "[?]",
        "skipped" => "[-]",
        "out of sessions" => "[~]",
        _ if current == Some(t.slug.as_str()) => "[>]",
        _ => "[ ]",
    }
}

/// The wall as lines of text, `w` x at most `h`. `x` = GET /lmdeck/xtreme, `s` = /lmdeck/status
/// (the model's context and the request in flight). `keys` is the last line ("" = none).
pub fn wall_text(x: &Value, s: Option<&Value>, w: usize, h: usize, ascii: bool, keys: &str) -> Vec<String> {
    let w = w.max(20);
    let st = st_of(x);
    let hr = if ascii { "-" } else { "─" }.repeat(w);
    let vbar = if ascii { " | " } else { " │ " };
    let running = s.and_then(|s| s["running"].as_array()).and_then(|r| r.iter().find(|r| r["name"] == st.model.as_str()).or(r.first())).cloned();
    let active = st.phase.active();

    // Header: the logo with four lines of state beside it.
    let el = x["elapsed_secs"].as_u64().unwrap_or(0);
    let time = match (active, st.ends_at) {
        (false, _) => st.ended.map(|e| format!("last run ended {}", xt::clock(e))).unwrap_or_else(|| "no run yet".into()),
        (true, Some(e)) => format!("{} of {}  (desktop back {})", xt::span(el), xt::span(e.saturating_sub(st.started)), xt::clock(e)),
        (true, None) => format!("{}, no end", xt::span(el)),
    };
    let model = if st.model.is_empty() { "XTreme".to_string() } else { st.model.clone() };
    let mut speed = vec![st.arena_gb.map(|a| format!("arena {a:.1} GB")).unwrap_or_else(|| "arena -".into())];
    // Only once this run has loaded the model: the log line may be from an older request.
    if let Some(r) = x["decode_tps"].as_f64().filter(|_| active && st.arena_gb.is_some()) {
        speed.push(format!("{r:.1} tok/s"));
    }
    if let Some(r) = running.as_ref().and_then(|r| r["work"]["tokens_per_sec"].as_f64()) {
        speed.push(format!("prompt {r:.0} tok/s"));
    }
    let (total, avail) = s.map(|s| (s["mem"]["total"].as_u64().unwrap_or(0), s["mem"]["available"].as_u64().unwrap_or(0))).unwrap_or((0, 0));
    let gb = |b: u64| b as f64 / 1e9;
    let mut state = st.phase.label().to_string();
    if st.paused {
        state.push_str(", paused");
    } else if st.pause_requested && active {
        state.push_str(", pausing after this turn");
    }
    if st.dry_run && active {
        state.push_str(" (dry run)");
    }
    let ips: Vec<String> = s.and_then(|s| s["ips"].as_array()).into_iter().flatten().filter_map(|i| i["ip"].as_str().map(str::to_string)).collect();
    let ports: Vec<String> = x["ports"].as_array().into_iter().flatten().filter_map(|p| p.as_u64().map(|p| p.to_string())).collect();
    let power = match (x["power"]["ac"].as_bool(), x["power"]["charge"].as_u64()) {
        (Some(true), Some(c)) => format!("AC, battery {c}%"),
        (Some(false), Some(c)) => format!("on battery {c}%"),
        _ => String::new(),
    };
    let lan = &x["lan"];
    let lan_line = lan["urls"].as_array().and_then(|u| u.first()).and_then(|u| u.as_str()).filter(|_| lan["open"] == true && active).map(|url| {
        format!("network {url}{}", lan["token"].as_str().map(|t| format!("   token {t}")).unwrap_or_else(|| "   (no token)".into()))
    });
    let info = [
        String::new(), // the model and the time, spread to the width below
        speed.join("   "),
        format!("desktop {}   screen {}   RAM {:.1} / {:.1} GB   {state}", if st.desktop_stopped && !st.dry_run { "off" } else { "on" }, if st.screen.is_empty() { "on" } else { &st.screen }, gb(total.saturating_sub(avail)), gb(total)),
        lan_line.clone().unwrap_or_else(|| format!("{}{}   {power}", if ips.is_empty() { "no network".into() } else { format!("IP {}", ips.join(" ")) }, if ports.is_empty() { String::new() } else { format!("   ports {}", ports.join(" ")) })),
    ];
    let mut out: Vec<String> = Vec::new();
    let logo = w >= 90 && h >= 22;
    let iw = if logo { w - 41 } else { w };
    for (i, line) in info.iter().enumerate() {
        let line = if i == 0 { spread(&model, &time, iw) } else { fit(line, iw) };
        out.push(if logo { format!("{:<41}{line}", LOGO[i]) } else { line });
    }
    out.push(hr.clone());

    let tail_rows = if keys.is_empty() { 0 } else { 2 };
    let body = h.saturating_sub(out.len() + tail_rows + 1);
    let lw = (w * 45 / 100).max(10);
    let rw = w.saturating_sub(lw + 3);

    // Tasks | now. (After a run, no task is current.)
    let cur = x["current"].as_str().filter(|_| active);
    let mut left: Vec<String> = Vec::new();
    if active && st.tasks.is_empty() {
        left.push("TASKS".into());
        left.push("none: serving only. a adds one; the agent".into());
        left.push("starts on it, the model stays loaded.".into());
    } else if active || !st.tasks.is_empty() {
        let done = st.tasks.iter().filter(|t| t.status == "done").count();
        left.push(spread("TASKS", &format!("{done} of {} done", st.tasks.len()), lw));
        for (i, t) in st.tasks.iter().enumerate() {
            let tail = match (t.commits, t.status.as_str()) {
                (0, "pending") if t.sessions == 0 => String::new(),
                (c, "running") => format!("{c} commit{}, ...", if c == 1 { "" } else { "s" }),
                (c, st) => format!("{c} commit{}{}", if c == 1 { "" } else { "s" }, if matches!(st, "done" | "pending") { String::new() } else { format!(", {st}") }),
            };
            left.push(spread(&format!("{} {} {}", mark(t, cur), i + 1, t.title), &tail, lw));
        }
    } else {
        let planned: Vec<String> = x["planned"].as_array().into_iter().flatten().filter_map(|p| p.as_str().map(str::to_string)).collect();
        left.push(spread("TASKS (planned)", &format!("{}", planned.len()), lw));
        for (i, p) in planned.iter().enumerate() {
            left.push(format!("[ ] {} {p}", i + 1));
        }
        if planned.is_empty() {
            left.push("none yet: t writes the task list".into());
        }
    }
    let mut right: Vec<String> = Vec::new();
    if let Some(c) = x["countdown_secs"].as_u64().filter(|_| st.phase == Phase::Countdown) {
        right.push(format!("BACK TO THE DESKTOP IN {c} s"));
        right.push("any key adds an hour, Enter or X goes now".into());
    }
    match st.session.as_ref() {
        Some(sess) => {
            let i = st.tasks.iter().position(|t| t.slug == sess.slug).map(|i| i + 1).unwrap_or(0);
            let title = st.task(&sess.slug).map(|t| t.title.clone()).unwrap_or_default();
            right.push(format!("NOW  #{i} {title}"));
            let left_s = sess.deadline.saturating_sub(x["now"].as_u64().unwrap_or(0));
            right.push(format!("session {}, turn {}/{}, {} left", sess.n, sess.turns, sess.max_turns, xt::span(left_s)));
        }
        None => right.push(format!("NOW  {}", if active { st.phase.label() } else { "no run" })),
    }
    if active && st.serve {
        let u = s.map(|s| &s["usage"]);
        if let Some(l) = u.map(|u| &u["last"]).filter(|l| l.is_object()) {
            right.push(format!("last request  {}: {:.1} tok/s, prompt {:.0} tok/s, first token {:.1} s", l["model"].as_str().unwrap_or(""), l["gen_tps"].as_f64().unwrap_or(0.0), l["prompt_tps"].as_f64().unwrap_or(0.0), l["ttft_secs"].as_f64().unwrap_or(0.0)));
        }
        if let Some(t) = u.map(|u| &u["today"]).filter(|t| t["requests"].as_u64().is_some()) {
            right.push(format!("today  {} requests, {} in, {} out", t["requests"], k(t["prompt"].as_u64().unwrap_or(0)), k(t["completion"].as_u64().unwrap_or(0))));
        }
        if lan["open"] != true && st.lan {
            right.push("network listener not open (see LOG)".into());
        }
    }
    if let Some(r) = &running {
        let c = &r["context"];
        if let (Some(used), Some(limit)) = (c["used"].as_u64(), c["limit"].as_u64()) {
            let bw = rw.saturating_sub(24).clamp(5, 20);
            right.push(format!("context  {}  {} / {}", bar(used as f64 / limit.max(1) as f64, bw, ascii), k(used), k(limit)));
        }
        let wk = &r["work"];
        let line = match wk["phase"].as_str() {
            Some("reading prompt") => format!(
                "reading the prompt {} / {}{}",
                k(wk["prompt_done"].as_u64().unwrap_or(0)),
                k(wk["prompt_tokens"].as_u64().unwrap_or(0)),
                wk["eta_secs"].as_f64().map(|e| format!(", {} left", xt::span(e as u64))).unwrap_or_default()
            ),
            Some("generating") => format!("writing: {} tokens", wk["generated"].as_u64().unwrap_or(0)),
            Some(_) => "the engine is getting ready".into(),
            None if st.session.is_some() => "the agent's turn (a tool runs, or the next request is on its way)".into(),
            None => String::new(),
        };
        if !line.is_empty() {
            right.push(line);
        }
    } else if active && !st.dry_run && st.phase != Phase::Waiting {
        right.push("model not loaded".into());
    }
    if let Some(sess) = &st.session {
        if !sess.last_tool.is_empty() {
            right.push(format!("last tool  {}", sess.last_tool));
        }
    }
    if let Some(t) = cur.and_then(|c| st.task(c)) {
        if let Some(c) = &t.check {
            right.push(format!("check  {c}{}", match t.check_ok { Some(true) => " -> passed", Some(false) => " -> failed", None => "" }));
        }
        if !t.note.is_empty() && t.finished() {
            right.push(format!("note  {}", t.note));
        }
    }
    if !active {
        right.push(String::new());
        right.push(x["summary"].as_str().filter(|_| st.ended.is_some()).map(|s| format!("last run: {s}")).unwrap_or_default());
    }
    // As tall as the tasks and the now pane need (at most 60%); the log gets the rest.
    let top = (left.len().max(right.len()) + 1).clamp(3.min(body), (body * 60 / 100).max(3.min(body)));
    let bottom = body.saturating_sub(top);
    for i in 0..top {
        out.push(format!("{}{vbar}{}", pad(left.get(i).map(String::as_str).unwrap_or(""), lw), fit(right.get(i).map(String::as_str).unwrap_or(""), rw)).trim_end().to_string());
    }

    // Notes | log.
    if bottom > 1 {
        out.push(hr.clone());
        let mut left = vec![cur.and_then(|c| st.tasks.iter().position(|t| t.slug == c)).map(|i| format!("PROGRESS #{} (last lines)", i + 1)).unwrap_or_else(|| "PROGRESS".into())];
        let notes: Vec<String> = x["progress_tail"].as_array().into_iter().flatten().filter_map(|l| l.as_str().map(str::to_string)).collect();
        left.extend(notes[notes.len().saturating_sub(bottom - 2)..].iter().cloned());
        let mut right = vec!["LOG".to_string()];
        right.extend(st.log[st.log.len().saturating_sub(bottom - 2)..].iter().cloned());
        for i in 0..bottom - 1 {
            out.push(format!("{}{vbar}{}", pad(left.get(i).map(String::as_str).unwrap_or(""), lw), fit(right.get(i).map(String::as_str).unwrap_or(""), rw)).trim_end().to_string());
        }
    }
    if !keys.is_empty() {
        out.push(hr);
        out.push(fit(keys, w));
    }
    out.truncate(h);
    out
}

impl Ui {
    fn xview(&self) -> Value {
        self.status.as_ref().map(|s| s["xtreme"].clone()).unwrap_or(Value::Null)
    }

    fn xstate(&self) -> State {
        st_of(&self.xview())
    }

    pub(super) fn xtreme_actions(&self) -> Vec<Act> {
        let st = self.xstate();
        let up = self.status.is_some();
        if !st.phase.active() {
            let mut a = vec![
                Act::new(KeyCode::Char('S'), "Start XTreme…").when(up, "the server is not running (s on Home)"),
                Act::new(KeyCode::Char('a'), "Add a task"),
                Act::new(KeyCode::Char('t'), "Edit the tasks"),
            ];
            if xt::report_path().exists() {
                a.push(Act::new(KeyCode::Char('r'), "Last report"));
            }
            return a;
        }
        let cur = st.current().cloned();
        let has_cur = cur.is_some();
        let mut a = vec![
            Act::new(KeyCode::Char('a'), "Add a task"),
            Act::new(KeyCode::Char('t'), "Tasks"),
            Act::new(KeyCode::Char('p'), "Progress").when(has_cur, "no task"),
            Act::new(KeyCode::Char('d'), "Diff").when(cur.as_ref().is_some_and(|t| t.base.is_some()), "this task has no branch yet"),
            Act::new(KeyCode::Char('l'), "Log").when(cur.as_ref().is_some_and(|t| xt::last_log(&t.slug).is_some()), "no session yet"),
            Act::new(KeyCode::Char('e'), "Edit the repo").when(has_cur, "no task"),
            Act::new(KeyCode::Char(' '), if st.pause_requested { "Resume" } else { "Pause" }),
            Act::new(KeyCode::Char('n'), "Skip task").when(has_cur, "no task"),
            Act::new(KeyCode::Char('s'), if st.screen == "off" { "Screen on" } else { "Screen off" }),
            Act::new(KeyCode::Char('o'), "Open a file"),
            Act::new(KeyCode::Char('O'), "Open with LSP"),
            Act::new(KeyCode::Char('!'), "Shell"),
        ];
        if st.ends_at.is_some() {
            a.push(Act::new(KeyCode::Char('+'), "One more hour"));
        }
        if st.lan {
            a.push(Act::new(KeyCode::Char('i'), "Connect info"));
        }
        a.push(Act::new(KeyCode::Char('X'), if self.xt.confirm_stop { "End: press X again" } else { "End the run" }));
        a
    }

    fn post_xt(&self, action: &'static str, body: Value) {
        let d = self.d.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match xt::post(&d, action, body).await {
                Ok(_) => {}
                Err(e) => {
                    let _ = tx.send(Msg::Toast(format!("{action}: {e:#}")));
                }
            }
            let _ = tx.send(Msg::Status(d.status().await));
        });
    }

    fn job(&self, argv: Vec<String>, kind: JobKind) -> Action {
        let st = self.xstate();
        let idle = if st.phase.active() && st.screen == "on" && self.xt.wall { self.cfg.xtreme.screen_idle_secs as u64 } else { 0 };
        Action::Job(Job { argv, kind, idle_secs: idle, base: self.cfg.base_url() })
    }

    fn editor_job(&self, file: &Path, lsp: bool) -> Action {
        let run = crate::config::state_dir().join("xtreme");
        let sock = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(|| run.clone()).join(format!("lmdeck-xtreme-nvim-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let argv = xt::editor_argv(&self.cfg, file, lsp, Some(&sock));
        if argv[0].ends_with("nvim") {
            let session = run.join("nvim-session.vim");
            let mut reopen = argv.clone();
            reopen.pop();
            reopen.extend(["-S".into(), session.display().to_string()]);
            self.job(argv, JobKind::Editor { sock, session, reopen })
        } else {
            self.job(argv, JobKind::Plain)
        }
    }

    pub(super) fn xtreme_act(&mut self, key: KeyCode) -> Option<Action> {
        let st = self.xstate();
        if key != KeyCode::Char('X') {
            self.xt.confirm_stop = false;
        }
        let cur = st.current().cloned();
        match key {
            KeyCode::Char('S') => {
                let planned = if st.phase.active() { st.tasks.iter().filter(|t| !t.finished()).count() } else { self.xview()["planned"].as_array().map(|p| p.len()).unwrap_or(0) };
                self.xt.start = Some(StartForm::new(planned));
            }
            KeyCode::Char('a') => {
                // The repository of the current task, else none (a scratch directory).
                let repo = cur.as_ref().and_then(|t| t.repo.clone()).unwrap_or_default();
                self.xt.add = Some((String::new(), repo, 0));
            }
            KeyCode::Char('i') => {
                let lan = &self.xview()["lan"];
                let token = lan["token"].as_str();
                let mut text = String::from("Model APIs for other computers (lmdeck xtreme ip prints this too)\n\n");
                for url in lan["urls"].as_array().into_iter().flatten().filter_map(|u| u.as_str()) {
                    text.push_str(&format!("{url}\n"));
                    for l in xt::client_snippets(url, token, &st.model) {
                        text.push_str(&format!("  {l}\n"));
                    }
                    text.push('\n');
                }
                let p = xt::dir().join("connect.txt");
                let _ = std::fs::write(&p, text);
                return Some(self.job(vec!["less".into(), p.display().to_string()], JobKind::Plain));
            }
            KeyCode::Char('t') => {
                let p = xt::ensure_tasks_file(&self.cfg, &st);
                return Some(self.editor_job(&p, false));
            }
            KeyCode::Char('r') => return Some(Action::Edit(vec!["less".into(), xt::report_path().display().to_string()])),
            KeyCode::Char('p') => return cur.map(|t| self.editor_job(&xt::progress_path(&t.slug), false)),
            KeyCode::Char('d') => return cur.as_ref().and_then(xt::diff_argv).map(|a| self.job(a, JobKind::Plain)),
            KeyCode::Char('l') => return cur.and_then(|t| xt::log_argv(&t.slug)).map(|a| self.job(a, JobKind::Plain)),
            KeyCode::Char('e') => {
                let t = cur?;
                if !st.pause_requested {
                    self.post_xt("pause", json!({}));
                    self.xt.resume_after = true;
                    self.toast("paused while you edit; it resumes when the editor exits");
                }
                return Some(self.editor_job(&t.workdir(), false));
            }
            KeyCode::Char(' ') => self.post_xt(if st.pause_requested { "resume" } else { "pause" }, json!({})),
            KeyCode::Char('n') => self.post_xt("skip", json!({})),
            KeyCode::Char('s') => self.post_xt("screen", json!({"on": st.screen == "off"})),
            KeyCode::Char('+') => self.post_xt("extend", json!({"secs": 3600})),
            KeyCode::Char('o') | KeyCode::Char('O') => {
                self.xt.picker = Some(Picker { query: String::new(), files: pick_files(&st), sel: 0, lsp: key == KeyCode::Char('O') });
            }
            KeyCode::Char('!') => {
                let dir = cur.map(|t| t.workdir()).unwrap_or_else(crate::config::home);
                return Some(self.job(xt::shell_argv(&dir), JobKind::Shell));
            }
            KeyCode::Char('X') => {
                if self.xt.confirm_stop {
                    self.xt.confirm_stop = false;
                    self.post_xt("stop", json!({}));
                    self.toast("ending the run: the desktop comes back");
                } else {
                    self.xt.confirm_stop = true;
                    self.toast("X again ends the run and brings the desktop back; any other key cancels");
                }
            }
            _ => {}
        }
        None
    }

    /// Whether this key belongs to the XTreme screen before the global keys see it.
    pub(super) fn xtreme_wants(&self) -> bool {
        self.tab == Tab::Xtreme && (self.xt.wall || self.xt.picker.is_some() || self.xt.start.is_some() || self.xt.add.is_some() || self.xt.confirm_stop || {
            let st = self.xstate();
            st.phase == Phase::Countdown || (st.phase.active() && st.screen == "off")
        })
    }

    pub(super) fn xtreme_key(&mut self, k: KeyEvent) -> Option<Action> {
        self.xt.last_key = Some(Instant::now());
        let st = self.xstate();
        // The panel off: a key brings it back (and an editor the panel closed).
        if st.phase.active() && st.screen == "off" {
            self.post_xt("screen", json!({"on": true}));
            return REOPEN.lock().unwrap().take().map(Action::Job);
        }
        if st.phase == Phase::Countdown {
            if matches!(k.code, KeyCode::Enter | KeyCode::Char('X')) {
                self.post_xt("stop", json!({}));
            } else {
                self.post_xt("extend", json!({"secs": 3600}));
                self.toast("one more hour");
            }
            return None;
        }
        if let Some(p) = &mut self.xt.picker {
            match k.code {
                KeyCode::Esc => self.xt.picker = None,
                KeyCode::Up => p.sel = p.sel.saturating_sub(1),
                KeyCode::Down => p.sel += 1,
                KeyCode::Backspace => {
                    p.query.pop();
                    p.sel = 0;
                }
                KeyCode::Char(c) => {
                    p.query.push(c);
                    p.sel = 0;
                }
                KeyCode::Enter => {
                    let hits = filter_files(&p.files, &p.query);
                    let pick = hits.get(p.sel.min(hits.len().saturating_sub(1))).map(|f| f.1.clone());
                    let lsp = p.lsp;
                    self.xt.picker = None;
                    return pick.map(|f| self.editor_job(&f, lsp));
                }
                _ => {}
            }
            return None;
        }
        if let Some((title, repo, field)) = &mut self.xt.add {
            let cur = if *field == 0 { title } else { repo };
            match k.code {
                KeyCode::Esc => self.xt.add = None,
                KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down => *field = 1 - *field,
                KeyCode::Backspace => {
                    cur.pop();
                }
                KeyCode::Char(c) => cur.push(c),
                KeyCode::Enter => {
                    let (title, repo, _) = self.xt.add.take().unwrap();
                    if title.trim().is_empty() {
                        self.toast("a task needs a title");
                        return None;
                    }
                    let repo = Some(repo.trim().to_string()).filter(|r| !r.is_empty());
                    if let Some(r) = &repo {
                        if !crate::config::expand(r).is_dir() {
                            self.toast(format!("{r} is not a directory"));
                            self.xt.add = Some((title, r.clone(), 1));
                            return None;
                        }
                    }
                    self.post_xt("add", json!({"text": title.trim(), "repo": repo}));
                    self.toast(format!("added: {} (t edits the details)", title.trim()));
                }
                _ => {}
            }
            return None;
        }
        if let Some(f) = &mut self.xt.start {
            match k.code {
                KeyCode::Esc | KeyCode::Char('q') => self.xt.start = None,
                KeyCode::Char('h') | KeyCode::Right => f.hours = (f.hours + 1) % HOURS.len(),
                KeyCode::Left => f.hours = (f.hours + HOURS.len() - 1) % HOURS.len(),
                KeyCode::Char('a') => f.away = !f.away,
                KeyCode::Char('d') => f.dry = !f.dry,
                KeyCode::Char('s') => f.serve = !f.serve,
                KeyCode::Char('l') => f.lan = !f.lan,
                KeyCode::Enter => {
                    let hours = HOURS[f.hours];
                    let body = json!({
                        "hours": (hours > 0 && !f.away).then_some(hours as f64),
                        "away": f.away,
                        "dry_run": f.dry,
                        "serve": f.serve,
                        "lan": f.lan,
                        "agent_bin": crate::config::which(&self.cfg.xtreme.agent).map(|p| p.display().to_string()),
                        "path_env": std::env::var("PATH").unwrap_or_default(),
                    });
                    self.xt.start = None;
                    self.post_xt("start", body);
                    self.toast("starting XTreme…");
                }
                _ => {}
            }
            return None;
        }
        if self.xt.wall {
            if k.code == KeyCode::Char('q') {
                self.quit = true;
                return None;
            }
            if let Some(a) = self.xtreme_actions().into_iter().find(|a| a.keys.contains(&k.code)) {
                if let Some(why) = a.why_not {
                    self.toast(why);
                    return None;
                }
            }
            return self.xtreme_act(k.code);
        }
        self.xtreme_act(k.code)
    }

    /// Wall mode, every tick: the panel off after the idle time; the wall leaves when the run
    /// it showed on the console has ended (the desktop is back).
    pub(super) fn xtreme_tick(&mut self) {
        if !self.xt.wall {
            return;
        }
        let st = self.xstate();
        if st.phase == Phase::Ended && ascii() && self.xt.last_key.is_none_or(|t| t.elapsed() > Duration::from_secs(30)) {
            self.quit = true;
            return;
        }
        let idle = self.cfg.xtreme.screen_idle_secs as u64;
        let last = *self.xt.last_key.get_or_insert_with(Instant::now);
        if idle > 0 && st.phase.active() && st.phase != Phase::Countdown && st.screen == "on" && last.elapsed() >= Duration::from_secs(idle) {
            self.xt.last_key = Some(Instant::now());
            self.post_xt("screen", json!({"on": false}));
        }
    }

    pub(super) fn xtreme_after_job(&mut self) {
        if std::mem::take(&mut self.xt.resume_after) {
            self.post_xt("resume", json!({}));
        }
        self.xt.last_key = Some(Instant::now());
    }

    pub(super) fn draw_xtreme(&mut self, f: &mut Frame, area: Rect) {
        let x = self.xview();
        let st = st_of(&x);
        // With the panel off nothing draws.
        if self.xt.wall && st.phase.active() && st.screen == "off" {
            f.render_widget(Clear, area);
            return;
        }
        let keys = if !self.xt.wall {
            ""
        } else if st.phase == Phase::Countdown {
            "any key: one more hour   Enter / X: back to the desktop now"
        } else if !st.phase.active() {
            "S start   a add a task   t tasks   r report   q leave"
        } else {
            WALL_KEYS
        };
        let lines = wall_text(&x, self.status.as_ref(), area.width as usize, area.height as usize, ascii(), keys);
        let n = lines.len();
        let styled: Vec<Line> = lines
            .into_iter()
            .enumerate()
            .map(|(i, l)| {
                let sep = l.chars().all(|c| c == '-' || c == '─') && !l.is_empty();
                let style = if i < 4 {
                    Style::new().fg(Color::Cyan)
                } else if sep {
                    Style::new().fg(Color::DarkGray)
                } else if l.starts_with("BACK TO") || l.contains("| BACK TO") || l.contains("│ BACK TO") {
                    Style::new().fg(Color::Yellow)
                } else if i + 1 == n && !keys.is_empty() {
                    Style::new().fg(Color::Green)
                } else {
                    Style::new()
                };
                Line::styled(l, style)
            })
            .collect();
        f.render_widget(Paragraph::new(styled), area);
        if self.xt.wall {
            if let Some((t, _)) = &self.toast {
                let r = Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1);
                f.render_widget(Paragraph::new(t.clone()).style(Style::new().fg(Color::Yellow)), r);
            }
        }
        if let Some(p) = &self.xt.picker {
            let hits = filter_files(&p.files, &p.query);
            let w = area.width.saturating_sub(8).min(110);
            let h = area.height.saturating_sub(4).min(30);
            let r = Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h);
            let rows = h.saturating_sub(3) as usize;
            let sel = p.sel.min(hits.len().saturating_sub(1));
            let first = sel.saturating_sub(rows.saturating_sub(1));
            let mut lines = vec![Line::from(format!("> {}_", p.query))];
            for (i, (label, _)) in hits.iter().enumerate().skip(first).take(rows) {
                let style = if i == sel { Style::new().fg(Color::Black).bg(Color::Cyan) } else { Style::new() };
                lines.push(Line::styled(fit(label, w as usize - 2), style));
            }
            if hits.is_empty() {
                lines.push(Line::from("no file matches"));
            }
            let title = format!(" open in the editor{}: type to filter, Enter opens, Esc closes ", if p.lsp { " (with LSP)" } else { "" });
            f.render_widget(Clear, r);
            f.render_widget(Paragraph::new(lines).block(block(&title)), r);
        }
        if let Some((title, repo, field)) = &self.xt.add {
            let w = area.width.saturating_sub(4).min(90);
            let r = Rect::new(area.x + (area.width - w) / 2, area.y + area.height.saturating_sub(8) / 2, w, 8.min(area.height));
            let caret = |i: usize| if *field == i { "_" } else { "" };
            let lines = vec![
                Line::from(format!("  title  {title}{}", caret(0))),
                Line::from(format!("  repo   {repo}{}", caret(1))),
                Line::from(""),
                Line::styled("  repo: a directory (git: the task gets its own branch); empty = none", Style::new().fg(Color::DarkGray)),
                Line::styled("Enter adds   Tab: next field   Esc cancels", Style::new().fg(Color::Green)),
            ];
            f.render_widget(Clear, r);
            f.render_widget(Paragraph::new(lines).block(block(" Add a task ")), r);
        }
        if let Some(form) = &self.xt.start {
            let w = area.width.saturating_sub(4).min(76);
            let r = Rect::new(area.x + (area.width - w) / 2, area.y + area.height.saturating_sub(14) / 2, w, 14.min(area.height));
            let hours = HOURS[form.hours];
            let lines = vec![
                Line::from("The desktop stops, the screen turns off, the model reloads into the freed"),
                Line::from("memory, and the agent works through the tasks. Ctrl+Alt+F2 or ssh reach it."),
                Line::from(""),
                Line::from(format!("  end (h, ←/→)   {}", if form.away { "none (away mode)".to_string() } else if hours == 0 { "none: lmdeck xtreme stop ends it".to_string() } else { format!("after {hours} h, then the desktop comes back") })),
                Line::from(format!("  server (s)     {}", if form.serve { "yes: the model stays loaded; tasks optional" } else { "no: the agent works through the tasks" })),
                Line::from(format!("  network (l)    {}", if form.lan { format!("yes: model APIs on port {}, with a token", self.cfg.xtreme.lan_port) } else { "no: this machine only (and ssh)".into() })),
                Line::from(format!("  away (a)       {}", if form.away { "yes: Tailscale, 80% charge limit, watchdog" } else { "no" })),
                Line::from(format!("  dry run (d)    {}", if form.dry { "yes: desktop, screen and model left alone" } else { "no" })),
                Line::from(format!("  tasks          {}", super::widgets::tilde(&xt::tasks_path(&self.cfg).display().to_string()))),
                Line::from(""),
                Line::styled("Enter starts   Esc cancels", Style::new().fg(Color::Green)),
            ];
            f.render_widget(Clear, r);
            f.render_widget(Paragraph::new(lines).block(block(" Start XTreme ")), r);
        }
    }
}

fn block(title: &str) -> Block<'static> {
    let b = Block::bordered().title(title.to_string());
    if ascii() {
        b.border_set(ratatui::symbols::border::Set {
            top_left: "+", top_right: "+", bottom_left: "+", bottom_right: "+",
            vertical_left: "|", vertical_right: "|", horizontal_top: "-", horizontal_bottom: "-",
        })
    } else {
        b
    }
}

/// Files of the task repositories: what git shows as changed first, then the rest, newest first.
fn pick_files(st: &State) -> Vec<(String, PathBuf)> {
    let mut dirs: Vec<PathBuf> = st.tasks.iter().map(|t| t.workdir()).filter(|d| d.is_dir()).collect();
    dirs.dedup();
    let mut changed = Vec::new();
    let mut rest: Vec<(std::time::SystemTime, String, PathBuf)> = Vec::new();
    for d in dirs {
        let name = d.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
        let git = |args: &[&str]| std::process::Command::new("git").arg("-C").arg(&d).args(args).output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
        let mut seen = std::collections::HashSet::new();
        for l in git(&["status", "--porcelain"]).unwrap_or_default().lines() {
            if let Some(f) = l.get(3..) {
                let f = f.rsplit(" -> ").next().unwrap_or(f).to_string();
                seen.insert(f.clone());
                changed.push((format!("* {name}/{f}"), d.join(&f)));
            }
        }
        for f in git(&["ls-files"]).unwrap_or_default().lines().take(20_000) {
            if seen.contains(f) {
                continue;
            }
            let p = d.join(f);
            let t = p.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
            rest.push((t, format!("  {name}/{f}"), p));
        }
    }
    rest.sort_by(|a, b| b.0.cmp(&a.0));
    changed.extend(rest.into_iter().map(|(_, l, p)| (l, p)));
    changed
}

fn filter_files<'a>(files: &'a [(String, PathBuf)], q: &str) -> Vec<&'a (String, PathBuf)> {
    let q = q.to_lowercase();
    files.iter().filter(|(l, _)| q.split_whitespace().all(|w| l.to_lowercase().contains(w))).take(500).collect()
}

// ---------- jobs: editors and shells run from the wall ----------

/// A POST to the daemon from a plain thread (the wall's runtime is parked while a job runs).
fn post_blocking(base: &str, path: &str, body: &str) {
    use std::io::Write;
    let host = base.trim_start_matches("http://").trim_end_matches('/');
    if let Ok(mut s) = std::net::TcpStream::connect(host) {
        let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
        let _ = write!(s, "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        let _ = std::io::Read::read(&mut s, &mut [0u8; 512]);
    }
}

/// Seconds since the terminal last had input (its tty's access time, as `w` reports idle).
fn tty_idle() -> Option<u64> {
    let t = std::fs::metadata("/proc/self/fd/0").ok()?.accessed().ok()?;
    Some(t.elapsed().unwrap_or_default().as_secs())
}

fn has_children(pid: u32) -> bool {
    std::fs::read_dir(format!("/proc/{pid}/task")).into_iter().flatten().flatten().any(|t| {
        std::fs::read_to_string(t.path().join("children")).is_ok_and(|c| !c.trim().is_empty())
    })
}

/// Run a job in this terminal. With `idle_secs`, a watcher turns the panel off once the terminal
/// has had no input that long, and with it closes what it can: nvim saves its session and swap
/// files (unsaved changes stay recoverable, your files are not written) and quits; a shell that
/// runs nothing is closed. The next key reopens the editor.
pub fn run_job(job: Job) {
    let Ok(mut child) = std::process::Command::new(&job.argv[0]).args(&job.argv[1..]).spawn() else {
        eprintln!("could not run {}", job.argv[0]);
        std::thread::sleep(Duration::from_secs(2));
        return;
    };
    let pid = child.id();
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watcher = (job.idle_secs > 0).then(|| {
        let job = job.clone();
        let done = done.clone();
        std::thread::spawn(move || {
            while !done.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(5));
                if tty_idle().is_none_or(|i| i < job.idle_secs) {
                    continue;
                }
                match &job.kind {
                    JobKind::Editor { sock, session, reopen } => {
                        let keys = format!("<C-\\><C-N>:silent! mksession! {}<CR>:silent! preserve<CR>", session.display());
                        let _ = std::process::Command::new("nvim").arg("--server").arg(sock).args(["--remote-send", &keys]).status();
                        std::thread::sleep(Duration::from_millis(1500));
                        // SIGKILL, not :qa!: a clean exit deletes the swap files that hold unsaved changes.
                        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
                        let _ = std::fs::remove_file(sock);
                        *REOPEN.lock().unwrap() = Some(Job { argv: reopen.clone(), ..job.clone() });
                    }
                    JobKind::Shell if !has_children(pid) => unsafe {
                        libc::kill(pid as i32, libc::SIGHUP);
                    },
                    _ => {}
                }
                post_blocking(&job.base, "/lmdeck/xtreme/screen", r#"{"on":false}"#);
                return;
            }
        })
    });
    let _ = child.wait();
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    drop(watcher);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> Value {
        let mut st = State { phase: Phase::Running, model: "strata/q2_0".into(), started: 1000, ends_at: Some(1000 + 8 * 3600), arena_gb: Some(14.8), screen: "on".into(), desktop_stopped: true, ..Default::default() };
        for (i, title) in ["lmdeck: grader NaN", "strata: tail chunk", "panel: scroll on hover", "r2: find the CRC in fw.bin"].iter().enumerate() {
            st.tasks.push(TaskState { slug: xt::slugify(title), title: title.to_string(), status: if i < 2 { "done" } else { "pending" }.into(), commits: if i < 2 { 2 } else { 0 }, sessions: if i < 3 { 1 } else { 0 }, ..Default::default() });
        }
        st.tasks[2].status = "running".into();
        st.session = Some(xt::Session { slug: st.tasks[2].slug.clone(), n: 2, turns: 9, max_turns: 40, deadline: 1000 + 8000 + 1800, last_tool: "bash       cargo test".into(), ..Default::default() });
        st.log = vec!["02:13 read       LmdeckSection.qml".into(), "02:13 bash       qmllint LmdeckSection.qml".into()];
        json!({"state": st, "now": 1000 + 8000, "elapsed_secs": 8000, "current": st.tasks[2].slug, "decode_tps": 12.1,
               "progress_tail": ["- hover handler is in LmdeckSection.qml", "- Flickable jumps on resize"], "ports": [22], "power": {"ac": true, "charge": 92}})
    }

    pub(super) fn view_pub() -> Value {
        view()
    }
    pub(super) fn status_pub() -> Value {
        status()
    }

    fn status() -> Value {
        json!({"mem": {"total": 33_000_000_000u64, "available": 6_000_000_000u64}, "ips": [{"iface": "wlan0", "ip": "192.168.1.20"}],
               "running": [{"name": "strata/q2_0", "context": {"used": 21300, "limit": 32768},
                            "work": {"phase": "reading prompt", "prompt_done": 4100, "prompt_tokens": 6000, "tokens_per_sec": 128.0, "eta_secs": 15.0}}]})
    }

    #[test]
    fn wall_fits_the_console_and_a_small_terminal() {
        for (w, h, ascii) in [(189, 61, true), (100, 30, false), (60, 16, true), (30, 8, true)] {
            let lines = wall_text(&view(), Some(&status()), w, h, ascii, WALL_KEYS);
            assert!(lines.len() <= h, "{w}x{h}: {} lines", lines.len());
            for l in &lines {
                assert!(l.chars().count() <= w, "{w}x{h}: {l:?}");
                if ascii {
                    assert!(l.is_ascii(), "{l:?}");
                }
            }
        }
        let text = wall_text(&view(), Some(&status()), 189, 61, true, WALL_KEYS).join("\n");
        for want in ["strata/q2_0", "2h13m of 8h00m", "arena 14.8 GB", "12.1 tok/s", "prompt 128 tok/s", "desktop off", "IP 192.168.1.20", "ports 22",
                     "2 of 4 done", "[x] 1 lmdeck: grader NaN", "[>] 3 panel: scroll on hover", "NOW  #3 panel: scroll on hover", "session 2, turn 9/40, 30m left",
                     "context  [", "21.3k / 32.8k", "reading the prompt 4.1k / 6.0k, 15s left", "last tool  bash", "PROGRESS #3", "Flickable jumps", "qmllint", "X end"] {
            assert!(text.contains(want), "missing {want:?} in\n{text}");
        }
    }

    #[test]
    fn a_server_run_shows_where_to_connect() {
        let mut v = view();
        v["state"]["serve"] = json!(true);
        v["state"]["lan"] = json!(true);
        v["state"]["tasks"] = json!([]);
        v["state"]["session"] = Value::Null;
        v["state"]["phase"] = json!("serving");
        v["lan"] = json!({"open": true, "urls": ["http://192.168.1.20:11435"], "token": "lmx-abc"});
        let mut s = status();
        s["usage"] = json!({"today": {"requests": 3, "prompt": 12000, "completion": 800}, "last": {"model": "strata/q2_0", "gen_tps": 11.5, "prompt_tps": 120.0, "ttft_secs": 40.0}});
        let text = wall_text(&v, Some(&s), 140, 30, true, WALL_KEYS).join("\n");
        for want in ["network http://192.168.1.20:11435   token lmx-abc", "none: serving only", "NOW  serving", "today  3 requests, 12.0k in", "last request  strata/q2_0: 11.5 tok/s"] {
            assert!(text.contains(want), "missing {want:?} in\n{text}");
        }
    }

    #[test]
    fn countdown_says_how_to_stay() {
        let mut v = view();
        v["state"]["phase"] = json!("countdown");
        v["countdown_secs"] = json!(42);
        let text = wall_text(&v, None, 120, 30, true, "").join("\n");
        assert!(text.contains("BACK TO THE DESKTOP IN 42 s") && text.contains("any key adds an hour"));
    }

    #[test]
    fn files_filter_by_every_word() {
        let files = vec![("* lmdeck/src/xtreme.rs".to_string(), PathBuf::from("/a")), ("  lmdeck/README.md".to_string(), PathBuf::from("/b"))];
        assert_eq!(filter_files(&files, "src xt").len(), 1);
        assert_eq!(filter_files(&files, "").len(), 2);
    }
}

#[cfg(test)]
mod print {
    /// `cargo test print_wall -- --ignored --nocapture`: the wall from sample state, as text.
    #[test]
    #[ignore]
    fn print_wall() {
        let w = std::env::var("W").ok().and_then(|v| v.parse().ok()).unwrap_or(120);
        let h = std::env::var("H").ok().and_then(|v| v.parse().ok()).unwrap_or(34);
        for l in super::wall_text(&super::tests::view_pub(), Some(&super::tests::status_pub()), w, h, true, super::WALL_KEYS) {
            println!("{l}");
        }
    }
}
