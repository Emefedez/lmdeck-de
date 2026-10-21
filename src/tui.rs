//! Terminal UI. Event-driven: redraws only on input or background updates,
//! polls the daemon once per second. Uses ANSI palette colors so the
//! Omarchy theme applies.

use crate::client::{self, Daemon};
use crate::config::{get_setting, log_dir, set_setting, Config, SETTINGS};
use crate::hf::{self, RepoFile, RepoHit};
use crate::human;
use crate::models::{self, Format, LocalModel};
use crate::sys::{self, Versions};
use anyhow::Result;
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell, Clear, Gauge, Paragraph, Row, Table, TableState, Tabs, Wrap};
use ratatui::{DefaultTerminal, Frame};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Dashboard,
    Models,
    Discover,
    Settings,
    Logs,
}
const TABS: [Tab; 5] = [Tab::Dashboard, Tab::Models, Tab::Discover, Tab::Settings, Tab::Logs];
impl Tab {
    fn title(self) -> &'static str {
        match self {
            Tab::Dashboard => "1 Dashboard",
            Tab::Models => "2 Models",
            Tab::Discover => "3 Discover",
            Tab::Settings => "4 Settings",
            Tab::Logs => "5 Logs",
        }
    }
}

enum Msg {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Resize,
    Tick,
    Status(Option<Value>),
    Versions(Box<Versions>),
    Search(u64, Result<(Vec<RepoHit>, usize), String>),
    Repo(String, Result<Vec<RepoFile>, String>),
    Toast(String),
    Models,
    /// Load failed for lack of a GPU kernel: (model, recommended download).
    GpuGap(String, Option<String>),
    /// Load refused for lack of memory: (model, daemon's explanation).
    NeedMemory(String, String),
    /// Memory preset finished: load this model now.
    RetryLoad(String),
}

#[derive(Clone)]
enum Action {
    Script(String, String), // title, bash
    Exec(String, Vec<String>),
    /// An editor: runs in the terminal and returns straight to lmdeck (no "press Enter").
    Edit(Vec<String>),
}

fn parse_search_budget(text: &str) -> Result<Option<u64>, String> {
    let text = text.trim();
    if text.is_empty() { return Ok(None); }
    let gib = text.parse::<f64>().map_err(|_| "Enter a positive RAM budget in GiB, or leave empty for unlimited".to_string())?;
    let bytes = gib * (1u64 << 30) as f64;
    if !gib.is_finite() || bytes < 1.0 || bytes >= u64::MAX as f64 {
        return Err("RAM budget must be a finite positive number in GiB".into());
    }
    Ok(Some(bytes as u64))
}

fn fmt_gib(b: u64) -> String {
    format!("{:.1} GiB", crate::backend::gib(b))
}

enum Input {
    None,
    Search,
    SearchBudget,
    Downloads(Vec<String>, usize),
    Setting(usize),
    Confirm(String, Confirmed),
    Update,
    Runtime,
    RuntimeKeep(String),
    Guide,
    Override(String),
    GpuGap(String, Option<String>),
    Free(Vec<sys::ProcGroup>, usize, Option<(String, String)>),
}

#[derive(Clone)]
enum Confirmed {
    ForceStop,
    Device(String, String),
    Delete(String),
    ResetPrompt(String),
    Run(Action),
}

struct RepoView {
    repo: String,
    files: Option<Vec<RepoFile>>,
    err: Option<String>,
    sel: TableState,
}

struct Ui {
    /// A prompt file open in the editor, and the text a new model file was seeded with.
    editing: Option<(std::path::PathBuf, Option<String>)>,
    tab: Tab,
    cfg: Config,
    d: Daemon,
    status: Option<Value>,
    versions: Versions,
    models: Vec<LocalModel>,
    models_sel: TableState,
    query: String,
    search_kind: usize,
    search_budget: Option<u64>,
    catalog_mode: bool,
    catalog: Vec<Value>,
    hits: Vec<RepoHit>,
    hits_sel: TableState,
    searching: bool,
    search_generation: u64,
    search_sort: usize,
    search_task: Option<tokio::task::JoinHandle<()>>,
    repo: Option<RepoView>,
    settings_sel: TableState,
    input: Input,
    buf: String,
    logs_model: bool,
    logs_scroll: u16,
    logs: String,
    toast: Option<(String, Instant)>,
    quit: bool,
    tx: mpsc::UnboundedSender<Msg>,
    clicks: Vec<(Rect, Click)>,
    popup_scroll: u16,
}

#[derive(Clone)]
enum Click {
    Key(KeyCode),
    Model(usize),
    Setting(usize),
    Discover(usize),
}

pub async fn run(cfg: Config) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
    let pause = Arc::new(AtomicBool::new(false));
    spawn_input(tx.clone(), pause.clone());
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if tx.send(Msg::Tick).is_err() {
                    break;
                }
            }
        });
    }
    let mut ui = Ui::new(cfg, tx.clone());
    ui.refresh_versions();
    ui.poll_status();

    let mut term = ratatui::init();
    let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture);
    let res = loop {
        if let Err(e) = term.draw(|f| ui.draw(f)) {
            break Err(e.into());
        }
        let Some(msg) = rx.recv().await else { break Ok(()) };
        let mut pending = vec![msg];
        while let Ok(m) = rx.try_recv() {
            pending.push(m);
        }
        let mut action = None;
        for m in pending {
            if let Some(a) = ui.handle(m) {
                action = Some(a);
            }
        }
        if let Some(a) = action {
            suspend_run(&mut term, &pause, a);
            ui.after_action();
        }
        if ui.quit {
            break Ok(());
        }
    };
    let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    res
}

fn spawn_input(tx: mpsc::UnboundedSender<Msg>, pause: Arc<AtomicBool>) {
    std::thread::spawn(move || loop {
        if pause.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        if event::poll(Duration::from_millis(100)).unwrap_or(false) {
            let m = match event::read() {
                Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => Msg::Key(k),
                Ok(Event::Resize(..)) => Msg::Resize,
                Ok(Event::Mouse(m)) if matches!(m.kind, MouseEventKind::Down(MouseButton::Left) | MouseEventKind::ScrollDown | MouseEventKind::ScrollUp) => Msg::Mouse(m),
                _ => continue,
            };
            if tx.send(m).is_err() {
                return;
            }
        }
    });
}

/// Leave the TUI, run an interactive command (sudo prompts work), come back.
fn suspend_run(term: &mut DefaultTerminal, pause: &AtomicBool, a: Action) {
    pause.store(true, Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(150)); // let the input thread park
    let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    if let Action::Edit(argv) = &a {
        let _ = std::process::Command::new(&argv[0]).args(&argv[1..]).status();
        *term = ratatui::init();
        let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture);
        let _ = term.clear();
        pause.store(false, Ordering::Relaxed);
        return;
    }
    let (title, mut cmd) = match a {
        Action::Edit(_) => unreachable!(),
        Action::Script(t, s) => {
            let mut c = std::process::Command::new("bash");
            c.arg("-c").arg(s);
            (t, c)
        }
        Action::Exec(t, argv) => {
            let mut c = std::process::Command::new(&argv[0]);
            c.args(&argv[1..]);
            (t, c)
        }
    };
    println!("\x1b[1;36m── lmdeck: {title} ──\x1b[0m\n");
    let st = cmd.status();
    match st {
        Ok(s) if s.success() => println!("\n\x1b[1;32m✓ finished\x1b[0m"),
        Ok(s) => println!("\n\x1b[1;31m✗ exited with {s}\x1b[0m"),
        Err(e) => println!("\n\x1b[1;31m✗ {e}\x1b[0m"),
    }
    print!("Press Enter to return to lmdeck…");
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    let _ = std::io::stdin().read_line(&mut s);
    *term = ratatui::init();
    let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture);
    let _ = term.clear();
    pause.store(false, Ordering::Relaxed);
}

// ---------- state & events ----------

impl Ui {
    fn new(cfg: Config, tx: mpsc::UnboundedSender<Msg>) -> Self {
        Self {
            editing: None,
            tab: Tab::Dashboard,
            d: Daemon::new(cfg.base_url()),
            models: models::scan(&cfg),
            catalog: crate::compat::catalog().to_vec(),
            cfg,
            status: None,
            versions: Versions::default(),
            models_sel: TableState::default().with_selected(Some(0)),
            query: String::new(),
            search_kind: 0,
            search_budget: None,
            catalog_mode: true,
            hits: vec![],
            hits_sel: TableState::default().with_selected(Some(0)),
            searching: false,
            search_generation: 0,
            search_sort: 0,
            search_task: None,
            repo: None,
            settings_sel: TableState::default().with_selected(Some(0)),
            input: Input::None,
            buf: String::new(),
            logs_model: false,
            logs_scroll: 0,
            logs: String::new(),
            toast: None,
            quit: false,
            tx,
            clicks: vec![],
            popup_scroll: 0,
        }
    }

    fn toast(&mut self, s: impl Into<String>) {
        self.toast = Some((s.into(), Instant::now()));
    }

