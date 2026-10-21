//! Terminal UI. Event-driven: redraws only on input or background updates, polls the daemon
//! once per second. Uses ANSI palette colors so the Omarchy theme applies.
//!
//! Every screen has an action bar at the top that lists exactly the actions its keys run, and
//! each action knows when it does not apply (it is shown dimmed, and its key says why). Only
//! the keys in `GLOBAL_KEYS` work everywhere.

mod backends;
mod discover;
mod home;
mod logs;
mod models;
mod popup;
mod settings;
mod widgets;

use crate::client::{self, Daemon};
use crate::config::Config;
use crate::hf::{RepoFile, RepoHit};
use crate::models::LocalModel;
use crate::sys::{self, Versions};
use anyhow::Result;
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use popup::Popup;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{TableState, Tabs};
use ratatui::{DefaultTerminal, Frame};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
pub use widgets::load_label;
use widgets::Act;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tab {
    Home,
    Models,
    Discover,
    Backends,
    Settings,
    Logs,
}
const TABS: [Tab; 6] = [Tab::Home, Tab::Models, Tab::Discover, Tab::Backends, Tab::Settings, Tab::Logs];
impl Tab {
    fn title(self) -> &'static str {
        match self {
            Tab::Home => "Home",
            Tab::Models => "Models",
            Tab::Discover => "Get models",
            Tab::Backends => "Backends",
            Tab::Settings => "Settings",
            Tab::Logs => "Logs",
        }
    }
}

/// Keys that work on every screen (shown in the Keys popup).
const GLOBAL_KEYS: &[(&str, &str)] = &[
    ("1-6, Tab", "switch screens"),
    ("g", "recommend a model (Models sorted by the grader; again: next goal)"),
    ("?", "keys of this screen"),
    ("q", "quit (the server keeps running)"),
];

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
    /// The planner's decision for each local model (`/lmdeck/plan`).
    Plans(std::collections::HashMap<String, Value>),
}

#[derive(Clone, Debug)]
enum Action {
    Script(String, String), // title, bash
    Exec(String, Vec<String>),
    /// An editor: runs in the terminal and returns straight to lmdeck (no "press Enter").
    Edit(Vec<String>),
}

#[derive(Clone, Copy)]
enum Click {
    Key(KeyCode),
    Row(usize),
}

struct RepoView {
    repo: String,
    files: Option<Vec<RepoFile>>,
    err: Option<String>,
    sel: TableState,
}

struct Ui {
    tab: Tab,
    cfg: Config,
    d: Daemon,
    status: Option<Value>,
    versions: Versions,
    models: Vec<LocalModel>,
    popup: Option<Popup>,
    /// A prompt file open in the editor, and the text a new model file was seeded with.
    editing: Option<(std::path::PathBuf, Option<String>)>,
    toast: Option<(String, Instant)>,
    quit: bool,
    tx: mpsc::UnboundedSender<Msg>,
    clicks: Vec<(Rect, Click)>,
    // Models
    models_sel: TableState,
    sort: models::Sort,
    grade: Option<crate::grade::View>,
    /// The planner's last decision per model; the grade's Fit and the detail pane use it.
    plans: std::collections::HashMap<String, Value>,
    // Get models
    query: String,
    search_kind: usize,
    search_budget: Option<u64>,
    search_sort: usize,
    catalog_mode: bool,
    catalog: Vec<Value>,
    hits: Vec<RepoHit>,
    hits_sel: TableState,
    searching: bool,
    search_generation: u64,
    search_task: Option<tokio::task::JoinHandle<()>>,
    repo: Option<RepoView>,
    // Backends
    backends_sel: usize,
    // Settings
    settings_sel: TableState,
    // Logs
    logs_model: bool,
    logs_scroll: u16,
    logs: String,
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
    let restore = |term: &mut DefaultTerminal| {
        *term = ratatui::init();
        let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture);
        let _ = term.clear();
        pause.store(false, Ordering::Relaxed);
    };
    let (title, mut cmd) = match a {
        Action::Edit(argv) => {
            let _ = std::process::Command::new(&argv[0]).args(&argv[1..]).status();
            restore(term);
            return;
        }
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
    match cmd.status() {
        Ok(s) if s.success() => println!("\n\x1b[1;32m✓ finished\x1b[0m"),
        Ok(s) => println!("\n\x1b[1;31m✗ exited with {s}\x1b[0m"),
        Err(e) => println!("\n\x1b[1;31m✗ {e}\x1b[0m"),
    }
    print!("Press Enter to return to lmdeck…");
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let _ = std::io::stdin().read_line(&mut String::new());
    restore(term);
}