    fn poll_status(&self) {
        let d = self.d.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let _ = tx.send(Msg::Status(d.status().await));
        });
    }

    fn refresh_versions(&self) {
        let cfg = self.cfg.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let mut v = tokio::task::spawn_blocking(move || sys::versions_local(&cfg)).await.unwrap_or_default();
            let _ = tx.send(Msg::Versions(Box::new(v.clone())));
            v.mlx_upstream = sys::upstream_release(&hf::client()).await;
            let _ = tx.send(Msg::Versions(Box::new(v)));
        });
    }

    fn rescan(&mut self) {
        self.models = models::scan(&self.cfg);
        let n = self.models.len();
        if self.models_sel.selected().map(|s| s >= n).unwrap_or(true) {
            self.models_sel.select(if n > 0 { Some(n - 1) } else { None });
        }
    }

    fn running_names(&self) -> Vec<String> {
        self.status
            .as_ref()
            .and_then(|s| s["running"].as_array())
            .map(|a| a.iter().filter_map(|r| r["name"].as_str().map(str::to_string)).collect())
            .unwrap_or_default()
    }

    fn spawn_daemon_op<F, Fut>(&self, label: String, f: F)
    where
        F: FnOnce(Daemon) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<String>> + Send,
    {
        let d = self.d.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            if !d.up().await {
                let _ = tx.send(Msg::Toast("starting daemon…".into()));
                if let Err(e) = client::start_daemon() {
                    let _ = tx.send(Msg::Toast(format!("daemon start failed: {e}")));
                    return;
                }
                for _ in 0..50 {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    if d.up().await {
                        break;
                    }
                }
            }
            let _ = tx.send(Msg::Toast(label));
            let r = f(d.clone()).await;
            let _ = tx.send(Msg::Toast(match r {
                Ok(s) => s,
                Err(e) => format!("error: {e:#}"),
            }));
            let _ = tx.send(Msg::Status(d.status().await));
            let _ = tx.send(Msg::Models);
        });
    }

    fn after_action(&mut self) {
        if let Some((p, seed)) = self.editing.take() {
            let now = std::fs::read_to_string(&p).unwrap_or_default();
            match seed {
                // A new model file left as seeded (the backend's prompt): keep inheriting.
                Some(seed) if now == seed => {
                    let _ = std::fs::remove_file(&p);
                    self.toast("unchanged: still the backend's system prompt");
                }
                _ if now.trim().is_empty() && p.parent().is_some_and(|d| d.ends_with("models")) => {
                    self.toast("saved: no system prompt for this model (P = use the backend's)")
                }
                _ => self.toast(format!("saved {}", p.display())),
            }
        }

        self.cfg = Config::load().unwrap_or_else(|_| self.cfg.clone());
        self.refresh_versions();
        self.rescan();
        self.poll_status();
    }

    fn handle(&mut self, m: Msg) -> Option<Action> {
        match m {
            Msg::Tick => {
                self.poll_status();
                if self.tab == Tab::Logs {
                    self.load_logs();
                }
                if let Some((_, t)) = &self.toast {
                    if t.elapsed() > Duration::from_secs(6) {
                        self.toast = None;
                    }
                }
            }
            Msg::Resize => {}
            Msg::Status(s) => {
                // Rescan models when a download finishes.
                let was = count_active(&self.status);
                let now = count_active(&s);
                self.status = s;
                if now < was {
                    self.rescan();
                }
            }
            Msg::Versions(v) => self.versions = *v,
            Msg::Search(generation, r) => {
                if generation != self.search_generation { return None; }
                self.searching = false;
                match r {
                    Ok((h, unreadable)) => {
                        self.hits = h;
                        self.hits_sel.select(Some(0));
                        if unreadable > 0 { self.toast(format!("{unreadable} repos could not be checked and were excluded")); }
                    }
                    Err(e) => { self.hits.clear(); self.toast(format!("search failed: {e}")); },
                }
            }
            Msg::Repo(repo, r) => {
                if let Some(rv) = &mut self.repo {
                    if rv.repo == repo {
                        match r {
                            Ok(f) => rv.files = Some(f),
                            Err(e) => rv.err = Some(e),
                        }
                    }
                }
            }
            Msg::Toast(s) => self.toast(s),
            Msg::RetryLoad(model) => {
                self.input = Input::None;
                self.load_model(model);
            }
            Msg::NeedMemory(model, why) => {
                if why.contains("needs ~") { self.toast(format!("{model}: not enough free memory")); }
                let groups = sys::proc_groups(unsafe { libc::getuid() }, true);
                self.input = Input::Free(groups, 0, Some((model, why)));
            }
            Msg::GpuGap(model, rec) => {
                self.toast(format!("{model}: no GPU kernel for one of its ops"));
                self.input = Input::GpuGap(model, rec);
            }
            Msg::Models => {
                self.cfg = Config::load().unwrap_or_else(|_| self.cfg.clone());
                self.rescan();
            }
            Msg::Key(k) => return self.key(k),
            Msg::Mouse(m) => return self.mouse(m),
        }
        None
    }

    fn key(&mut self, k: KeyEvent) -> Option<Action> {
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            self.quit = true;
            return None;
        }
        // Text input modes take all keys.
        if !matches!(self.input, Input::None) {
            return self.key_input(k);
        }
        match k.code {
            KeyCode::Char('x') if matches!(self.tab, Tab::Dashboard | Tab::Discover) => {
                let specs: Vec<_> = active_downloads(&self.status).into_iter().map(|d| d.0).collect();
                if specs.is_empty() { self.toast("no active downloads"); }
                else { self.input = Input::Downloads(specs, 0); }
            }
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char(c @ '1'..='5') => self.tab = TABS[(c as u8 - b'1') as usize],
            KeyCode::Tab => self.tab = TABS[(TABS.iter().position(|t| *t == self.tab).unwrap() + 1) % TABS.len()],
            KeyCode::BackTab => self.tab = TABS[(TABS.iter().position(|t| *t == self.tab).unwrap() + TABS.len() - 1) % TABS.len()],
            _ => {
                return match self.tab {
                    Tab::Dashboard => self.key_dashboard(k),
                    Tab::Models => self.key_models(k),
                    Tab::Discover => self.key_discover(k),
                    Tab::Settings => self.key_settings(k),
                    Tab::Logs => self.key_logs(k),
                }
            }
        }
        if self.tab == Tab::Logs {
            self.load_logs();
        }
        None
    }

    fn mouse(&mut self, m: MouseEvent) -> Option<Action> {
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let click = self.clicks.iter().rev()
                    .find(|(r, _)| m.column >= r.x && m.column < r.right() && m.row >= r.y && m.row < r.bottom())
                    .map(|(_, c)| c.clone());
                if click.is_some() { self.clicks.clear(); }
                match click {
                    Some(Click::Key(k)) => return self.key(KeyEvent::new(k, KeyModifiers::NONE)),
                    Some(Click::Model(i)) => self.models_sel.select(Some(i)),
                    Some(Click::Discover(i)) => self.hits_sel.select(Some(i)),
                    Some(Click::Setting(i)) => {
                        self.settings_sel.select(Some(i));
                        return self.key_settings(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                    }
                    None => {}
                }
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp if matches!(self.input, Input::Guide) => {
                self.popup_scroll = if m.kind == MouseEventKind::ScrollDown { self.popup_scroll.saturating_add(3) } else { self.popup_scroll.saturating_sub(3) };
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp if matches!(self.input, Input::None) => {
                let code = if m.kind == MouseEventKind::ScrollDown { KeyCode::Down } else { KeyCode::Up };
                for _ in 0..3 { self.key(KeyEvent::new(code, KeyModifiers::NONE)); }
            }
            _ => {}
        }
        None
    }

    fn key_input(&mut self, k: KeyEvent) -> Option<Action> {
        let input = std::mem::replace(&mut self.input, Input::None);
        if matches!(input, Input::Guide) {
            return match k.code {
                KeyCode::Esc | KeyCode::Char('q') => None,
                KeyCode::Char('U' | 'B') => self.key_dashboard(k),
                KeyCode::Down | KeyCode::PageDown => { self.popup_scroll = self.popup_scroll.saturating_add(3); self.input = Input::Guide; None }
                KeyCode::Up | KeyCode::PageUp => { self.popup_scroll = self.popup_scroll.saturating_sub(3); self.input = Input::Guide; None }
                _ => { self.input = Input::Guide; None }
            };
        }
        if let Input::Downloads(specs, mut sel) = input {
            match k.code {
                KeyCode::Esc | KeyCode::Char('q') => return None,
                KeyCode::Down | KeyCode::Char('j') => sel = (sel + 1).min(specs.len().saturating_sub(1)),
                KeyCode::Up | KeyCode::Char('k') => sel = sel.saturating_sub(1),
                KeyCode::Enter | KeyCode::Char('x') => {
                    if let Some(spec) = specs.get(sel).cloned() {
                        self.spawn_daemon_op(format!("cancelling {spec}"), move |d| async move {
                            let result = d.cancel(&spec).await?;
                            Ok(if result["ok"] == true { format!("cancelled {spec}; pull again to resume") } else { "download already finished".into() })
                        });
                    }
                    return None;
                }
                _ => {}
            }
            self.input = Input::Downloads(specs, sel);
            return None;
        }
        if let Input::Free(mut groups, mut sel, ctx) = input {
            match k.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('f') => return None,
                KeyCode::Down | KeyCode::Char('j') => sel = (sel + 1).min(groups.len().saturating_sub(1)),
                KeyCode::Up | KeyCode::Char('k') => sel = sel.saturating_sub(1),
                KeyCode::Char(ch @ ('[' | ']' | '{' | '}')) => {
                    let mut c = self.cfg.clone();
                    let step = if matches!(ch, ']' | '}') { 0.5 } else { -0.5 };
                    let (key, v) = if matches!(ch, '[' | ']') {
                        ("server.mem_reserve_gib", &mut c.server.mem_reserve_gib)
                    } else {
                        ("server.vm_reserve_gib", &mut c.server.vm_reserve_gib)
                    };
                    *v = (*v + step).clamp(if key == "server.mem_reserve_gib" { 0.5 } else { 0.0 }, 8.0);
                    if c.save().is_ok() {
                        self.cfg = c;
                        self.reload_daemon(key);
                    }
                }
                KeyCode::Char(ch @ ('-' | '+' | '=')) => {
                    self.nudge_setting("server.gpu_heap", if ch == '-' { -1.0 } else { 1.0 });
                }
                KeyCode::Char('p') if ctx.is_some() => {
                    let model = ctx.as_ref().map(|c| c.0.clone()).unwrap_or_default();
                    self.make_room(model);
                    self.input = Input::None;
                    return None;
                }
                KeyCode::Char('l') if ctx.is_some() => {
                    let model = ctx.map(|c| c.0).unwrap_or_default();
                    self.toast(format!("retrying {model}…"));
                    self.load_model(model);
                    return None;
                }
                KeyCode::Char('r') => { groups = sys::proc_groups(unsafe { libc::getuid() }, ctx.is_some()); sel = sel.min(groups.len().saturating_sub(1)); }
                KeyCode::Enter | KeyCode::Char('x') | KeyCode::Char('X') => {
                    if let Some(g) = groups.get(sel).cloned() {
                        if g.tweak.is_some_and(|(id, _)| id == "preset") {
                            if let Some((model, _)) = &ctx {
                                self.toast("preset: making room…");
                                self.make_room(model.clone());
                                return None;
                            }
                        } else if let Some((id, _)) = g.tweak.filter(|(id, _)| *id != "kill") {
                            let msg = sys::run_tweak(id);
                            self.toast(msg);
                            groups = sys::proc_groups(unsafe { libc::getuid() }, ctx.is_some());
                        } else if let Some(why) = g.protected {
                            self.toast(format!("{} is protected ({why})", g.name));
                        } else {
                            let sig = if k.code == KeyCode::Char('X') { libc::SIGKILL } else { libc::SIGTERM };
                            for pid in &g.pids {
                                unsafe { libc::kill(*pid as i32, sig) };
                            }
                            self.toast(format!("sent {} to {} ({} processes, {})", if sig == libc::SIGKILL { "SIGKILL" } else { "SIGTERM" }, g.name, g.pids.len(), fmt_gib(g.rss)));
                            std::thread::sleep(Duration::from_millis(400));
                            groups = sys::proc_groups(unsafe { libc::getuid() }, ctx.is_some());
                            sel = sel.min(groups.len().saturating_sub(1));
                        }
                    }
                }
                _ => {}
            }
            self.input = Input::Free(groups, sel, ctx);
            return None;
        }
        if matches!(input, Input::Runtime) {
            self.input = match k.code {
                KeyCode::Char('1') => Input::RuntimeKeep("omarchy".into()),
                KeyCode::Char('2') => Input::RuntimeKeep("custom".into()),
                KeyCode::Char('3') => {
                    // The Fixed runtime has its own folder: select it, nothing to keep or remove.
                    let exe = std::env::current_exe().ok().map(|p| p.display().to_string()).unwrap_or_else(|| "lmdeck".into());
                    return Some(Action::Exec("select Fixed MLX runtime".into(), vec![exe, "runtime".into(), "fixed".into()]));
                }
                KeyCode::Char('q') | KeyCode::Esc => Input::None,
                _ => Input::Runtime,
            };
            return None;
        }
        if let Input::RuntimeKeep(source) = input {
            match k.code {
                KeyCode::Char('1' | '2') => {
                    let only = k.code == KeyCode::Char('2');
                    let mut args = vec![std::env::current_exe().ok()?.display().to_string(), "runtime".into(), source.clone()];
                    if only { args.push("--only".into()); }
                    let action = Action::Exec(format!("select {source} MLX runtime"), args);
                    if only {
                        self.input = Input::Confirm(format!("Select {source} and remove the other MLX runtime? Models, chat data and the GPU driver are kept. Unload MLX models first. [y/N]"), Confirmed::Run(action));
                    } else {
                        return Some(action);
                    }
                }
                KeyCode::Char('q') | KeyCode::Esc => {}
                _ => self.input = Input::RuntimeKeep(source),
            }
            return None;
        }
        if matches!(input, Input::Update) {
            return match k.code {
                KeyCode::Char('1') => Some(Action::Script("update MLX via Omarchy".into(), sys::script_update_mlx())),
                KeyCode::Char('2') => Some(Action::Script("update MLX via Custom Installer".into(), sys::script_update_mlx_custom())),
                KeyCode::Char('3') => Some(Action::Script("build Fixed MLX runtime (release + lmdeck fixes)".into(), sys::script_build_mlx_fixed())),
                KeyCode::Char('q' | 'Q') | KeyCode::Esc => None,
                _ => { self.input = Input::Update; None }
            };
        }
        if let Input::GpuGap(model, rec) = &input {
            match k.code {
                KeyCode::Char('c') => self.set_device_and_load(model.clone(), "cpu"),
                KeyCode::Char('d') => {
                    if let Some(r) = rec.clone() {
                        self.spawn_daemon_op(format!("pulling {r}"), move |d| async move {
                            d.pull(&r).await?;
                            Ok(format!("downloading {r} (progress on Dashboard)"))
                        });
                    }
                }
                _ => {}
            }
            return None;
        }
        match (&input, k.code) {
            (Input::Confirm(_, c), KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter) => {
                let c = c.clone();
                return self.confirmed(c);
            }
            (Input::Confirm(..), _) => {}
            (_, KeyCode::Esc) => {}
            (_, KeyCode::Enter) => {
                let text = std::mem::take(&mut self.buf);
                match input {
                    Input::Search => {
                        self.query = text;
                        self.catalog_mode = false;
                        self.search();
                    }
                    Input::SearchBudget => {
                        match parse_search_budget(&text) {
                            Ok(budget) => {
                                self.search_budget = budget;
                                self.catalog_mode = false;
                                self.search();
                            }
                            Err(e) => {
                                self.buf = text;
                                self.input = Input::SearchBudget;
                                self.toast(e);
                            }
                        }
                    }
                    Input::Setting(i) => {
                        let key = SETTINGS[i].1;
                        let mut c = self.cfg.clone();
                        match set_setting(&mut c, key, &text).and_then(|_| c.save().map_err(|e| e.to_string())) {
                            Ok(()) => {
                                self.cfg = c;
                                self.reload_daemon(key);
                            }
                            Err(e) => self.toast(format!("{key}: {e}")),
                        }
                    }
                    Input::Override(name) => {
                        let mut c = self.cfg.clone();
                        let o = c.overrides.entry(name.clone()).or_default();
                        let t = text.trim();
                        o.ctx_size = if t.is_empty() { None } else { t.parse().ok() };
                        if c.save().is_ok() {
                            self.cfg = c;
                            self.reload_daemon("override");
                            self.toast(format!("{name}: context {}", if t.is_empty() { "default" } else { t }));
                        }
                    }
                    _ => {}
                }
            }
            (_, KeyCode::Backspace) => {
                self.buf.pop();
                self.input = input;
            }
            (_, KeyCode::Char(ch)) => {
                self.buf.push(ch);
                self.input = input;
            }
            _ => self.input = input,
        }
        None
    }

    fn confirmed(&mut self, c: Confirmed) -> Option<Action> {
        match c {
            Confirmed::Device(name, dev) => {
                self.set_device(name, dev, false);
                None
            }
            Confirmed::ForceStop => {
                let pid = self.status.as_ref().and_then(|s| s["pid"].as_u64()).map(|p| p as u32);
                match client::force_stop(pid) {
                    Ok(()) => self.toast("force-stopped lmdeck and its backends"),
                    Err(e) => self.toast(format!("force stop failed: {e}")),
                }
                None
            }
            Confirmed::ResetPrompt(name) => {
                let _ = std::fs::remove_file(crate::sysprompt::model_file(&name));
                self.toast(format!("{name}: system prompt from the backend again"));
                None
            }
            Confirmed::Delete(name) => {
                let cfg = self.cfg.clone();
                let tx = self.tx.clone();
                let d = self.d.clone();
                tokio::spawn(async move {
                    let r = if d.up().await {
                        d.delete(&name).await.map_err(|e| e.to_string())
                    } else {
                        let ms = models::scan(&cfg);
                        match models::resolve(&name, &ms, &cfg) {
                            Some(m) => models::delete(m, &cfg).map_err(|e| e.to_string()),
                            None => Err("not found".into()),
                        }
                    };
                    let _ = tx.send(Msg::Toast(match r {
                        Ok(()) => format!("deleted {name}"),
                        Err(e) => format!("delete failed: {e}"),
                    }));
                    let _ = tx.send(Msg::Models);
                });
                None
            }
            Confirmed::Run(a) => Some(a),
        }
    }

    /// Open the memory popup for `model` with the planner's current verdict.
    fn open_make_room(&self, model: String) {
        let d = self.d.clone();
        let tx = self.tx.clone();
        let url = format!("{}/lmdeck/plan?model={model}", self.cfg.base_url());
        tokio::spawn(async move {
            let v: Option<Value> = match d.http().get(url).send().await { Ok(r) => r.json().await.ok(), Err(_) => None };
            let why = match v {
                Some(v) if v["ok"].as_bool() == Some(true) => format!("{model} fits right now; press l to load it."),
                Some(v) => v["error"].as_str().unwrap_or("not enough memory").to_string(),
                None => "daemon not reachable".into(),
            };
            let _ = tx.send(Msg::NeedMemory(model, why));
        });
    }

    /// Slider step for the memory settings; returns false for other keys.
    fn nudge_setting(&mut self, key: &str, dir: f64) -> bool {
        let mut c = self.cfg.clone();
        let total_gib = sys::mem().0 as f64 / (1u64 << 30) as f64;
        match key {
            "server.mem_reserve_gib" => c.server.mem_reserve_gib = (c.server.mem_reserve_gib + 0.5 * dir).clamp(0.5, 8.0),
            "server.vm_reserve_gib" => c.server.vm_reserve_gib = (c.server.vm_reserve_gib + 0.5 * dir).clamp(0.0, 8.0),
            "server.gpu_heap" => {
                let stock = crate::compat::gpu_budget() as f64 / (1u64 << 30) as f64;
                let cur = match c.server.gpu_heap.trim() {
                    "auto" | "" => None,
                    "stock" => Some(stock),
                    n => n.parse::<f64>().ok(),
                };
                c.server.gpu_heap = match cur {
                    // Leaving "auto" starts from the stock heap.
                    None => format!("{:.1}", (stock * 2.0).round() / 2.0),
                    Some(v) => {
                        let next = ((v + 0.5 * dir) * 2.0).round() / 2.0;
                        if next < 4.0 { "auto".into() } else { format!("{:.1}", next.min((total_gib - 2.0).floor())) }
                    }
                };
            }
            _ => return false,
        }
        if c.save().is_ok() {
            self.cfg = c;
            self.reload_daemon(key);
        }
        true
    }

    /// Preset: free memory step by step (least disruptive first) until `model` fits, then load it.
    fn make_room(&self, model: String) {
        let d = self.d.clone();
        let tx = self.tx.clone();
        let base = self.cfg.base_url();
        tokio::spawn(async move {
            let fits = |d: &Daemon| {
                let url = format!("{base}/lmdeck/plan?model={model}");
                let http = d.http().clone();
                async move {
                    tokio::time::sleep(Duration::from_millis(1500)).await;
                    let v: Option<Value> = match http.get(url).send().await { Ok(r) => r.json().await.ok(), Err(_) => None };
                    v.and_then(|v| v["ok"].as_bool()).unwrap_or(false)
                }
            };
            let say = |m: String| { let _ = tx.send(Msg::Toast(m)); };
            let mut steps: Vec<&str> = vec!["reserves", "hypr-lite", "plugins-lite", "vm"];
            if fits(&d).await { steps.clear(); }
            for step in steps {
                match step {
                    "reserves" => {
                        if let Ok(mut c) = Config::load() {
                            c.server.mem_reserve_gib = c.server.mem_reserve_gib.min(1.5);
                            c.server.vm_reserve_gib = c.server.vm_reserve_gib.min(0.5);
                            let _ = c.save();
                            let _ = d.reload().await;
                            say("preset: reserves lowered to 1.5 GiB desktop + 0.5 GiB VM".into());
                        }
                    }
                    "vm" => {
                        let uid = unsafe { libc::getuid() };
                        let vm: Vec<u32> = sys::proc_groups(uid, false).iter()
                            .filter(|g| g.name == "muvm VM" || g.name == "muvm" || g.name == "muvm-run").flat_map(|g| g.pids.clone()).collect();
                        if !vm.is_empty() {
                            for pid in vm { unsafe { libc::kill(pid as i32, libc::SIGTERM) }; }
                            say("preset: closing the muvm VM…".into());
                            tokio::time::sleep(Duration::from_secs(3)).await;
                        }
                    }
                    id => { say(format!("preset: {}", sys::run_tweak(id))); }
                }
                if fits(&d).await { break; }
            }
            if fits(&d).await {
                say("preset: enough memory now, loading…".into());
                let _ = tx.send(Msg::RetryLoad(model));
            } else {
                say("preset: still not enough. Close big apps from the list (Enter on a row), then press l".into());
            }
        });
    }

    fn reload_daemon(&mut self, key: &str) {
        let d = self.d.clone();
        let restart = key.starts_with("server.host") || key.starts_with("server.port");
        tokio::spawn(async move {
            let _ = d.reload().await;
        });
        if restart {
            self.toast("saved — restart the daemon (s twice on Dashboard) to apply host/port");
            self.d = Daemon::new(self.cfg.base_url());
        } else {
            self.toast(format!("saved {key}"));
        }
    }

    fn key_dashboard(&mut self, k: KeyEvent) -> Option<Action> {
        let pid = self.status.as_ref().and_then(|s| s["pid"].as_u64()).map(|p| p as u32);
        match k.code {
            KeyCode::Char('s') => {
                if self.status.is_some() {
                    let _ = client::stop_daemon(pid);
                    self.toast("daemon stopping");
                } else {
                    match client::start_daemon() {
                        Ok(()) => self.toast("daemon starting"),
                        Err(e) => self.toast(format!("start failed: {e}")),
                    }
                }
            }
            KeyCode::Char('R') => self.spawn_daemon_op("reloading models to apply settings".into(), |d| async move {
                d.restart(None).await?;
                Ok("reloaded".into())
            }),
            KeyCode::Char('X') => {
                self.input = Input::Confirm(
                    "Force-stop lmdeck (SIGKILL server + all model backends)? [y/N]".into(),
                    Confirmed::ForceStop,
                );
            }
            KeyCode::Char('S') => match sys::install_service() {
                Ok(()) => self.toast("user service installed & enabled (starts at login)"),
                Err(e) => self.toast(format!("service install failed: {e}")),
            },
            KeyCode::Char('D') => match sys::desktop_entry().and_then(|_| sys::install_menu()) {
                Ok(_) => self.toast("launcher + Omarchy menu entries installed (apps › lmdeck, setup › Local LLM Server)"),
                Err(e) => self.toast(format!("failed: {e}")),
            },
            KeyCode::Char('a') => self.spawn_daemon_op("unloading all".into(), |d| async move {
                d.unload(None).await?;
                Ok("all models unloaded".into())
            }),
            KeyCode::Char('M') => {
                let ane = self.versions.ane;
                self.input = Input::Confirm(
                    format!(
                        "Switch to Omarchy packages{} and remove the user-installed MLX runtime? Models and chat data are kept. Requires sudo. [y/N]",
                        if ane { " (+ANE driver)" } else { "" }
                    ),
                    Confirmed::Run(Action::Script("migrate to omarchy-mlx packages".into(), sys::script_migrate(ane))),
                );
            }
            KeyCode::Char('U') => {
                self.input = Input::Update;
            }
            KeyCode::Char('i') => self.input = Input::Runtime,
            KeyCode::Char('f') => self.input = Input::Free(sys::proc_groups(unsafe { libc::getuid() }, false), 0, None),
            KeyCode::Char('?') => { self.popup_scroll = 0; self.input = Input::Guide; }
            KeyCode::Char('B') => {
                self.input = Input::Confirm(
                    format!("Set up the optional GGUF backend (llama.cpp, {})? Builds with Vulkan GPU support; takes a few minutes. MLX models do not need this. [y/N]", self.cfg.llama.git_ref),
                    Confirmed::Run(Action::Script("build llama.cpp (Vulkan)".into(), sys::script_build_llama(&self.cfg))),
                );
            }
            KeyCode::Char('T') => {
                self.input = Input::Confirm(
                    format!("Install or update Strata (Qwen3.8-Flash-Next, {} branch {})? Clones it, sets up its server and builds the Apple GPU Vulkan driver it runs on. The model itself is not downloaded. [y/N]", self.cfg.strata.repo, self.cfg.strata.git_ref),
                    Confirmed::Run(Action::Script("install Strata".into(), sys::script_install_strata(&self.cfg))),
                );
            }
            KeyCode::Char('C') => {
                let d = self.d.clone();
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let _ = d.clear_downloads().await;
                    let _ = tx.send(Msg::Status(d.status().await));
                });
            }
            KeyCode::Char('r') => {
                let _ = std::fs::remove_file(crate::config::state_dir().join("upstream-release"));
                self.refresh_versions();
                self.toast("checking versions…");
            }
            _ => {}
        }
        None
    }

    fn selected_model(&self) -> Option<&LocalModel> {
        self.models_sel.selected().and_then(|i| self.models.get(i))
    }

    fn key_models(&mut self, k: KeyEvent) -> Option<Action> {
        let n = self.models.len();
        match k.code {
            KeyCode::Char('m') => {
                if let Some(m) = self.selected_model() {
                    self.open_make_room(m.name.clone());
                }
            }
            KeyCode::Down | KeyCode::Char('j') => move_sel(&mut self.models_sel, n, 1),
            KeyCode::Up | KeyCode::Char('k') => move_sel(&mut self.models_sel, n, -1),
            KeyCode::Char('r') => {
                self.rescan();
                self.toast(format!("{} models", self.models.len()));
            }
            KeyCode::Enter | KeyCode::Char('l') => {
                if let Some(m) = self.selected_model() {
                    self.load_model(m.name.clone());
                }
            }
            KeyCode::Char('g') => {
                if let Some(m) = self.selected_model() {
                    let name = m.name.clone();
                    if self.device_of(&name) == "cpu" {
                        self.set_device(name, "gpu".into(), false);
                    } else {
                        self.input = Input::Confirm(
                            format!("Run {name} on the CPU? Discouraged: much slower than the Apple GPU. [y/N]"),
                            Confirmed::Device(name, "cpu".into()),
                        );
                    }
                }
            }
            KeyCode::Char('u') => {
                if let Some(m) = self.selected_model() {
                    let name = m.name.clone();
                    self.spawn_daemon_op(format!("unloading {name}"), move |d| async move {
                        d.unload(Some(&name)).await?;
                        Ok(format!("{name} unloaded"))
                    });
                }
            }
            KeyCode::Char('d') | KeyCode::Delete => {
                if let Some(m) = self.selected_model() {
                    self.input = Input::Confirm(format!("Delete {} ({})? [y/N]", m.name, human(m.size)), Confirmed::Delete(m.name.clone()));
                }
            }
            KeyCode::Char('c') => {
                if let Some(m) = self.selected_model() {
                    let exe = sys::exe_path().display().to_string();
                    return Some(Action::Exec(format!("chat with {}", m.name), vec![exe, "run".into(), m.name.clone()]));
                }
            }
            KeyCode::Char('x') => {
                if let Some(name) = self.selected_model().map(|m| m.name.clone()) {
                    self.buf = self.cfg.overrides.get(&name).and_then(|o| o.ctx_size).map(|c| c.to_string()).unwrap_or_default();
                    self.input = Input::Override(name);
                }
            }
            KeyCode::Char('p') => {
                if let Some(m) = self.selected_model() {
                    let (name, format) = (m.name.clone(), m.format);
                    return self.edit_model_prompt(name, format);
                }
            }
            KeyCode::Char('P') => {
                if let Some(name) = self.selected_model().map(|m| m.name.clone()) {
                    if crate::sysprompt::model_file(&name).exists() {
                        self.input = Input::Confirm(format!("Remove {name}'s own system prompt and use the backend's? [y/N]"), Confirmed::ResetPrompt(name));
                    } else {
                        self.toast(format!("{name} already uses the backend's system prompt"));
                    }
                }
            }
            KeyCode::Char('y') => {
                if let Some(m) = self.selected_model() {
                    let name = m.name.clone();
                    let ok = std::process::Command::new("wl-copy").arg(&name).status().map(|s| s.success()).unwrap_or(false);
                    self.toast(if ok { format!("copied {name}") } else { "wl-copy not available".into() });
                }
            }
            _ => {}
        }
        None
    }

    /// Name plus compatibility marker and CPU tag.
    fn model_label(&self, m: &LocalModel, name: String) -> Line<'static> {
        let mut spans = vec![Span::raw(name)];
        if m.format == Format::Mlx {
            let v = crate::compat::assess_mlx(&self.cfg, m.repo.as_deref().unwrap_or(&m.name), m.qmode.as_deref());
            let color = match v.level {
                "unlikely" => Color::Red,
                "untested" => Color::Yellow,
                _ => Color::Green,
            };
            let mk = crate::compat::marker(v.level);
            if !mk.is_empty() {
                spans.push(Span::styled(format!(" {mk}"), Style::new().fg(color)));
            }
        }
        if self.device_of(&m.name) == "cpu" {
            spans.push(Span::styled(" cpu", Style::new().fg(Color::Yellow)));
        }
        Line::from(spans)
    }

    /// Open prompts/models/<model>.md in the editor. A model without one gets a new file seeded with
    /// the backend's prompt, so you see what applies now; left unchanged it is removed again.
    fn edit_model_prompt(&mut self, name: String, format: Format) -> Option<Action> {
        let p = crate::sysprompt::model_file(&name);
        let mut seed = None;
        if !p.exists() {
            let text = crate::sysprompt::backend_text(format);
            if let Err(e) = std::fs::create_dir_all(p.parent().unwrap()).and_then(|_| std::fs::write(&p, &text)) {
                self.toast(format!("{}: {e}", p.display()));
                return None;
            }
            seed = Some(text);
        }
        self.editing = Some((p.clone(), seed));
        Some(Action::Edit(crate::sysprompt::editor_cmd(&self.cfg, &p)))
    }

    fn device_of(&self, name: &str) -> String {
        self.cfg.overrides.get(name).and_then(|o| o.device.clone()).unwrap_or_else(|| "gpu".into())
    }

    /// Load through the daemon; a missing-GPU-kernel failure opens the CPU /
    /// recommended-download choice instead of just a toast.
    fn load_model(&self, name: String) {
        let d = self.d.clone();
        let tx = self.tx.clone();
        let cfg = self.cfg.clone();
        let local = self.models.iter().find(|m| m.name == name).cloned();
        tokio::spawn(async move {
            if !d.up().await {
                let _ = client::start_daemon();
                for _ in 0..50 {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    if d.up().await {
                        break;
                    }
                }
            }
            let _ = tx.send(Msg::Toast(format!("loading {name}…")));
            match d.load(&name).await {
                Ok(v) => {
                    let _ = tx.send(Msg::Toast(format!("{name} ready ({:.1}s)", v["load_secs"].as_f64().unwrap_or(0.0))));
                }
                Err(e) if crate::compat::is_gpu_gap(&e.to_string()) => {
                    let rec = local
                        .filter(|m| m.format == Format::Mlx)
                        .and_then(|m| crate::compat::assess_mlx(&cfg, m.repo.as_deref().unwrap_or(&m.name), m.qmode.as_deref()).recommend);
                    let _ = tx.send(Msg::GpuGap(name, rec));
                }
                Err(e) if e.to_string().contains("can be given to it right now") => {
                    let _ = tx.send(Msg::NeedMemory(name, format!("{e:#}")));
                }
                Err(e) => {
                    let _ = tx.send(Msg::Toast(format!("error: {e:#}")));
                }
            }
            let _ = tx.send(Msg::Status(d.status().await));
        });
    }

    fn set_device(&self, name: String, dev: String, then_load: bool) {
        let d = self.d.clone();
        let tx = self.tx.clone();
        let this_load = then_load.then(|| name.clone());
        tokio::spawn(async move {
            match d.set_override(&name, "device", &dev).await {
                Ok(_) => {
                    let _ = tx.send(Msg::Toast(format!("{name} will run on the {}", if dev == "cpu" { "CPU (discouraged)" } else { "Apple GPU" })));
                }
                Err(e) => {
                    let _ = tx.send(Msg::Toast(format!("error: {e:#}")));
                    return;
                }
            }
            let _ = tx.send(Msg::Models);
            if let Some(n) = this_load {
                let _ = tx.send(Msg::Toast(format!("loading {n} on the CPU…")));
                let r = d.load(&n).await;
                let _ = tx.send(Msg::Toast(match r {
                    Ok(v) => format!("{n} ready on CPU ({:.1}s)", v["load_secs"].as_f64().unwrap_or(0.0)),
                    Err(e) => format!("error: {e:#}"),
                }));
                let _ = tx.send(Msg::Status(d.status().await));
            }
        });
    }

    fn set_device_and_load(&self, name: String, dev: &str) {
        self.set_device(name, dev.into(), true);
    }

    fn search_kind(&self) -> &'static str {
        ["mlx", "gguf", "both"][self.search_kind]
    }

    fn search(&mut self) {
        if let Some(task) = self.search_task.take() { task.abort(); }
        let q = self.query.clone();
        let kind = self.search_kind();
        let budget = self.search_budget;
        let cfg = self.cfg.clone();
        let tx = self.tx.clone();
        self.searching = true;
        self.search_generation += 1;
        let generation = self.search_generation;
        let sort = ["relevance", "downloads", "likes", "lastModified"][self.search_sort];
        self.repo = None;
        self.hits.clear();
        self.hits_sel.select(Some(0));
        self.search_task = Some(tokio::spawn(async move {
            let r = hf::search_discover(&hf::client(), &cfg, &q, kind, budget, sort).await.map_err(|e| e.to_string());
            let _ = tx.send(Msg::Search(generation, r));
        }));
    }

    fn open_repo(&mut self, repo: String) {
        let tx = self.tx.clone();
        self.repo = Some(RepoView { repo: repo.clone(), files: None, err: None, sel: TableState::default().with_selected(Some(0)) });
        tokio::spawn(async move {
            let c = hf::client();
            let r = async {
                let sha = hf::repo_sha(&c, &repo, "main").await?;
                hf::repo_files(&c, &repo, &sha).await
            }
            .await
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Repo(repo, r));
        });
    }

    /// Choices shown for an opened repo: (pull spec, label, size).
    fn repo_choices(&self) -> Vec<(String, String, u64)> {
        let Some(rv) = &self.repo else { return vec![] };
        let Some(files) = &rv.files else { return vec![] };
        let choices = if self.catalog_mode {
            hf::choices(&rv.repo, files, &self.cfg.download.gguf_default_quant)
        } else {
            hf::discovery_choices(&self.cfg, &rv.repo, files, self.search_kind(), self.search_budget)
        };
        let advice: Vec<_> = choices.iter().map(|c| crate::compat::hardware_advice(&self.cfg, c.size, c.format)).collect();
        let recommended = crate::compat::recommended_choice(&choices, &advice);
        let recommended = recommended.filter(|&i| choices[i].format != "mlx" || crate::compat::assess_mlx(&self.cfg, &rv.repo, None).level != "unlikely");
        choices.into_iter().enumerate().map(|(i, c)| {
            (c.spec, if recommended == Some(i) { format!("{} · recommended", c.label) }
                else if c.default && c.format == "gguf" { format!("{} ★", c.label) } else { c.label }, c.size)
        }).collect()
    }

    fn key_discover(&mut self, k: KeyEvent) -> Option<Action> {
        if self.repo.is_some() {
            let choices = self.repo_choices();
            let n = choices.len();
            let sel = &mut self.repo.as_mut().unwrap().sel;
            match k.code {
                KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') => self.repo = None,
                KeyCode::Char('a') => {
                    let repo = self.repo.as_ref().unwrap().repo.clone();
                    match crate::compat::assess_mlx(&self.cfg, &repo, None).recommend {
                        Some(r) => self.open_repo(r),
                        None => self.toast("no recommended alternative known"),
                    }
                    return None;
                }
                KeyCode::Down | KeyCode::Char('j') => move_sel(sel, n, 1),
                KeyCode::Up | KeyCode::Char('k') => move_sel(sel, n, -1),
                KeyCode::Enter | KeyCode::Char('p') => {
                    let i = sel.selected().unwrap_or(0);
                    if let Some((spec, _, _)) = choices.get(i).cloned() {
                        self.spawn_daemon_op(format!("pulling {spec}"), move |d| async move {
                            d.pull(&spec).await?;
                            Ok(format!("downloading {spec} (progress on Dashboard)"))
                        });
                    }
                }
                _ => {}
            }
            return None;
        }
        let n = if self.catalog_mode { self.catalog.len() } else { self.hits.len() };
        match k.code {
            KeyCode::Char('/') | KeyCode::Char('s') => {
                self.buf = self.query.clone();
                self.input = Input::Search;
            }
            KeyCode::Char('g') => {
                self.search_kind = (self.search_kind + 1) % 3;
                self.catalog_mode = false;
                self.search();
            }
            KeyCode::Char('m') => {
                self.buf = self.search_budget.map(|b| format!("{}", crate::backend::gib(b))).unwrap_or_default();
                self.input = Input::SearchBudget;
            }
            KeyCode::Char('o') => {
                self.search_sort = (self.search_sort + 1) % 4;
                self.catalog_mode = false;
                self.search();
            }
            KeyCode::Char('c') => {
                self.catalog_mode = !self.catalog_mode;
                if !self.catalog_mode && self.hits.is_empty() {
                    self.search();
                }
            }
            KeyCode::Down | KeyCode::Char('j') => move_sel(&mut self.hits_sel, n, 1),
            KeyCode::Up | KeyCode::Char('k') => move_sel(&mut self.hits_sel, n, -1),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                let i = self.hits_sel.selected().unwrap_or(0);
                let repo = if self.catalog_mode {
                    self.catalog.get(i).and_then(|c| c["repo"].as_str()).map(str::to_string)
                } else {
                    self.hits.get(i).map(|h| h.id.clone())
                };
                if let Some(r) = repo {
                    self.open_repo(r);
                }
            }
            _ => {}
        }
        None
    }

    fn key_settings(&mut self, k: KeyEvent) -> Option<Action> {
        match k.code {
            KeyCode::Left | KeyCode::Right | KeyCode::Char('h') | KeyCode::Char('l') => {
                let i = self.settings_sel.selected().unwrap_or(0);
                let dir = if matches!(k.code, KeyCode::Left | KeyCode::Char('h')) { -1.0 } else { 1.0 };
                if !self.nudge_setting(SETTINGS[i].1, dir) {
                    self.toast("←/→ adjust only the memory sliders; Enter edits");
                }
            }
            KeyCode::Down | KeyCode::Char('j') => move_sel(&mut self.settings_sel, SETTINGS.len(), 1),
            KeyCode::Up | KeyCode::Char('k') => move_sel(&mut self.settings_sel, SETTINGS.len(), -1),
            KeyCode::Enter | KeyCode::Char('e') => {
                let i = self.settings_sel.selected().unwrap_or(0);
                if let Some(f) = match SETTINGS[i].1 {
                    "mlx.system_prompt" => Some(Format::Mlx),
                    "llama.system_prompt" => Some(Format::Gguf),
                    "strata.system_prompt" => Some(Format::Strata),
                    _ => None,
                } {
                    let p = crate::sysprompt::backend_file(f);
                    if !p.exists() {
                        if let Err(e) = crate::sysprompt::set_backend_text(f, "") {
                            self.toast(format!("{}: {e}", p.display()));
                            return None;
                        }
                    }
                    self.editing = Some((p.clone(), None));
                    return Some(Action::Edit(crate::sysprompt::editor_cmd(&self.cfg, &p)));
                }
                self.buf = get_setting(&self.cfg, SETTINGS[i].1);
                self.input = Input::Setting(i);
            }
            KeyCode::Char('o') => {
                let p = crate::config::config_path();
                if !p.exists() {
                    let _ = self.cfg.save();
                }
                return Some(Action::Edit(crate::sysprompt::editor_cmd(&self.cfg, &p)));
            }
            _ => {}
        }
        None
    }

    fn key_logs(&mut self, k: KeyEvent) -> Option<Action> {
        match k.code {
            KeyCode::Char('m') => {
                self.logs_model = !self.logs_model;
                self.logs_scroll = 0;
            }
            KeyCode::Up | KeyCode::Char('k') => self.logs_scroll = self.logs_scroll.saturating_add(1),
            KeyCode::Down | KeyCode::Char('j') => self.logs_scroll = self.logs_scroll.saturating_sub(1),
            KeyCode::PageUp => self.logs_scroll = self.logs_scroll.saturating_add(20),
            KeyCode::PageDown => self.logs_scroll = self.logs_scroll.saturating_sub(20),
            KeyCode::End | KeyCode::Char('G') => self.logs_scroll = 0,
            _ => {}
        }
        self.load_logs();
        None
    }

    fn logs_path(&self) -> std::path::PathBuf {
        if self.logs_model {
            let name = self
                .running_names()
                .first()
                .cloned()
                .or_else(|| self.selected_model().map(|m| m.name.clone()))
                .unwrap_or_default();
            log_dir().join(format!("{}.log", name.replace(['/', ':'], "_")))
        } else {
            log_dir().join("lmdeck.log")
        }
    }

    fn load_logs(&mut self) {
        self.logs = sys::read_tail(&self.logs_path(), 128 * 1024);
    }

    // ---------- drawing ----------

    fn draw(&mut self, f: &mut Frame) {
        self.clicks.clear();
        let [top, body, foot] = Layout::vertical([Constraint::Length(1), Constraint::Min(5), Constraint::Length(1)]).areas(f.area());
        let narrow = f.area().width < 100;
        let titles: Vec<Line> = TABS
            .iter()
            .map(|t| Line::from(if narrow { t.title().split_whitespace().next().unwrap_or("").to_string() } else { t.title().to_string() }))
            .collect();
        let idx = TABS.iter().position(|t| *t == self.tab).unwrap();
        let up = self.status.is_some();
        let [tabs_area, badge] = Layout::horizontal([Constraint::Min(10), Constraint::Length(if narrow { 9 } else { 34 })]).areas(top);
        let titles = if narrow {
            // Numbers only, plus the active tab's name.
            let mut t = titles;
            t[idx] = Line::from(self.tab.title().to_string());
            t
        } else {
            titles
        };
        let mut tab_x = tabs_area.x;
        for (i, title) in titles.iter().enumerate() {
            let width = (title.width() as u16 + 2).min(tabs_area.right().saturating_sub(tab_x));
            if width > 0 {
                self.clicks.push((Rect::new(tab_x, tabs_area.y, width, 1), Click::Key(KeyCode::Char((b'1' + i as u8) as char))));
            }
            tab_x = tab_x.saturating_add(width + 1);
        }
        f.render_widget(
            Tabs::new(titles).select(idx).highlight_style(Style::new().fg(Color::Black).bg(Color::Cyan).bold()).divider(" "),
            tabs_area,
        );
        let badge_line = if narrow {
            Line::from(vec![
                if up { Span::styled("● ", Style::new().fg(Color::Green)) } else { Span::styled("○ ", Style::new().fg(Color::DarkGray)) },
                Span::styled(format!(":{}", self.cfg.server.port), Style::new().fg(Color::DarkGray)),
            ])
            .right_aligned()
        } else {
            Line::from(vec![
                Span::raw("lmdeck "),
                if up { Span::styled("● serving ", Style::new().fg(Color::Green)) } else { Span::styled("○ stopped ", Style::new().fg(Color::DarkGray)) },
                Span::styled(format!(":{}", self.cfg.server.port), Style::new().fg(Color::DarkGray)),
            ])
            .right_aligned()
        };
        f.render_widget(badge_line, badge);
        match self.tab {
            Tab::Dashboard => self.draw_dashboard(f, body),
            Tab::Models => self.draw_models(f, body),
            Tab::Discover => self.draw_discover(f, body),
            Tab::Settings => self.draw_settings(f, body),
            Tab::Logs => self.draw_logs(f, body),
        }
        let help = match self.tab {
            Tab::Dashboard => "Click a button · ? setup guide · i choose MLX runtime · B optional GGUF setup · r refresh",
            Tab::Models => "⏎ load · u unload · g GPU⇄CPU · c chat · x context · p prompt (P: backend's) · d delete · y copy · ✓ qualified ? untested ✗ GPU-incompatible",
            Tab::Discover => {
                if self.repo.is_some() {
                    "⏎ download · a recommended alternative · x cancel download · esc back"
                } else {
                    "/ search · g MLX/GGUF/Both · m RAM budget · o sort · c catalog⇄search · ⏎ open · x cancel download"
                }
            }
            Tab::Settings => "⏎ edit (prompts open in your editor) · o open config in your editor",
            Tab::Logs => "m daemon⇄model log · j/k scroll · G end",
        };
        let foot_line = match &self.toast {
            Some((t, _)) => Line::from(Span::styled(t.clone(), Style::new().fg(Color::Yellow))),
            None => Line::from(vec![Span::styled(help, Style::new().fg(Color::DarkGray)), Span::styled("  · q closes TUI (server keeps running)", Style::new().fg(Color::DarkGray))]),
        };
        f.render_widget(foot_line, foot);
        self.draw_input(f);
    }

    fn draw_buttons(&mut self, f: &mut Frame, area: Rect, buttons: &[(&str, KeyCode)]) {
        for (index, cell) in button_cells(area, buttons) {
            let (label, key) = buttons[index];
            f.render_widget(Paragraph::new(label).centered().style(
                Style::new().fg(Color::Rgb(134, 201, 190)).bg(Color::Rgb(62, 68, 78))
            ), cell);
            self.clicks.push((cell, Click::Key(key)));
        }
    }

    fn draw_choice(&mut self, f: &mut Frame, title: &str, description: &str, buttons: &[(&str, KeyCode)]) {
        self.clicks.clear();
        let a = f.area();
        let width = a.width.saturating_sub(4).min(88);
        let text_width = width.saturating_sub(4).max(1) as usize;
        let lines: usize = description.lines().map(|l| l.chars().count().max(1).div_ceil(text_width)).sum();
        let actions_height = (buttons.len() as u16 * 2).saturating_sub(1);
        let height = (lines as u16 + actions_height + 5).min(a.height.saturating_sub(2));
        let r = Rect::new(a.x + (a.width - width) / 2, a.y + (a.height - height) / 2, width, height);
        f.render_widget(Clear, r);
        let block = panel(title);
        let inner = block.inner(r).inner(ratatui::layout::Margin { horizontal: 1, vertical: 0 });
        f.render_widget(block, r);
        let [text, _, actions] = Layout::vertical([
            Constraint::Min(0), Constraint::Length(1), Constraint::Length(actions_height),
        ]).areas(inner);
        let scroll = if matches!(self.input, Input::Guide) {
            self.popup_scroll = self.popup_scroll.min((lines as u16).saturating_sub(text.height));
            self.popup_scroll
        } else { 0 };
        f.render_widget(Paragraph::new(description).wrap(Wrap { trim: false }).scroll((scroll, 0)), text);
        for (i, button) in buttons.iter().enumerate() {
            let row = i as u16 * 2;
            if row >= actions.height { break; }
            self.draw_buttons(f, Rect::new(actions.x, actions.y + row, actions.width, 1), &[*button]);
        }
    }

    fn draw_input(&mut self, f: &mut Frame) {
        if let Input::Confirm(question, _) = &self.input {
            let question = question.clone();
            self.draw_choice(f, "Confirm", &question, &[("Confirm [y]", KeyCode::Char('y')), ("Cancel [n]", KeyCode::Char('n'))]);
            return;
        }
        if matches!(self.input, Input::Update) {
            self.draw_choice(f, "Update MLX", "Choose how to install and update the MLX backend.\n\nOmarchy: system-managed packages, full system update first.\nCustom Installer: latest GitHub release in your home folder.\nFixed: the latest release built from source with lmdeck's fixes\n(finer GPU buffer sizes: ~0.3 GiB less for a 27B model); takes a while to build.\nAll use the Apple GPU; release and model differences affect speed.\nYou can keep both runtimes or only the selected one afterwards.", &[
                ("1. Omarchy packages", KeyCode::Char('1')),
                ("2. Custom Installer", KeyCode::Char('2')),
                ("3. Fixed (build with lmdeck fixes)", KeyCode::Char('3')),
                ("q. Cancel", KeyCode::Char('q')),
            ]);
            return;
        }
        if matches!(self.input, Input::Runtime) {
            let description = format!("Choose an already installed runtime; this does not reinstall it.\n\nSelected: {}\nOmarchy package: {}\nCustom Installer: {}\nFixed (release + lmdeck fixes): {}\n\n{}\nThe selected launcher is saved explicitly in lmdeck settings. Loaded models need a reload to switch.", self.versions.mlx_selected, self.versions.mlx_pkg.as_deref().unwrap_or("not installed — use Update MLX"), self.versions.mlx_legacy.as_deref().unwrap_or("not installed — use Update MLX"), self.versions.mlx_fixed.as_deref().unwrap_or("not built — Update MLX → 3"), {
                let notes: Vec<String> = [("omarchy", "package"), ("custom", "Custom"), ("fixed", "Fixed")].iter()
                    .filter(|(s, _)| sys::mlx_runtime_path(s).exists())
                    .filter_map(|(s, label)| crate::compat::runtime_buffer_rounding(&sys::mlx_runtime_path(s).display().to_string()).describe().map(|d| format!("the {label} build {d}")))
                    .collect();
                if notes.is_empty() { String::new() } else { format!("Memory: {}.", notes.join("; ")) }
            });
            self.draw_choice(f, "Choose MLX runtime", &description, &[
                ("1. Use Omarchy package", KeyCode::Char('1')),
                ("2. Use Custom Installer", KeyCode::Char('2')),
                ("3. Use Fixed (release + lmdeck fixes)", KeyCode::Char('3')),
                ("q. Cancel", KeyCode::Char('q')),
            ]);
            return;
        }
        if let Input::RuntimeKeep(source) = &self.input {
            let description = format!("Use {source} for MLX inference.\n\nKeep the other runtime as a fallback, or remove it to save disk space.\nModels, chat history and the required GPU driver are preserved.\nAlready loaded models need reloading to use a different runtime.");
            self.draw_choice(f, "Keep one or both runtimes?", &description, &[
                ("1. Use selected; keep existing runtimes", KeyCode::Char('1')),
                ("2. Use selected; remove other runtime", KeyCode::Char('2')),
                ("q. Cancel", KeyCode::Char('q')),
            ]);
            return;
        }
        if let Input::Free(groups, sel, ctx) = &self.input {
            let (total, avail) = sys::mem();
            let a = f.area();
            let w = a.width.saturating_sub(4).min(90);
            let h = a.height.saturating_sub(2).min(groups.len() as u16 + 6).max(8);
            let r = Rect::new(a.x + (a.width - w) / 2, a.y + (a.height - h) / 2, w, h);
            self.clicks.clear();
            f.render_widget(Clear, r);
            let block = panel(&match ctx { Some((m, _)) => format!("Free memory for {m} — {} free of {}", fmt_gib(avail), fmt_gib(total)), None => format!("Processes — {} free of {}", fmt_gib(avail), fmt_gib(total)) });
            let inner = block.inner(r);
            f.render_widget(block, r);
            let rows = inner.height.saturating_sub(7) as usize;
            let start = sel.saturating_sub(rows.saturating_sub(1));
            let lines: Vec<Line> = groups.iter().enumerate().skip(start).take(rows).map(|(i, g)| {
                let name: String = g.name.chars().take(46).collect();
                let amount = if g.tweak.is_some() && g.rss == 0 { String::new() } else { format!("{:>3}x {:>8}", g.pids.len(), fmt_gib(g.rss)) };
                let note = if g.protected.is_some() { "  (protected)" } else { "" };
                let text = format!("{} {:<46} {}{}", if i == *sel { "▶" } else { " " }, name, amount, note);
                let style = if g.protected.is_some() {
                    Style::new().fg(Color::DarkGray)
                } else if g.tweak.is_some() {
                    Style::new().fg(Color::Cyan)
                } else if g.rss >= 1 << 30 {
                    Style::new().fg(Color::Yellow)
                } else {
                    Style::new()
                };
                Line::styled(text, if i == *sel { style.add_modifier(Modifier::REVERSED) } else { style })
            }).collect();
            let headline = ctx.as_ref().and_then(|(_, why)| {
                let num = |key: &str| why.split(key).nth(1).and_then(|r| r.split_whitespace().next()).and_then(|v| v.parse::<f64>().ok());
                let (need, cap) = (num("needs ~")?, num("but only ")?);
                let gap = (need * 1.0834 + 0.5 - cap).max(0.0);
                Some(format!("Needs ~{need:.1} GiB, only {cap:.1} GiB available to it: free about {gap:.1} GiB more, then press l."))
            });
            let [head, sliders, list, help] = Layout::vertical([Constraint::Length(headline.is_some() as u16), Constraint::Length(3), Constraint::Min(0), Constraint::Length(3)]).areas(inner);
            let bar = |v: f64| { let n = (v / 8.0 * 24.0).round() as usize; format!("{}{}", "█".repeat(n.min(24)), "░".repeat(24 - n.min(24))) };
            let (dv, vv) = (self.cfg.server.mem_reserve_gib, self.cfg.server.vm_reserve_gib);
            f.render_widget(Paragraph::new(format!(
                "Desktop reserve  {} {dv:.1} GiB   [ less  ] more\nVM reserve       {} {vv:.1} GiB   {{ less  }} more   (only counted while the VM runs)\nTotal kept free  {:.1} GiB · GPU heap {} (- +)",
                bar(dv), bar(vv), dv + vv, self.cfg.server.gpu_heap
            )).style(Style::new().fg(Color::Green)), sliders);
            if let Some(h) = headline { f.render_widget(Paragraph::new(h).style(Style::new().fg(Color::Yellow)), head); }
            let hint = ctx.as_ref().filter(|_| groups.get(*sel).is_none_or(|g| g.tweak.is_none() && g.protected.is_none())).map(|(_, why)| why.clone()).or_else(|| groups.get(*sel).and_then(|g| g.tweak.map(|(_, d)| d.to_string()).or(g.protected.map(|p| format!("protected: {p}")))));
            let _unused = groups.get(*sel).and_then(|g| g.tweak.map(|(_, d)| d.to_string()).or(g.protected.map(|p| format!("protected: {p}")))).unwrap_or_default();
            f.render_widget(Paragraph::new(lines), list);
            f.render_widget(Paragraph::new(format!("{}\n↑↓ select · Enter/x run or quit (SIGTERM) · X force kill · [ ] desktop / {{ }} VM reserve · - + GPU heap · p preset · r refresh · l retry · esc close\nRows group same-named processes; RSS counts shared pages.",hint.unwrap_or_default())).wrap(Wrap { trim: true }), help);
            return;
        }
        if let Input::Downloads(specs, sel) = &self.input {
            let area = f.area();
            let width = area.width.saturating_sub(4).min(100);
            let height = (specs.len() as u16 + 5).min(area.height.saturating_sub(2));
            let rect = Rect::new(area.x + (area.width - width) / 2, area.y + (area.height - height) / 2, width, height);
            self.clicks.clear();
            f.render_widget(Clear, rect);
            let block = panel("Cancel download — partial files kept for resume");
            let inner = block.inner(rect);
            f.render_widget(block, rect);
            let [list, help] = Layout::vertical([Constraint::Min(1), Constraint::Length(2)]).areas(inner);
            let count = list.height as usize;
            let start = sel.saturating_sub(count.saturating_sub(1));
            let lines: Vec<Line> = specs.iter().enumerate().skip(start).take(count).map(|(i, s)| {
                Line::styled(format!("{} {s}", if i == *sel { "▶" } else { " " }), if i == *sel { Style::new().fg(Color::Cyan).bold() } else { Style::new() })
            }).collect();
            f.render_widget(Paragraph::new(lines), list);
            f.render_widget(Paragraph::new("↑↓ select · Enter/x cancel · Esc close\nDownload the same model again to resume.").fg(Color::DarkGray), help);
            return;
        }
        if matches!(self.input, Input::Guide) {
            self.draw_choice(f, "Setup guide", "MLX / safetensors\nRuns compatible MLX models on your Apple GPU. Omarchy and Custom\nInstaller are two installation sources for this same backend.\n\nGGUF / llama.cpp — optional\nRuns GGUF models with Vulkan GPU offload. Set it up only if you want\nGGUF models; MLX inference does not need a llama.cpp build.\n\nPerformance\nSpeed depends on the model, quantization, context length and GPU\noffload. CPU execution can be much slower. Compare the same workload\nusing generation tokens/s, prompt tokens/s and first-token latency\nin Usage. A newer release is not a guarantee of faster inference.", &[
                ("Update MLX [U]", KeyCode::Char('U')),
                ("Set up optional GGUF [B]", KeyCode::Char('B')),
                ("Close [q]", KeyCode::Char('q')),
            ]);
            return;
        }
        let (title, text) = match &self.input {
            Input::Update | Input::Runtime | Input::RuntimeKeep(_) | Input::Guide | Input::Free(..) => unreachable!(),
            Input::None => return,
            Input::Downloads(..) => unreachable!(),
            Input::Search => (format!("Search Hugging Face ({})", self.search_kind().to_uppercase()), format!("{}▏", self.buf)),
            Input::SearchBudget => ("Maximum estimated RAM in GiB (empty = unlimited)".into(), format!("{}▏", self.buf)),
            Input::Setting(i) => (format!("{} — {}", SETTINGS[*i].1, SETTINGS[*i].2), format!("{}▏", self.buf)),
            Input::Override(n) => (format!("Context size for {n} (empty = default)"), format!("{}▏", self.buf)),
            Input::Confirm(q, _) => ("Confirm".into(), q.clone()),
            Input::GpuGap(m, rec) => (
                "No Apple GPU kernel for this model".into(),
                format!(
                    "{m} uses an op omarchy-mlx can't run on the GPU yet.  c = run on CPU (discouraged: much slower){}  ·  esc = cancel",
                    rec.as_ref().map(|r| format!("  ·  d = download {r} instead")).unwrap_or_default()
                ),
            ),
        };
        let a = f.area();
        let w = a.width.saturating_sub(8).min(100);
        let lines = (text.chars().count() as u16 / w.saturating_sub(2).max(1)) + 1;
        let r = Rect { x: a.x + (a.width - w) / 2, y: a.y + a.height / 3, width: w, height: 2 + lines.min(6) };
        self.clicks.clear();
        f.render_widget(Clear, r);
        f.render_widget(
            Paragraph::new(text).wrap(Wrap { trim: true }).block(Block::bordered().border_type(BorderType::Rounded).title(title).border_style(Style::new().fg(Color::Cyan))),
            r,
        );
    }

    fn draw_dashboard(&mut self, f: &mut Frame, area: Rect) {
        let server = if self.status.is_some() { "Stop server [s]" } else { "Start server [s]" };
        let runtime = format!("MLX: {} [i]", match self.cfg.mlx.command.first() {
            Some(c) if c.starts_with("/usr/") => "package",
            Some(c) if c.ends_with("-fixed") => "Fixed",
            _ => "Custom",
        });
        let primary = [(server, KeyCode::Char('s')), ("Update MLX [U]", KeyCode::Char('U')), (runtime.as_str(), KeyCode::Char('i')), ("Setup guide [?]", KeyCode::Char('?'))];
        let secondary = [("Models [2]", KeyCode::Char('2')), ("Free memory [f]", KeyCode::Char('f')), ("GGUF setup [B]", KeyCode::Char('B')), ("Strata [T]", KeyCode::Char('T')), ("Reload models [R]", KeyCode::Char('R')), ("Unload all [a]", KeyCode::Char('a')), ("Cancel download [x]", KeyCode::Char('x'))];
        let width = area.width.saturating_sub(4);
        let primary_height = button_height(width, &primary);
        let secondary_height = button_height(width, &secondary);
        let description_height = if width < 60 { 2 } else { 1 };
        let [controls, area] = Layout::vertical([
            Constraint::Length(primary_height + secondary_height + description_height + 3), Constraint::Min(0),
        ]).areas(area);
        let block = panel("Actions");
        let inner = block.inner(controls).inner(ratatui::layout::Margin { horizontal: 1, vertical: 0 });
        f.render_widget(block, controls);
        let [primary_area, _, secondary_area, explanation] = Layout::vertical([
            Constraint::Length(primary_height), Constraint::Length(1),
            Constraint::Length(secondary_height), Constraint::Length(description_height),
        ]).areas(inner);
        self.draw_buttons(f, primary_area, &primary);
        self.draw_buttons(f, secondary_area, &secondary);
        f.render_widget(Paragraph::new("MLX: Apple GPU models · GGUF: optional llama.cpp").fg(Color::DarkGray).wrap(Wrap { trim: true }), explanation);
        let narrow = area.width < 100;
        let (srv, loaded, usage_area, dls, right) = if narrow {
            let usage_height = if area.height >= 25 { 5 } else { 0 };
            let downloads_height = if area.height >= 32 { 5 } else { 0 };
            let [right, srv, loaded, usage, downloads] = Layout::vertical([
                Constraint::Length(if area.height >= 25 { 13 } else { 7 }),
                Constraint::Length(7), Constraint::Min(3),
                Constraint::Length(usage_height), Constraint::Length(downloads_height),
            ]).areas(area);
            (srv, loaded, usage, downloads, right)
        } else {
            let [left, right] = Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(area);
            let [srv, loaded, usage, downloads] = Layout::vertical([
                Constraint::Length(9), Constraint::Min(4), Constraint::Length(6),
                Constraint::Length(if area.height >= 24 { 5 } else { 0 }),
            ]).areas(left);
            (srv, loaded, usage, downloads, right)
        };

        // Server
        let mut lines = vec![];
        lines.push(Line::from(vec![
            Span::styled(format!("{:<12}", "Server"), Style::new().fg(Color::DarkGray)),
            if self.status.is_some() {
                Span::styled(" ON ", Style::new().fg(Color::Black).bg(Color::Green).bold())
            } else {
                Span::styled(" OFF ", Style::new().fg(Color::Black).bg(Color::DarkGray).bold())
            },
            Span::styled(
                if self.status.is_some() { "  s = turn off · X = force stop" } else { "  s = turn on" },
                Style::new().fg(Color::DarkGray),
            ),
        ]));
        match &self.status {
            Some(s) => {
                lines.push(Line::from(vec![
                    Span::styled("● running  ", Style::new().fg(Color::Green).bold()),
                    Span::raw(format!(
                        "pid {}  ·  {} RSS  ·  up {}",
                        s["pid"],
                        human(s["rss"].as_u64().unwrap_or(0)),
                        dur(s["uptime_secs"].as_u64().unwrap_or(0))
                    )),
                ]));
            }
            None => lines.push(Line::from(Span::styled("○ stopped", Style::new().fg(Color::DarkGray)))),
        }
        let base = self.cfg.base_url();
        lines.push(kv("Ollama API", &base));
        lines.push(kv("OpenAI API", &format!("{base}/v1")));
        lines.push(kv(
            "Service",
            if sys::service_installed() { if sys::service_active() { "installed · active" } else { "installed · inactive" } } else { "not installed (S)" },
        ));
        lines.push(kv("Policy", &format!("keep {} · max {} loaded · uclamp {}", self.cfg.server.keep_alive, self.cfg.server.max_loaded, if self.cfg.server.uclamp_boost { "on" } else { "off" })));
        let (total, avail) = sys::mem();
        lines.push(kv("Memory", &format!("{} free of {}", human(avail), human(total))));
        if let Some(st) = &self.status {
            let exposed = st["host"].as_str() == Some("0.0.0.0");
            let ip = st["ips"]
                .as_array()
                .and_then(|a| a.iter().find(|i| !i["iface"].as_str().unwrap_or("").starts_with("docker")).or(a.first()))
                .and_then(|i| i["ip"].as_str())
                .unwrap_or("-")
                .to_string();
            lines.push(Line::from(vec![
                Span::styled(format!("{:<12}", "LAN"), Style::new().fg(Color::DarkGray)),
                if exposed {
                    Span::styled(format!("http://{ip}:{} (exposed)", self.cfg.server.port), Style::new().fg(Color::Yellow))
                } else {
                    Span::raw(format!("{ip} · local only (server.host)"))
                },
            ]));
        }
        f.render_widget(Paragraph::new(lines).block(panel("Server")), srv);

        // Loaded models
        let rows: Vec<Row> = self
            .status
            .as_ref()
            .and_then(|s| s["running"].as_array().cloned())
            .unwrap_or_default()
            .iter()
            .map(|r| {
                Row::new(vec![
                    Cell::from(r["name"].as_str().unwrap_or("").to_string()),
                    Cell::from(r["format"].as_str().unwrap_or("").to_string()),
                    Cell::from(human(r["rss"].as_u64().unwrap_or(0))),
                    Cell::from(if r["in_flight"].as_u64().unwrap_or(0) > 0 {
                        "busy".to_string()
                    } else {
                        r["expires_in_secs"].as_u64().map(|s| format!("{}", dur(s))).unwrap_or("∞".into())
                    }),
                    Cell::from(if r["note"].as_str().map(|n| !n.is_empty()).unwrap_or(false) && r["stale"].as_bool() != Some(true) {
                        Span::styled("⚠", Style::new().fg(Color::Yellow))
                    } else if r["stale"].as_bool() == Some(true) {
                        Span::styled("⟳ R", Style::new().fg(Color::Yellow))
                    } else {
                        Span::raw(format!("{:.1}s", r["load_secs"].as_f64().unwrap_or(0.0)))
                    }),
                ])
            })
            .collect();
        let empty = rows.is_empty();
        let block = panel("Loaded");
        let inner_loaded = block.inner(loaded);
        f.render_widget(block, loaded);
        let progress = self.status.as_ref().and_then(|s| s.get("load")).filter(|l| !l.is_null()).cloned();
        let (prog_area, table_area) = if progress.is_some() {
            let [a, b] = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).areas(inner_loaded);
            (Some(a), b)
        } else {
            (None, inner_loaded)
        };
        if let (Some(area), Some(l)) = (prog_area, progress.as_ref()) {
            let [label_area, gauge_area] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
            let label = load_label(l);
            f.render_widget(Line::from(Span::styled(label, Style::new().fg(Color::Cyan))), label_area);
            f.render_widget(
                Gauge::default()
                    .ratio(l["progress"].as_f64().unwrap_or(0.0).clamp(0.0, 1.0))
                    .gauge_style(Style::new().fg(Color::Cyan).bg(Color::Reset))
                    .use_unicode(true)
                    .label(format!("{:.0}%", l["progress"].as_f64().unwrap_or(0.0) * 100.0)),
                gauge_area,
            );
        }
        let t = Table::new(rows, [Constraint::Min(20), Constraint::Length(5), Constraint::Length(7), Constraint::Length(8), Constraint::Length(7)])
            .header(Row::new(["Model", "Fmt", "RSS", "Unload", "Load"]).style(Style::new().fg(Color::DarkGray)));
        f.render_widget(t, table_area);
        let empty = empty && progress.is_none();
        if empty {
            let inner = loaded.inner(ratatui::layout::Margin { horizontal: 2, vertical: 2 });
            f.render_widget(Paragraph::new("nothing loaded — models load on first request").fg(Color::DarkGray), inner);
        }

        self.draw_usage(f, usage_area);
        self.draw_downloads(f, dls);

        // Show the selected backend first; setup details live in the guide.
        let v = &self.versions;
        let selected_version = match v.mlx_selected.as_str() {
            "Custom Installer" => v.mlx_legacy.as_deref(),
            "Omarchy package" => v.mlx_pkg.as_deref(),
            _ => None,
        };
        let mut rl = vec![
            Line::from(Span::styled("MLX · Apple GPU", Style::new().fg(Color::Magenta).bold())),
            kv("Selected", &v.mlx_selected),
            kv("Version", selected_version.unwrap_or("not installed — Update MLX")),
        ];
        if let Some(p) = &v.mlx_pkg { rl.push(kv("Package", p)); }
        if v.mlx_selected != "Custom Installer" {
            if let Some(c) = &v.mlx_legacy { rl.push(kv("Custom", c)); }
        }
        if let Some(u) = &v.mlx_upstream { rl.push(kv("Latest", &format!("{u} (GitHub)"))); }
        rl.push(Line::raw(""));
        rl.push(Line::from(Span::styled("GGUF · llama.cpp · optional", Style::new().fg(Color::Blue).bold())));
        rl.push(Line::raw(if v.llama_path.is_some() { "Ready for GGUF models (Vulkan GPU offload)." } else { "Not set up. Only needed to run GGUF models." }));
        rl.push(Line::from(Span::styled("MLX models do not need a llama.cpp build.", Style::new().fg(Color::DarkGray))));
        rl.push(Line::raw(""));
        let n_mlx = self.models.iter().filter(|m| m.format == Format::Mlx).count();
        let disk: u64 = self.models.iter().map(|m| m.size).sum();
        rl.push(kv("Library", &format!("{n_mlx} MLX · {} GGUF · {}", self.models.len() - n_mlx, human(disk))));
        rl.push(Line::from(Span::styled("Compare speed and latency in Usage.", Style::new().fg(Color::DarkGray))));
        f.render_widget(Paragraph::new(rl).wrap(Wrap { trim: false }).block(panel("Backends · setup guide [?]")), right);
    }

    fn draw_usage(&self, f: &mut Frame, area: Rect) {
        let u = self.status.as_ref().map(|s| s["usage"].clone()).unwrap_or(Value::Null);
        let mut l = vec![];
        let today = &u["today"];
        l.push(kv(
            "Today",
            &format!(
                "{} requests · {} in · {} out",
                today["requests"].as_u64().unwrap_or(0),
                compact(today["prompt"].as_u64().unwrap_or(0)),
                compact(today["completion"].as_u64().unwrap_or(0))
            ),
        ));
        match u["last"].as_object() {
            Some(last) => l.push(kv(
                "Last",
                &format!(
                    "{:.1} tok/s gen · {:.0} tok/s prefill · ttft {:.2}s",
                    last.get("gen_tps").and_then(|v| v.as_f64()).unwrap_or(0.0),
                    last.get("prompt_tps").and_then(|v| v.as_f64()).unwrap_or(0.0),
                    last.get("ttft_secs").and_then(|v| v.as_f64()).unwrap_or(0.0)
                ),
            )),
            None => l.push(kv("Last", "no requests yet")),
        }
        // 7-day sparkline of tokens
        if let Some(week) = u["week"].as_array() {
            let vals: Vec<u64> = week.iter().map(|d| d["tokens"].as_u64().unwrap_or(0)).collect();
            let peak = vals.iter().copied().max().unwrap_or(0).max(1);
            let bars = "▁▂▃▄▅▆▇█";
            let spark: String = vals.iter().map(|v| bars.chars().nth(((*v as f64 / peak as f64) * 7.0).round() as usize).unwrap_or('▁')).collect();
            l.push(kv("7 days", &format!("{spark}  peak {}", compact(peak))));
        }
        for m in u["models"].as_array().cloned().unwrap_or_default().iter().take(area.height.saturating_sub(5) as usize) {
            let name = m["model"].as_str().unwrap_or("");
            let tokens = m["prompt"].as_u64().unwrap_or(0) + m["completion"].as_u64().unwrap_or(0);
            l.push(Line::from(vec![
                Span::styled(format!("  {:>7} ", compact(tokens)), Style::new().fg(Color::DarkGray)),
                Span::raw(name.to_string()),
            ]));
        }
        f.render_widget(Paragraph::new(l).block(panel("Usage")), area);
    }

    fn draw_downloads(&self, f: &mut Frame, area: Rect) {
        let dl = all_downloads(&self.status);
        let block = panel("Downloads");
        let inner = block.inner(area);
        f.render_widget(block, area);
        if dl.is_empty() {
            f.render_widget(Paragraph::new("none — find models in Discover (3)").fg(Color::DarkGray), inner);
            return;
        }
        let rows = Layout::vertical(vec![Constraint::Length(1); dl.len().min(inner.height as usize)]).split(inner);
        for (i, d) in dl.iter().take(rows.len()).enumerate() {
            let pct = if d.total > 0 { d.completed as f64 / d.total as f64 } else { 0.0 };
            let rate = d.completed as f64 / d.elapsed.max(0.1);
            let label = match d.state.as_str() {
                "done" => format!("✓ {}  {}", d.name, human(d.total)),
                "failed" => format!("✗ {}  {}", d.name, d.detail),
                "cancelled" => format!("⊘ {}", d.name),
                "resolving" => format!("… {}", d.name),
                st => format!("{} {}  {}/{}  {}/s{}", if st == "verifying" { "⧗" } else { "↓" }, d.name, human(d.completed), human(d.total), human(rate as u64), if st == "verifying" { " verifying" } else { "" }),
            };
            let color = match d.state.as_str() {
                "done" => Color::Green,
                "failed" => Color::Red,
                _ => Color::Cyan,
            };
            f.render_widget(Gauge::default().ratio(pct.clamp(0.0, 1.0)).label(label).gauge_style(Style::new().fg(color).bg(Color::Reset)).use_unicode(true), rows[i]);
        }
    }

    fn draw_models(&mut self, f: &mut Frame, area: Rect) {
        let buttons = [("Load [Enter]", KeyCode::Enter), ("Make room [m]", KeyCode::Char('m')), ("Unload [u]", KeyCode::Char('u')), ("Chat [c]", KeyCode::Char('c')), ("Context [x]", KeyCode::Char('x')), ("Prompt [p]", KeyCode::Char('p')), ("Delete [d]", KeyCode::Char('d'))];
        let height = button_height(area.width.saturating_sub(2), &buttons);
        let [actions, area] = Layout::vertical([Constraint::Length(height + 1), Constraint::Min(0)]).areas(area);
        self.draw_buttons(f, actions.inner(ratatui::layout::Margin { horizontal: 1, vertical: 0 }), &buttons);
        let narrow = area.width < 100;
        let running = self.running_names();
        let rows: Vec<Row> = self
            .models
            .iter()
            .map(|m| {
                let loaded = running.contains(&m.name);
                let fits = crate::compat::hardware_advice(&self.cfg, m.size, if m.format == Format::Mlx { "mlx" } else { "gguf" }).fit != "too big";
                let caps: Vec<&str> = m.capabilities().into_iter().filter(|c| *c != "completion").collect();
                let short = m.name.rsplit('/').next().unwrap_or(&m.name).to_string();
                if narrow {
                    return Row::new(vec![
                        Cell::from(if loaded { "●" } else { " " }).fg(Color::Green),
                        Cell::from(self.model_label(m, short)),
                        Cell::from(m.format.as_str()).fg(if m.format == Format::Mlx { Color::Magenta } else { Color::Blue }),
                        Cell::from(human(m.size)).fg(if fits { Color::Reset } else { Color::Red }),
                    ]);
                }
                Row::new(vec![
                    Cell::from(if loaded { "●" } else { " " }).fg(Color::Green),
                    Cell::from(self.model_label(m, m.name.clone())),
                    Cell::from(m.format.as_str()).fg(if m.format == Format::Mlx { Color::Magenta } else { Color::Blue }),
                    Cell::from(m.quant.clone()),
                    Cell::from(m.params.clone()),
                    Cell::from(human(m.size)).fg(if fits { Color::Reset } else { Color::Red }),
                    Cell::from(
                        self.cfg
                            .overrides
                            .get(&m.name)
                            .and_then(|o| o.ctx_size)
                            .map(|c| format!("{}k*", c / 1024))
                            .or(m.ctx_len.map(|c| format!("{}k", c / 1024)))
                            .unwrap_or_default(),
                    ),
                    Cell::from(caps.join(",")).fg(Color::DarkGray),
                    match crate::sysprompt::source(&m.name, m.format) {
                        crate::sysprompt::Source::Model => Cell::from("model").fg(Color::Cyan),
                        crate::sysprompt::Source::Off => Cell::from("off").fg(Color::DarkGray),
                        crate::sysprompt::Source::Backend => Cell::from(if m.format == Format::Mlx { "mlx" } else { "llama" }),
                        crate::sysprompt::Source::None => Cell::from(""),
                    },
                ])
            })
            .collect();
        let (widths, header): (Vec<Constraint>, Vec<&str>) = if narrow {
            (vec![Constraint::Length(1), Constraint::Min(12), Constraint::Length(5), Constraint::Length(6)], vec!["", "Name", "Fmt", "Size"])
        } else {
            (
                vec![Constraint::Length(1), Constraint::Min(30), Constraint::Length(5), Constraint::Length(11), Constraint::Length(8), Constraint::Length(7), Constraint::Length(6), Constraint::Length(22), Constraint::Length(6)],
                vec!["", "Name", "Fmt", "Quant", "Params", "Size", "Ctx", "Caps", "Prompt"],
            )
        };
        let t = Table::new(rows, widths)
        .header(Row::new(header).style(Style::new().fg(Color::DarkGray)))
        .row_highlight_style(Style::new().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
        .block(panel(&if narrow { format!("Local models ({})", self.models.len()) } else { format!("Local models ({})  ·  {}", self.models.len(), self.cfg.hf_cache().display()) }));
        f.render_stateful_widget(t, area, &mut self.models_sel);
        for row in 0..area.height.saturating_sub(3) {
            let index = self.models_sel.offset() + row as usize;
            if index >= self.models.len() { break; }
            self.clicks.push((Rect::new(area.x + 1, area.y + 2 + row, area.width.saturating_sub(2), 1), Click::Model(index)));
        }
        if self.models.is_empty() {
            let inner = area.inner(ratatui::layout::Margin { horizontal: 2, vertical: 2 });
            f.render_widget(Paragraph::new("No models yet. Go to Discover (3) to download MLX or GGUF models.").fg(Color::DarkGray), inner);
        }
    }

    fn draw_discover(&mut self, f: &mut Frame, area: Rect) {
        let buttons = [("Search [/]", KeyCode::Char('/')), ("MLX / GGUF / Both [g]", KeyCode::Char('g')), ("RAM budget [m]", KeyCode::Char('m')), ("Catalog / HF [c]", KeyCode::Char('c')), ("Open [Enter]", KeyCode::Enter), ("Sort [o]", KeyCode::Char('o')), ("Cancel download [x]", KeyCode::Char('x'))];
        let height = button_height(area.width.saturating_sub(2), &buttons);
        let [actions, area] = Layout::vertical([Constraint::Length(height + 1), Constraint::Min(0)]).areas(area);
        self.draw_buttons(f, actions.inner(ratatui::layout::Margin { horizontal: 1, vertical: 0 }), &buttons);
        let [main, dl] = Layout::vertical([Constraint::Min(6), Constraint::Length(5)]).areas(area);
        let [list, detail] = Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)]).areas(main);

        if self.catalog_mode {
            let rows: Vec<Row> = self
                .catalog
                .iter()
                .map(|c| {
                    let q = &c["qualification"];
                    let st = |k: &str| q[k]["status"].as_str().unwrap_or("-").to_string();
                    let qual = if st("generation") == "qualified" && st("http") == "qualified" { "✓ qualified" } else { "untested" };
                    let size = c["memory"]["weights_bytes"].as_u64().unwrap_or(0);
                    Row::new(vec![
                        Cell::from(c["id"].as_str().unwrap_or("").to_string()),
                        Cell::from(c["kind"].as_str().unwrap_or("").to_string()),
                        Cell::from(human(size)).fg(if crate::compat::hardware_advice(&self.cfg, size, "mlx").fit == "fits" { Color::Reset } else { Color::Yellow }),
                        Cell::from(qual).fg(if qual.starts_with('✓') { Color::Green } else { Color::DarkGray }),
                    ])
                })
                .collect();
            let title = if self.catalog.is_empty() { "omarchy-mlx catalog (not found — install the package)".into() } else { format!("omarchy-mlx curated catalog ({})", self.catalog.len()) };
            let t = Table::new(rows, [Constraint::Min(24), Constraint::Length(9), Constraint::Length(7), Constraint::Length(12)])
                .header(Row::new(["Id", "Kind", "Size", "On Apple GPU"]).style(Style::new().fg(Color::DarkGray)))
                .row_highlight_style(Style::new().bg(Color::DarkGray).bold())
                .block(panel(&title));
            f.render_stateful_widget(t, list, &mut self.hits_sel);
        } else {
            let rows: Vec<Row> = self
                .hits
                .iter()
                .map(|h| {
                    let v = h.formats.iter().any(|format| format == "mlx").then(|| crate::compat::assess_mlx(&self.cfg, &h.id, None));
                    let (mk, color) = match v.as_ref().map(|v| v.level) {
                        Some("unlikely") => ("✗ ", Color::Red),
                        Some("untested") => ("? ", Color::Yellow),
                        Some("qualified") => ("✓ ", Color::Green),
                        _ => ("  ", Color::Reset),
                    };
                    Row::new(vec![
                        Cell::from(Line::from(vec![Span::styled(mk, Style::new().fg(color)), Span::raw(h.id.clone())])),
                        Cell::from(h.formats.join("/").to_uppercase()),
                        Cell::from(h.estimated_bytes.map(fmt_gib).unwrap_or_else(|| "—".into())),
                        Cell::from(compact(h.downloads)),
                        Cell::from(compact(h.likes)),
                        Cell::from(h.last_modified.get(..10).unwrap_or("").to_string()).fg(Color::DarkGray),
                    ])
                })
                .collect();
            let title = format!(
                "Hugging Face · {} · \"{}\"{}",
                self.search_kind().to_uppercase(),
                self.query,
                format!(" · RAM {} · {}{}", self.search_budget.map(|b| format!("≤{}", fmt_gib(b))).unwrap_or_else(|| "unlimited".into()), ["relevance", "downloads", "likes", "updated"][self.search_sort], if self.searching { " · checking variants…" } else { "" })
            );
            let t = Table::new(rows, [Constraint::Min(20), Constraint::Length(8), Constraint::Length(10), Constraint::Length(7), Constraint::Length(6), Constraint::Length(10)])
                .header(Row::new(["Repo", "Format", "Est. RAM", "⬇", "♥", "Updated"]).style(Style::new().fg(Color::DarkGray)))
                .row_highlight_style(Style::new().bg(Color::DarkGray).bold())
                .block(panel(&title));
            f.render_stateful_widget(t, list, &mut self.hits_sel);
        }

        // Detail panel
        let count = if self.catalog_mode { self.catalog.len() } else { self.hits.len() };
        for row in 0..list.height.saturating_sub(3) {
            let index = self.hits_sel.offset() + row as usize;
            if index >= count { break; }
            self.clicks.push((Rect::new(list.x + 1, list.y + 2 + row, list.width.saturating_sub(2), 1), Click::Discover(index)));
        }
        let choices = self.repo_choices();
        let verdict = self
            .repo
            .as_ref()
            .filter(|_| choices.iter().any(|c| !c.0.contains(':')))
            .map(|rv| crate::compat::assess_mlx(&self.cfg, &rv.repo, None))
            .filter(|v| v.level != "ok");
        match &mut self.repo {
            None => {
                let mut l = vec![
                    Line::from("/ search · g MLX/GGUF/Both · m RAM budget · ⏎ open"),
                    Line::from("RAM is estimated for the configured context. Unreadable repos are excluded. Search checks up to 100 repos per format."),
                    Line::raw(""),
                    Line::from(vec![Span::styled("MLX ", Style::new().fg(Color::Magenta).bold()), Span::raw("runs on the Apple GPU via omarchy-mlx (mlx_lm.server).")]),
                    Line::from(vec![Span::styled("GGUF ", Style::new().fg(Color::Blue).bold()), Span::raw("runs via llama.cpp Vulkan on Honeykrisp.")]),
                    Line::raw(""),
                    Line::from("The catalog lists models the omarchy-mlx project has qualified (or plans to) on this backend. Other MLX repos may hit unimplemented ops."),
                ];
                if self.catalog_mode {
                    if let Some(c) = self.hits_sel.selected().and_then(|i| self.catalog.get(i)) {
                        l.push(Line::raw(""));
                        l.push(kv("repo", c["repo"].as_str().unwrap_or("")));
                        if let Some(s) = c["extension"]["scope"].as_str() {
                            l.push(Line::from(s.to_string()).fg(Color::DarkGray));
                        }
                    }
                }
                f.render_widget(Paragraph::new(l).wrap(Wrap { trim: true }).block(panel("Details")), detail);
            }
            Some(rv) => {
                if let Some(e) = &rv.err {
                    f.render_widget(Paragraph::new(e.clone()).fg(Color::Red).wrap(Wrap { trim: true }).block(panel(&rv.repo)), detail);
                } else if rv.files.is_none() {
                    f.render_widget(Paragraph::new("loading file list…").fg(Color::DarkGray).block(panel(&rv.repo)), detail);
                } else {
                    let rows: Vec<Row> = choices
                        .iter()
                        .map(|(_, label, size)| {
                            let hw = crate::compat::hardware_advice(&self.cfg, *size, if label.starts_with("GGUF") { "gguf" } else { "mlx" });
                            let fit = hw.fit;
                            let color = match fit {
                                "fits" => Color::Green,
                                "tight" => Color::Yellow,
                                _ => Color::Red,
                            };
                            Row::new(vec![Cell::from(label.clone()), Cell::from(format!("{} / ~{}", human(*size), human(hw.estimated_bytes))), Cell::from(fit).fg(color)])
                        })
                        .collect();
                    let block = panel(&rv.repo);
                    let inner = block.inner(detail);
                    f.render_widget(block, detail);
                    let vh = if verdict.is_some() { 4 } else { 0 };
                    let [vrect, hrect, trect] = Layout::vertical([Constraint::Length(vh), Constraint::Length(3), Constraint::Min(2)]).areas(inner);
                    if let Some((spec, _, size)) = rv.sel.selected().and_then(|i| choices.get(i)) {
                        let hw = crate::compat::hardware_advice(&self.cfg, *size, if spec.contains(':') { "gguf" } else { "mlx" });
                        f.render_widget(Paragraph::new(format!("Est. RAM {} · free {} · GPU budget {} · {} context tokens. {}", human(hw.estimated_bytes), human(hw.available_bytes), human(hw.gpu_budget_bytes), hw.context_tokens, hw.advice)).fg(Color::DarkGray).wrap(Wrap { trim: true }), hrect);
                    }
                    if let Some(v) = &verdict {
                        let color = match v.level {
                            "unlikely" => Color::Red,
                            "untested" => Color::Yellow,
                            _ => Color::Green,
                        };
                        let mut l = vec![Line::from(Span::styled(
                            format!("{} {}", crate::compat::marker(v.level), match v.level { "unlikely" => "likely won't run on the Apple GPU", "untested" => "untested on omarchy-mlx", _ => "qualified on omarchy-mlx" }),
                            Style::new().fg(color).bold(),
                        ))];
                        if !v.reason.is_empty() && v.level != "qualified" {
                            l.push(Line::from(v.reason.clone()).fg(Color::DarkGray));
                        }
                        if let Some(r) = &v.recommend {
                            l.push(Line::from(vec![Span::raw("recommended: "), Span::styled(r.clone(), Style::new().fg(Color::Cyan)), Span::styled("  (a to open)", Style::new().fg(Color::DarkGray))]));
                        }
                        f.render_widget(Paragraph::new(l).wrap(Wrap { trim: true }), vrect);
                    }
                    let t = Table::new(rows, [Constraint::Min(12), Constraint::Length(17), Constraint::Length(8)])
                        .header(Row::new(["Variant", "Disk / est. RAM", "Fit now"]).style(Style::new().fg(Color::DarkGray)))
                        .row_highlight_style(Style::new().bg(Color::DarkGray).bold());
                    f.render_stateful_widget(t, trect, &mut rv.sel);
                }
            }
        }
        self.draw_downloads(f, dl);
    }

    fn draw_settings(&mut self, f: &mut Frame, area: Rect) {
        // Themed groups: a header row per section with a blank spacer before it.
        let narrow = area.width < 100;
        let mut rows: Vec<Row> = Vec::new();
        let mut row_settings = Vec::new();
        let mut display_sel = None;
        let mut section = "";
        for (i, (sec, k, help)) in SETTINGS.iter().enumerate() {
            if *sec != section {
                if !section.is_empty() {
                    rows.push(Row::new(vec![Cell::from("")]));
                    row_settings.push(None);
                }
                section = sec;
                rows.push(Row::new(vec![Cell::from(Span::styled(format!("▍{}", sec.to_uppercase()), Style::new().fg(Color::Magenta).bold()))]));
                row_settings.push(None);
            }
            if self.settings_sel.selected() == Some(i) {
                display_sel = Some(rows.len());
            }
            let short = k.split_once('.').map(|(_, b)| b).unwrap_or(k);
            let value = get_setting(&self.cfg, k);
            let value = match *k {
                "server.mem_reserve_gib" | "server.vm_reserve_gib" | "server.gpu_heap" => {
                    let max = if *k == "server.gpu_heap" { sys::mem().0 as f64 / (1u64 << 30) as f64 } else { 8.0 };
                    let v = value.parse::<f64>().unwrap_or(0.0);
                    let n = ((v / max) * 12.0).round().clamp(0.0, 12.0) as usize;
                    if value.parse::<f64>().is_ok() { format!("◂ {}{} {value} GiB ▸", "█".repeat(n), "░".repeat(12 - n)) } else { format!("◂ {value} ▸") }
                }
                _ => value,
            };
            let mut cells = vec![Cell::from(format!("  {short}")).fg(Color::Cyan), Cell::from(value)];
            if !narrow {
                cells.push(Cell::from(*help).fg(Color::DarkGray));
            }
            rows.push(Row::new(cells));
            row_settings.push(Some(i));
        }
        let widths: Vec<Constraint> = if narrow {
            vec![Constraint::Percentage(55), Constraint::Percentage(45)]
        } else {
            vec![Constraint::Length(26), Constraint::Percentage(30), Constraint::Min(20)]
        };
        let mut st = TableState::default().with_selected(display_sel);
        let t = Table::new(rows, widths)
            .row_highlight_style(Style::new().bg(Color::DarkGray).bold())
            .block(panel(&format!("Settings · {}", crate::config::config_path().display())));
        f.render_stateful_widget(t, area, &mut st);
        for row in 0..area.height.saturating_sub(if narrow { 3 } else { 2 }) {
            if let Some(Some(i)) = row_settings.get(st.offset() + row as usize) {
                self.clicks.push((Rect::new(area.x + 1, area.y + 1 + row, area.width.saturating_sub(2), 1), Click::Setting(*i)));
            }
        }
        if narrow {
            // The help text no longer fits a column: show it for the selected row.
            if let Some(i) = self.settings_sel.selected() {
                let r = Rect { x: area.x + 1, y: area.y + area.height.saturating_sub(2), width: area.width.saturating_sub(2), height: 1 };
                f.render_widget(Paragraph::new(SETTINGS[i].2).fg(Color::DarkGray), r);
            }
        }
    }

    fn draw_logs(&self, f: &mut Frame, area: Rect) {
        let lines: Vec<&str> = self.logs.lines().collect();
        let h = area.height.saturating_sub(2) as usize;
        let end = lines.len().saturating_sub(self.logs_scroll as usize);
        let start = end.saturating_sub(h);
        let text = lines[start..end].join("\n");
        let p = self.logs_path();
        f.render_widget(Paragraph::new(text).block(panel(&format!("{} {}", if self.logs_model { "Model log" } else { "Daemon log" }, p.display()))), area);
    }
}

// ---------- small helpers ----------

/// Content-sized buttons with two columns and one row of space between them.
fn button_cells(area: Rect, buttons: &[(&str, KeyCode)]) -> Vec<(usize, Rect)> {
    let mut cells = Vec::new();
    let (mut x, mut y) = (area.x, area.y);
    for (index, (label, _)) in buttons.iter().enumerate() {
        let width = (Line::from(*label).width() as u16 + 4).min(area.width);
        if x != area.x && x.saturating_add(width) > area.right() {
            x = area.x;
            y = y.saturating_add(2);
        }
        if width == 0 || y >= area.bottom() { break; }
        cells.push((index, Rect::new(x, y, width, 1)));
        x = x.saturating_add(width + 2);
    }
    cells
}

fn button_height(width: u16, buttons: &[(&str, KeyCode)]) -> u16 {
    button_cells(Rect::new(0, 0, width, u16::MAX), buttons)
        .last().map(|(_, r)| r.bottom()).unwrap_or(0)
}

struct DlRow {
    name: String,
    state: String,
    detail: String,
    total: u64,
    completed: u64,
    elapsed: f64,
}

fn all_downloads(s: &Option<Value>) -> Vec<DlRow> {
    s.as_ref()
        .and_then(|s| s["downloads"].as_array())
        .map(|a| {
            a.iter()
                .map(|d| DlRow {
                    name: d["name"].as_str().unwrap_or("").into(),
                    state: d["status"]["state"].as_str().unwrap_or("").into(),
                    detail: d["status"]["detail"].as_str().unwrap_or("").into(),
                    total: d["total"].as_u64().unwrap_or(0),
                    completed: d["completed"].as_u64().unwrap_or(0),
                    elapsed: d["elapsed_secs"].as_f64().unwrap_or(0.0),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn active_downloads(s: &Option<Value>) -> Vec<(String, u64)> {
    s.as_ref()
        .and_then(|s| s["downloads"].as_array())
        .map(|a| {
            a.iter()
                .filter(|d| matches!(d["status"]["state"].as_str(), Some("downloading" | "resolving" | "verifying")))
                .map(|d| (d["spec"].as_str().unwrap_or("").to_string(), d["completed"].as_u64().unwrap_or(0)))
                .collect()
        })
        .unwrap_or_default()
}

fn count_active(s: &Option<Value>) -> usize {
    active_downloads(s).len()
}

/// "Loading X — weights 6.7/15.0G · 32s · ETA 40s" for a /lmdeck/status load object.
pub fn load_label(l: &Value) -> String {
    let name = l["model"].as_str().unwrap_or("").rsplit('/').next().unwrap_or("").to_string();
    let el = l["elapsed_secs"].as_f64().unwrap_or(0.0);
    let detail = match l["phase"].as_str().unwrap_or("") {
        // Strata: the engine's own stage (and the share of the wait it is at)
        "loading" if l["stage"].is_string() => format!(
            "{} ({:.0}%)",
            l["stage"].as_str().unwrap_or(""),
            100.0 * l["progress"].as_f64().unwrap_or(0.0)
        ),
        "loading" => format!(
            "weights {}/{}{}",
            human(l["loaded_bytes"].as_u64().unwrap_or(0)),
            human(l["total_bytes"].as_u64().unwrap_or(0)),
            l["eta_secs"].as_f64().map(|e| format!(" · ETA {:.0}s", e)).unwrap_or_default()
        ),
        "warming up" => match l["warmup"].as_array() {
            Some(w) if w.len() == 2 => format!("warming up {}/{} tokens", w[0], w[1]),
            _ => "warming up (compiling GPU kernels)".into(),
        },
        "starting" => "starting backend".into(),
        p => p.to_string(),
    };
    format!("Loading {name} — {detail} · {el:.0}s")
}

fn panel(title: &str) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Color::DarkGray))
        .title(Span::styled(format!(" {title} "), Style::new().fg(Color::Cyan)))
}

fn kv(k: &str, v: &str) -> Line<'static> {
    Line::from(vec![Span::styled(format!("{k:<12}"), Style::new().fg(Color::DarkGray)), Span::raw(v.to_string())])
}

fn move_sel(s: &mut TableState, n: usize, d: i32) {
    if n == 0 {
        s.select(None);
        return;
    }
    let i = s.selected().unwrap_or(0) as i32 + d;
    s.select(Some(i.clamp(0, n as i32 - 1) as usize));
}

fn dur(s: u64) -> String {
    if s >= 3600 {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

fn compact(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 1000 {
        format!("{:.1}k", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn ui() -> Ui {
        let (tx, _) = mpsc::unbounded_channel();
        let mut ui = Ui::new(Config::default(), tx);
        ui.versions.mlx_selected = "Custom Installer".into();
        ui.versions.mlx_legacy = Some("0.32.4.dev-test".into());
        ui.versions.mlx_pkg = Some("0.7.28-1".into());
        ui
    }

    fn draw(ui: &mut Ui, width: u16, height: u16) -> String {
        let mut terminal = ratatui::Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| ui.draw(f)).unwrap();
        for (r, _) in &ui.clicks {
            assert!(r.right() <= width && r.bottom() <= height, "out-of-bounds click region: {r:?}");
        }
        for (i, (a, _)) in ui.clicks.iter().enumerate() {
            for (b, _) in &ui.clicks[i + 1..] {
                let overlap = a.intersection(*b);
                assert!(overlap.width == 0 || overlap.height == 0, "overlapping controls: {a:?} and {b:?}");
            }
        }
        terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect()
    }

    fn click_key(ui: &mut Ui, key: KeyCode) -> Option<Action> {
        let r = ui.clicks.iter().find_map(|(r, c)| match c {
            Click::Key(k) if *k == key => Some(*r),
            _ => None,
        }).expect("button must be visible and clickable");
        ui.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: r.x + r.width / 2, row: r.y, modifiers: KeyModifiers::NONE })
    }

    #[test]
    fn cancel_picker_selects_downloads_from_dashboard_and_open_repo() {
        for tab in [Tab::Dashboard, Tab::Discover] {
            let mut ui = ui();
            ui.tab = tab;
            ui.status = Some(serde_json::json!({"downloads": [
                {"spec": "org/a", "name": "org/a", "status": {"state": "downloading"}},
                {"spec": "org/finished", "status": {"state": "done"}},
                {"spec": "org/b", "name": "org/b", "status": {"state": "resolving"}}
            ]}));
            ui.repo = Some(RepoView { repo: "org/a".into(), files: None, err: None, sel: TableState::default() });
            ui.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
            assert!(matches!(&ui.input, Input::Downloads(specs, 0) if specs == &vec!["org/a".to_string(), "org/b".to_string()]));
            ui.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
            assert!(matches!(ui.input, Input::Downloads(_, 1)));
            assert!(draw(&mut ui, 80, 24).contains("org/b"));
            ui.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
            assert!(matches!(ui.input, Input::None));
        }
    }

    #[test]
    fn search_budget_accepts_decimal_gib_and_rejects_invalid_values() {
        assert_eq!(parse_search_budget(" 8.5 ").unwrap(), Some((8 << 30) + (1 << 29)));
        assert_eq!(parse_search_budget(" ").unwrap(), None);
        for value in ["0", "-2", "NaN", "inf", "1e50", "nonsense"] {
            assert!(parse_search_budget(value).is_err(), "accepted {value}");
        }
    }

    #[tokio::test]
    async fn discovery_controls_cycle_all_formats_and_apply_budget() {
        let mut ui = ui();
        ui.tab = Tab::Discover;
        for kind in ["gguf", "both", "mlx"] {
            ui.key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
            assert_eq!(ui.search_kind(), kind);
            assert!(!ui.catalog_mode);
        }
        ui.key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE));
        assert!(matches!(ui.input, Input::SearchBudget));
        ui.buf = "bad".into();
        ui.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(ui.input, Input::SearchBudget));
        ui.buf = "8.5".into();
        ui.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(ui.search_budget, Some((8 << 30) + (1 << 29)));
        assert!(matches!(ui.input, Input::None));
        for (w, h) in [(60, 20), (80, 24), (120, 36)] { draw(&mut ui, w, h); }
        ui.key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE));
        ui.buf.clear();
        ui.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(ui.search_budget, None);
        ui.search_task.take().unwrap().abort();
    }

    #[test]
    fn opened_repo_obeys_format_and_budget_filters() {
        let mut ui = ui();
        ui.tab = Tab::Discover;
        ui.catalog_mode = false;
        let file = |path: &str, size| RepoFile { path: path.into(), size, sha256: None, blob: String::new() };
        ui.repo = Some(RepoView {
            repo: "org/mixed".into(),
            files: Some(vec![file("config.json", 128), file("model.safetensors", 4 << 30),
                file("model-Q4_K_M.gguf", 2 << 30), file("model-Q8_0.gguf", 6 << 30)]),
            err: None, sel: TableState::default().with_selected(Some(0)),
        });
        let mlx = ui.repo_choices();
        assert_eq!(mlx.len(), 1);
        assert_eq!(mlx[0].0, "org/mixed");
        ui.search_kind = 1;
        assert!(ui.repo_choices().iter().all(|v| v.0.contains(':')));
        ui.search_kind = 2;
        assert_eq!(ui.repo_choices().len(), 3);
        ui.search_budget = Some(crate::compat::estimated_memory(&ui.cfg, 2 << 30, "gguf"));
        let fits = ui.repo_choices();
        assert_eq!(fits.len(), 1);
        assert!(fits[0].0.ends_with(":Q4_K_M"));
        let text = draw(&mut ui, 120, 36);
        assert!(text.contains("Q4_K_M"));
        assert!(!text.contains("Q8_0"));
    }

    #[test]
    fn stale_search_cannot_replace_newer_results() {
        let mut ui = ui();
        ui.search_generation = 2;
        ui.searching = true;
        ui.handle(Msg::Search(1, Ok((vec![RepoHit { id: "org/old".into(), ..Default::default() }], 0))));
        assert!(ui.hits.is_empty());
        assert!(ui.searching);
        ui.handle(Msg::Search(2, Ok((vec![RepoHit { id: "org/new".into(), ..Default::default() }], 0))));
        assert_eq!(ui.hits[0].id, "org/new");
        assert!(!ui.searching);
    }

    #[test]
    fn clickable_runtime_removal_requires_confirmation() {
        let mut ui = ui();
        draw(&mut ui, 120, 36);
        assert!(click_key(&mut ui, KeyCode::Char('i')).is_none());
        assert!(matches!(ui.input, Input::Runtime));
        draw(&mut ui, 120, 36);
        assert!(click_key(&mut ui, KeyCode::Char('2')).is_none());
        assert!(matches!(&ui.input, Input::RuntimeKeep(source) if source == "custom"));
        draw(&mut ui, 120, 36);
        assert!(click_key(&mut ui, KeyCode::Char('2')).is_none());
        assert!(matches!(ui.input, Input::Confirm(..)));
        draw(&mut ui, 120, 36);
        assert!(click_key(&mut ui, KeyCode::Char('n')).is_none());
        assert!(matches!(ui.input, Input::None));
    }

    #[test]
    fn gguf_setup_explains_optional_backend_and_blocks_background_clicks() {
        let mut ui = ui();
        draw(&mut ui, 120, 36);
        assert!(click_key(&mut ui, KeyCode::Char('B')).is_none());
        let text = draw(&mut ui, 120, 36);
        assert!(text.contains("optional GGUF backend"));
        assert!(text.contains("MLX models do not need this"));
        assert!(!ui.clicks.iter().any(|(_, c)| matches!(c, Click::Key(KeyCode::Char('s')))));
        click_key(&mut ui, KeyCode::Char('n'));
        assert!(matches!(ui.input, Input::None));
    }

    #[test]
    fn dialogs_remain_clickable_on_small_terminals() {
        for (width, height) in [(120, 36), (80, 24), (40, 12)] {
            let mut ui = ui();
            let text = draw(&mut ui, width, height);
            if width >= 80 { assert!(text.contains("MLX runtime")); }
            ui.input = Input::Guide;
            draw(&mut ui, width, height);
            click_key(&mut ui, KeyCode::Char('q'));
            assert!(matches!(ui.input, Input::None));
            ui.input = Input::Update;
            draw(&mut ui, width, height);
            click_key(&mut ui, KeyCode::Char('q'));
            assert!(matches!(ui.input, Input::None));
        }
    }

    #[test]
    fn tabs_and_settings_support_mouse_navigation() {
        let mut ui = ui();
        draw(&mut ui, 120, 36);
        click_key(&mut ui, KeyCode::Char('4'));
        assert!(ui.tab == Tab::Settings);
        draw(&mut ui, 120, 36);
        let (r, index) = ui.clicks.iter().find_map(|(r, c)| match c {
            Click::Setting(i) => Some((*r, *i)),
            _ => None,
        }).unwrap();
        ui.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: r.x, row: r.y, modifiers: KeyModifiers::NONE });
        assert!(matches!(ui.input, Input::Setting(i) if i == index));
        assert_eq!(ui.buf, get_setting(&ui.cfg, SETTINGS[index].1));
    }

    #[test]
    fn buttons_wrap_with_spacing_and_use_soft_colors() {
        let buttons = [("First [1]", KeyCode::Char('1')), ("Second [2]", KeyCode::Char('2')), ("Third [3]", KeyCode::Char('3'))];
        let cells = button_cells(Rect::new(0, 0, 32, 10), &buttons);
        assert_eq!(cells.len(), 3);
        assert!(cells[0].1.right() + 2 <= cells[1].1.x);
        assert_eq!(cells[2].1.y, 2);
        assert_eq!(button_height(32, &buttons), 3);
        let mut ui = ui();
        let mut terminal = ratatui::Terminal::new(TestBackend::new(32, 10)).unwrap();
        terminal.draw(|f| ui.draw_buttons(f, f.area(), &buttons)).unwrap();
        let cell = &terminal.backend().buffer()[(0, 0)];
        assert_eq!(cell.bg, Color::Rgb(62, 68, 78));
        assert_eq!(cell.fg, Color::Rgb(134, 201, 190));
    }
}