impl Ui {
    fn new(cfg: Config, tx: mpsc::UnboundedSender<Msg>) -> Self {
        Self {
            tab: Tab::Home,
            d: Daemon::new(cfg.base_url()),
            models: crate::models::scan(&cfg),
            catalog: crate::compat::catalog().to_vec(),
            cfg,
            status: None,
            versions: Versions::default(),
            popup: None,
            editing: None,
            toast: None,
            quit: false,
            tx,
            clicks: vec![],
            models_sel: TableState::default().with_selected(Some(0)),
            sort: models::Sort::Recommended,
            grade: None,
            plans: Default::default(),
            query: String::new(),
            search_kind: 0,
            search_budget: None,
            search_sort: 0,
            catalog_mode: true,
            hits: vec![],
            hits_sel: TableState::default().with_selected(Some(0)),
            searching: false,
            search_generation: 0,
            search_task: None,
            repo: None,
            backends_sel: 0,
            settings_sel: TableState::default().with_selected(Some(0)),
            logs_model: false,
            logs_scroll: 0,
            logs: String::new(),
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
            v.mlx_upstream = sys::upstream_release(&crate::hf::client()).await;
            let _ = tx.send(Msg::Versions(Box::new(v)));
        });
    }

    fn rescan(&mut self) {
        self.models = crate::models::scan(&self.cfg);
        self.grade = None;
        let n = self.models.len();
        if self.models_sel.selected().is_none_or(|s| s >= n) {
            self.models_sel.select(n.checked_sub(1));
        }
    }

    fn running(&self) -> Vec<Value> {
        self.status.as_ref().and_then(|s| s["running"].as_array().cloned()).unwrap_or_default()
    }

    fn running_names(&self) -> Vec<String> {
        self.running().iter().filter_map(|r| r["name"].as_str().map(str::to_string)).collect()
    }

    /// Run `f` against the daemon (starting it first if needed); its result becomes a toast.
    fn spawn_daemon_op<F, Fut>(&self, label: String, f: F)
    where
        F: FnOnce(Daemon) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<String>> + Send,
    {
        let d = self.d.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            if !ensure_daemon(&d, &tx).await {
                return;
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

    /// Change the config file (only the keys `f` changes) and tell the daemon.
    fn edit_config(&mut self, what: &str, f: impl FnOnce(&mut Config) -> Result<(), String>) -> bool {
        match Config::edit(f) {
            Ok(c) => {
                let restart = c.server.host != self.cfg.server.host || c.server.port != self.cfg.server.port;
                self.cfg = c;
                let d = self.d.clone();
                tokio::spawn(async move {
                    let _ = d.reload().await;
                });
                if restart {
                    self.toast(format!("saved {what}: restart the server (s on Home) to apply"));
                    self.d = Daemon::new(self.cfg.base_url());
                } else {
                    self.toast(format!("saved {what}"));
                }
                self.grade = None;
                true
            }
            Err(e) => {
                self.toast(format!("{what}: {e}"));
                false
            }
        }
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
                    self.toast("saved: no system prompt for this model")
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
                if self.toast.as_ref().is_some_and(|(_, t)| t.elapsed() > Duration::from_secs(6)) {
                    self.toast = None;
                }
            }
            Msg::Resize => {}
            Msg::Status(s) => {
                // Rescan models when a download finishes; re-plan when what is loaded changes.
                let was = widgets::active_downloads(&self.status).len();
                let now = widgets::active_downloads(&s).len();
                let names = |s: &Option<Value>| s.as_ref().and_then(|s| s["running"].as_array().cloned()).unwrap_or_default().iter().filter_map(|r| r["name"].as_str().map(str::to_string)).collect::<Vec<_>>();
                let reloaded = names(&self.status) != names(&s) || self.status.is_some() != s.is_some();
                self.status = s;
                if now < was {
                    self.rescan();
                } else if reloaded {
                    self.grade = None;
                }
            }
            Msg::Plans(p) => {
                if let Some(g) = &mut self.grade {
                    g.apply_plans(&p);
                }
                self.plans = p;
            }
            Msg::Versions(v) => self.versions = *v,
            Msg::Search(generation, r) => self.search_done(generation, r),
            Msg::Repo(repo, r) => {
                if let Some(rv) = self.repo.as_mut().filter(|rv| rv.repo == repo) {
                    match r {
                        Ok(f) => rv.files = Some(f),
                        Err(e) => rv.err = Some(e),
                    }
                }
            }
            Msg::Toast(s) => self.toast(s),
            Msg::RetryLoad(model) => {
                self.popup = None;
                self.load_model(model);
            }
            Msg::NeedMemory(model, why) => {
                if why.contains("needs ~") {
                    self.toast(format!("{model}: not enough free memory"));
                }
                self.popup = Some(Popup::free_memory(Some((model, why))));
            }
            Msg::GpuGap(model, rec) => {
                self.toast(format!("{model}: no GPU kernel for one of its ops"));
                self.popup = Some(Popup::GpuGap(model, rec));
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

    fn set_tab(&mut self, t: Tab) {
        self.tab = t;
        if t == Tab::Logs {
            self.load_logs();
        }
    }

    /// The actions of the current screen, in action-bar order.
    fn actions(&self) -> Vec<Act> {
        match self.tab {
            Tab::Home => self.home_actions(),
            Tab::Models => self.models_actions(),
            Tab::Discover => self.discover_actions(),
            Tab::Backends => self.backends_actions(),
            Tab::Settings => self.settings_actions(),
            Tab::Logs => self.logs_actions(),
        }
    }

    fn key(&mut self, k: KeyEvent) -> Option<Action> {
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            self.quit = true;
            return None;
        }
        if self.popup.is_some() {
            return self.popup_key(k);
        }
        match k.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.popup = Some(Popup::Keys(0)),
            KeyCode::Char('g') => self.recommend(),
            KeyCode::Char(c @ '1'..='6') => self.set_tab(TABS[(c as u8 - b'1') as usize]),
            KeyCode::Tab | KeyCode::BackTab => {
                let i = TABS.iter().position(|t| *t == self.tab).unwrap();
                let n = TABS.len();
                self.set_tab(TABS[if k.code == KeyCode::Tab { (i + 1) % n } else { (i + n - 1) % n }]);
            }
            code => {
                if let Some(a) = self.actions().into_iter().find(|a| a.keys.contains(&code)) {
                    if let Some(why) = a.why_not {
                        self.toast(why);
                        return None;
                    }
                    return self.act(code);
                }
                self.nav(k);
            }
        }
        None
    }

    /// Run the current screen's action for `key`, one of that action's keys (already checked).
    fn act(&mut self, key: KeyCode) -> Option<Action> {
        match self.tab {
            Tab::Home => self.home_act(key),
            Tab::Models => self.models_act(key),
            Tab::Discover => self.discover_act(key),
            Tab::Backends => self.backends_act(key),
            Tab::Settings => self.settings_act(key),
            Tab::Logs => self.logs_act(key),
        }
    }

    /// Movement keys of the current screen.
    fn nav(&mut self, k: KeyEvent) {
        let d = match k.code {
            KeyCode::Down | KeyCode::Char('j') => 1,
            KeyCode::Up | KeyCode::Char('k') => -1,
            KeyCode::PageDown => 10,
            KeyCode::PageUp => -10,
            KeyCode::Home => -100_000,
            KeyCode::End => 100_000,
            _ => return,
        };
        match self.tab {
            Tab::Home => {}
            Tab::Models => {
                let n = self.models.len();
                widgets::move_sel(&mut self.models_sel, n, d);
            }
            Tab::Discover => self.discover_nav(d),
            Tab::Backends => {
                let n = self.backend_items().len();
                self.backends_sel = (self.backends_sel as i64 + d as i64).clamp(0, n.saturating_sub(1) as i64) as usize;
            }
            Tab::Settings => widgets::move_sel(&mut self.settings_sel, crate::config::SETTINGS.len(), d),
            Tab::Logs => {
                self.logs_scroll = (self.logs_scroll as i64 - d as i64).clamp(0, u16::MAX as i64) as u16;
                self.load_logs();
            }
        }
    }

    fn mouse(&mut self, m: MouseEvent) -> Option<Action> {
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let hit = |r: &Rect| m.column >= r.x && m.column < r.right() && m.row >= r.y && m.row < r.bottom();
                let click = self.clicks.iter().rev().find(|(r, _)| hit(r)).map(|(_, c)| *c);
                match click {
                    Some(Click::Key(k)) => return self.key(KeyEvent::new(k, KeyModifiers::NONE)),
                    Some(Click::Row(i)) => return self.click_row(i),
                    None => {}
                }
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let code = if m.kind == MouseEventKind::ScrollDown { KeyCode::Down } else { KeyCode::Up };
                for _ in 0..3 {
                    if self.popup.is_some() {
                        self.popup_key(KeyEvent::new(code, KeyModifiers::NONE));
                    } else {
                        self.nav(KeyEvent::new(code, KeyModifiers::NONE));
                    }
                }
            }
            _ => {}
        }
        None
    }

    /// A click on row `i` of the current screen's list selects it; a second click runs Enter.
    fn click_row(&mut self, i: usize) -> Option<Action> {
        if let Some(Popup::ModelSettings(_, sel)) = &mut self.popup {
            let again = std::mem::replace(sel, i) == i;
            return if again { self.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)) } else { None };
        }
        let again = match self.tab {
            Tab::Models => std::mem::replace(&mut self.models_sel, TableState::default().with_selected(Some(i))).selected() == Some(i),
            Tab::Discover => self.discover_click(i),
            Tab::Backends => std::mem::replace(&mut self.backends_sel, i) == i,
            Tab::Settings => std::mem::replace(&mut self.settings_sel, TableState::default().with_selected(Some(i))).selected() == Some(i),
            _ => false,
        };
        if again { self.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)) } else { None }
    }

    /// `g`: the Models screen sorted by the grader; pressed again, the next goal.
    fn recommend(&mut self) {
        if self.models.is_empty() {
            self.toast("no local models yet: find some in Get models (3)");
            return;
        }
        if self.tab == Tab::Models && self.sort == models::Sort::Recommended {
            let g = self.grade_view();
            g.next_goal(1);
        } else {
            self.sort = models::Sort::Recommended;
            self.grade = None;
            self.set_tab(Tab::Models);
        }
        self.models_sel.select(Some(0));
        let goal = crate::grade::GOALS[self.grade_view().goal].0;
        self.toast(format!("recommended for: {goal} (g: next goal)"));
    }

    /// Load through the daemon; a missing GPU kernel or a lack of memory opens the popup that
    /// deals with it instead of just a toast.
    fn load_model(&self, name: String) {
        let d = self.d.clone();
        let tx = self.tx.clone();
        let cfg = self.cfg.clone();
        let local = self.models.iter().find(|m| m.name == name).cloned();
        tokio::spawn(async move {
            if !ensure_daemon(&d, &tx).await {
                return;
            }
            let _ = tx.send(Msg::Toast(format!("loading {name}…")));
            match d.load(&name).await {
                Ok(v) => {
                    let _ = tx.send(Msg::Toast(format!("{name} ready ({:.1}s)", v["load_secs"].as_f64().unwrap_or(0.0))));
                }
                Err(e) if crate::compat::is_gpu_gap(&e.to_string()) => {
                    let rec = local
                        .filter(|m| m.format == crate::models::Format::Mlx)
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

    // ---------- drawing ----------

    fn draw(&mut self, f: &mut Frame) {
        self.clicks.clear();
        let [top, body, foot] = Layout::vertical([Constraint::Length(1), Constraint::Min(5), Constraint::Length(1)]).areas(f.area());
        self.draw_tabs(f, top);
        let acts = self.actions();
        let bar_h = widgets::button_height(body.width.saturating_sub(2), &acts);
        let [bar, _, content] = Layout::vertical([Constraint::Length(bar_h), Constraint::Length(1), Constraint::Min(0)]).areas(body);
        self.draw_actions(f, bar.inner(ratatui::layout::Margin { horizontal: 1, vertical: 0 }), &acts);
        match self.tab {
            Tab::Home => self.draw_home(f, content),
            Tab::Models => self.draw_models(f, content),
            Tab::Discover => self.draw_discover(f, content),
            Tab::Backends => self.draw_backends(f, content),
            Tab::Settings => self.draw_settings(f, content),
            Tab::Logs => self.draw_logs(f, content),
        }
        let foot_line = match &self.toast {
            Some((t, _)) => Line::from(Span::styled(t.clone(), Style::new().fg(Color::Yellow))),
            None => Line::from(Span::styled("? keys · g recommend a model · q quit (the server keeps running)", Style::new().fg(Color::DarkGray))),
        };
        f.render_widget(foot_line, foot);
        if self.popup.is_some() {
            self.draw_popup(f);
        }
    }

    fn draw_tabs(&mut self, f: &mut Frame, top: Rect) {
        let narrow = top.width < 90;
        let idx = TABS.iter().position(|t| *t == self.tab).unwrap();
        let titles: Vec<Line> = TABS
            .iter()
            .enumerate()
            .map(|(i, t)| Line::from(if narrow && i != idx { format!("{}", i + 1) } else { format!("{} {}", i + 1, t.title()) }))
            .collect();
        let up = self.status.is_some();
        let [tabs_area, badge] = Layout::horizontal([Constraint::Min(10), Constraint::Length(if narrow { 9 } else { 26 })]).areas(top);
        let mut x = tabs_area.x;
        for (i, title) in titles.iter().enumerate() {
            let w = (title.width() as u16 + 2).min(tabs_area.right().saturating_sub(x));
            if w > 0 {
                self.clicks.push((Rect::new(x, tabs_area.y, w, 1), Click::Key(KeyCode::Char((b'1' + i as u8) as char))));
            }
            x = x.saturating_add(w + 1);
        }
        f.render_widget(Tabs::new(titles).select(idx).highlight_style(Style::new().fg(Color::Black).bg(Color::Cyan).bold()).divider(" "), tabs_area);
        let dot = if up { Span::styled("● ", Style::new().fg(Color::Green)) } else { Span::styled("○ ", Style::new().fg(Color::DarkGray)) };
        let text = if narrow { format!(":{}", self.cfg.server.port) } else { format!("{} :{}", if up { "serving" } else { "stopped" }, self.cfg.server.port) };
        f.render_widget(Line::from(vec![dot, Span::styled(text, Style::new().fg(Color::DarkGray))]).right_aligned(), badge);
    }

    fn draw_actions(&mut self, f: &mut Frame, area: Rect, acts: &[Act]) {
        for (i, cell) in widgets::button_cells(area, acts) {
            let a = &acts[i];
            let style = if a.why_not.is_some() {
                Style::new().fg(Color::DarkGray).bg(Color::Rgb(44, 48, 56))
            } else {
                Style::new().fg(Color::Rgb(134, 201, 190)).bg(Color::Rgb(62, 68, 78))
            };
            f.render_widget(ratatui::widgets::Paragraph::new(a.button()).centered().style(style), cell);
            self.clicks.push((cell, Click::Key(a.keys[0])));
        }
    }
}

/// Start the daemon if it is not running and wait for it (up to 5 s).
async fn ensure_daemon(d: &Daemon, tx: &mpsc::UnboundedSender<Msg>) -> bool {
    if d.up().await {
        return true;
    }
    let _ = tx.send(Msg::Toast("starting the server…".into()));
    if let Err(e) = client::start_daemon() {
        let _ = tx.send(Msg::Toast(format!("server start failed: {e}")));
        return false;
    }
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if d.up().await {
            return true;
        }
    }
    let _ = tx.send(Msg::Toast("the server did not come up; see Logs (6)".into()));
    false
}

#[cfg(test)]
mod tests;
