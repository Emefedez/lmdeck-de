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
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
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
    Resize,
    Tick,
    Status(Option<Value>),
    Versions(Box<Versions>),
    Search(Result<Vec<RepoHit>, String>),
    Repo(String, Result<Vec<RepoFile>, String>),
    Toast(String),
    Models,
    /// Load failed for lack of a GPU kernel: (model, recommended download).
    GpuGap(String, Option<String>),
}

#[derive(Clone)]
enum Action {
    Script(String, String), // title, bash
    Exec(String, Vec<String>),
}

enum Input {
    None,
    Search,
    Setting(usize),
    Confirm(String, Confirmed),
    Override(String),
    GpuGap(String, Option<String>),
}

#[derive(Clone)]
enum Confirmed {
    ForceStop,
    Device(String, String),
    Delete(String),
    Run(Action),
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
    models_sel: TableState,
    query: String,
    kind_gguf: bool,
    catalog_mode: bool,
    catalog: Vec<Value>,
    hits: Vec<RepoHit>,
    hits_sel: TableState,
    searching: bool,
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
    let mut ui = Ui {
        tab: Tab::Dashboard,
        d: Daemon::new(cfg.base_url()),
        models: models::scan(&cfg),
        catalog: crate::compat::catalog().to_vec(),
        cfg,
        status: None,
        versions: Versions::default(),
        models_sel: TableState::default().with_selected(Some(0)),
        query: String::new(),
        kind_gguf: false,
        catalog_mode: true,
        hits: vec![],
        hits_sel: TableState::default().with_selected(Some(0)),
        searching: false,
        repo: None,
        settings_sel: TableState::default().with_selected(Some(0)),
        input: Input::None,
        buf: String::new(),
        logs_model: false,
        logs_scroll: 0,
        logs: String::new(),
        toast: None,
        quit: false,
        tx: tx.clone(),
    };
    ui.refresh_versions();
    ui.poll_status();

    let mut term = ratatui::init();
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
    ratatui::restore();
    let (title, mut cmd) = match a {
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
    let _ = term.clear();
    pause.store(false, Ordering::Relaxed);
}

// ---------- state & events ----------

impl Ui {
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
            Msg::Search(r) => {
                self.searching = false;
                match r {
                    Ok(h) => {
                        self.hits = h;
                        self.hits_sel.select(Some(0));
                    }
                    Err(e) => self.toast(format!("search failed: {e}")),
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
            Msg::GpuGap(model, rec) => {
                self.toast(format!("{model}: no GPU kernel for one of its ops"));
                self.input = Input::GpuGap(model, rec);
            }
            Msg::Models => {
                self.cfg = Config::load().unwrap_or_else(|_| self.cfg.clone());
                self.rescan();
            }
            Msg::Key(k) => return self.key(k),
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

    fn key_input(&mut self, k: KeyEvent) -> Option<Action> {
        let input = std::mem::replace(&mut self.input, Input::None);
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
                        "Migrate mlx-omarchy to the Omarchy packages{}? Uses omarchy-pkg-add + mlx-omarchy-retire-legacy (sudo). [y/N]",
                        if ane { " (+ANE driver)" } else { "" }
                    ),
                    Confirmed::Run(Action::Script("migrate to omarchy-mlx packages".into(), sys::script_migrate(ane))),
                );
            }
            KeyCode::Char('U') => {
                self.input = Input::Confirm(
                    "Update omarchy-mlx (runs a full `pacman -Syu`, Arch doesn't support partial upgrades)? [y/N]".into(),
                    Confirmed::Run(Action::Script("update omarchy-mlx".into(), sys::script_update_mlx())),
                );
            }
            KeyCode::Char('B') => {
                self.input = Input::Confirm(
                    format!("Build llama.cpp ({}) with Vulkan? Takes a few minutes. [y/N]", self.cfg.llama.git_ref),
                    Confirmed::Run(Action::Script("build llama.cpp (Vulkan)".into(), sys::script_build_llama(&self.cfg))),
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

    fn search(&mut self) {
        let q = self.query.clone();
        let kind = if self.kind_gguf { "gguf" } else { "mlx" };
        let tx = self.tx.clone();
        self.searching = true;
        self.repo = None;
        tokio::spawn(async move {
            let r = hf::search(&hf::client(), &q, kind, 60).await.map_err(|e| e.to_string());
            let _ = tx.send(Msg::Search(r));
        });
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
        hf::choices(&rv.repo, files, &self.cfg.download.gguf_default_quant)
            .into_iter()
            .map(|c| (c.spec, if c.default && c.format == "gguf" { format!("{} ★", c.label) } else { c.label }, c.size))
            .collect()
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
                self.kind_gguf = !self.kind_gguf;
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
            KeyCode::Char('x') => {
                let active: Vec<String> = active_downloads(&self.status).into_iter().map(|d| d.0).collect();
                if let Some(spec) = active.first().cloned() {
                    self.spawn_daemon_op(format!("cancelling {spec}"), move |d| async move {
                        d.cancel(&spec).await?;
                        Ok(format!("cancelled {spec}"))
                    });
                }
            }
            _ => {}
        }
        None
    }

    fn key_settings(&mut self, k: KeyEvent) -> Option<Action> {
        match k.code {
            KeyCode::Down | KeyCode::Char('j') => move_sel(&mut self.settings_sel, SETTINGS.len(), 1),
            KeyCode::Up | KeyCode::Char('k') => move_sel(&mut self.settings_sel, SETTINGS.len(), -1),
            KeyCode::Enter | KeyCode::Char('e') => {
                let i = self.settings_sel.selected().unwrap_or(0);
                self.buf = get_setting(&self.cfg, SETTINGS[i].1);
                self.input = Input::Setting(i);
            }
            KeyCode::Char('o') => {
                let p = crate::config::config_path();
                if !p.exists() {
                    let _ = self.cfg.save();
                }
                let editor = std::env::var("EDITOR").unwrap_or_else(|_| "nvim".into());
                return Some(Action::Exec("edit config".into(), vec![editor, p.display().to_string()]));
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
            Tab::Dashboard => "s start/stop · X force stop · R reload stale · S service · a unload all · M migrate · U update · B llama.cpp · D launcher · C clear dl",
            Tab::Models => "⏎ load · u unload · g GPU⇄CPU · c chat · x context · d delete · y copy · ✓ qualified ? untested ✗ GPU-incompatible",
            Tab::Discover => {
                if self.repo.is_some() {
                    "⏎ download · a recommended alternative · esc back"
                } else {
                    "/ search · g MLX⇄GGUF · c catalog⇄search · ⏎ open · x cancel download"
                }
            }
            Tab::Settings => "⏎ edit · o open config in $EDITOR",
            Tab::Logs => "m daemon⇄model log · j/k scroll · G end",
        };
        let foot_line = match &self.toast {
            Some((t, _)) => Line::from(Span::styled(t.clone(), Style::new().fg(Color::Yellow))),
            None => Line::from(vec![Span::styled(help, Style::new().fg(Color::DarkGray)), Span::styled("  · q closes TUI (server keeps running)", Style::new().fg(Color::DarkGray))]),
        };
        f.render_widget(foot_line, foot);
        self.draw_input(f);
    }

    fn draw_input(&self, f: &mut Frame) {
        let (title, text) = match &self.input {
            Input::None => return,
            Input::Search => (format!("Search Hugging Face ({})", if self.kind_gguf { "GGUF" } else { "MLX" }), format!("{}▏", self.buf)),
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
        f.render_widget(Clear, r);
        f.render_widget(
            Paragraph::new(text).wrap(Wrap { trim: true }).block(Block::bordered().border_type(BorderType::Rounded).title(title).border_style(Style::new().fg(Color::Cyan))),
            r,
        );
    }

    fn draw_dashboard(&self, f: &mut Frame, area: Rect) {
        let narrow = area.width < 100;
        let (srv, loaded, usage_area, dls, right) = if narrow {
            let [a, b, c, d, e] = Layout::vertical([
                Constraint::Length(10),
                Constraint::Length(5),
                Constraint::Length(8),
                Constraint::Length(5),
                Constraint::Min(6),
            ])
            .areas(area);
            (a, b, c, d, e)
        } else {
            let [left, right] = Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(area);
            let [a, b, c, d] =
                Layout::vertical([Constraint::Length(10), Constraint::Min(4), Constraint::Length(9), Constraint::Length(7)]).areas(left);
            (a, b, c, d, right)
        };

        // Server
        let mut lines = vec![];
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
            None => lines.push(Line::from(Span::styled("○ stopped — press s to start", Style::new().fg(Color::DarkGray)))),
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
        let t = Table::new(rows, [Constraint::Min(20), Constraint::Length(5), Constraint::Length(7), Constraint::Length(8), Constraint::Length(7)])
            .header(Row::new(["Model", "Fmt", "RSS", "Unload", "Load"]).style(Style::new().fg(Color::DarkGray)))
            .block(panel("Loaded"));
        f.render_widget(t, loaded);
        if empty {
            let inner = loaded.inner(ratatui::layout::Margin { horizontal: 2, vertical: 2 });
            f.render_widget(Paragraph::new("nothing loaded — models load on first request").fg(Color::DarkGray), inner);
        }

        self.draw_usage(f, usage_area);
        self.draw_downloads(f, dls);

        // Runtime
        let v = &self.versions;
        let mut rl = vec![];
        let install = match v.mlx_install.as_str() {
            "package" => Span::styled("Omarchy package", Style::new().fg(Color::Green)),
            "both" => Span::styled("package + legacy leftovers (M to clean)", Style::new().fg(Color::Yellow)),
            "legacy" => Span::styled("legacy script install → press M to migrate", Style::new().fg(Color::Yellow).bold()),
            "none" => Span::styled("not installed → press M", Style::new().fg(Color::Red)),
            _ => Span::raw("…"),
        };
        rl.push(Line::from(vec![Span::styled("omarchy-mlx  ", Style::new().fg(Color::Cyan).bold()), install]));
        if let Some(p) = &v.mlx_pkg {
            rl.push(kv("  package", p));
        }
        if let Some(r) = &v.mlx_pkg_repo {
            let newer = v.mlx_pkg.as_ref().map(|p| p != r).unwrap_or(false);
            rl.push(Line::from(vec![
                Span::styled(format!("{:<12}", "  repo"), Style::new().fg(Color::DarkGray)),
                Span::raw(r.clone()),
                if newer { Span::styled("  ↑ update available (U)", Style::new().fg(Color::Yellow)) } else { Span::raw("") },
            ]));
        }
        if let Some(l) = &v.mlx_legacy {
            rl.push(kv("  legacy", l));
        }
        if let Some(u) = &v.mlx_upstream {
            rl.push(kv("  upstream", &format!("{u} (GitHub release)")));
        }
        rl.push(kv("  server", &self.cfg.mlx.command.join(" ")));
        rl.push(kv("  binary", v.mlx_bin.as_deref().unwrap_or("not found")));
        rl.push(kv("  ANE", if v.ane { "present (/dev/accel/accel0)" } else { "absent" }));
        rl.push(Line::raw(""));
        rl.push(Line::from(vec![
            Span::styled("llama.cpp    ", Style::new().fg(Color::Cyan).bold()),
            match &v.llama {
                Some(ver) => Span::styled(format!("build {ver}"), Style::new().fg(Color::Green)),
                None => Span::styled("not built → press B (Vulkan build)", Style::new().fg(Color::Yellow)),
            },
        ]));
        if let Some(p) = &v.llama_path {
            rl.push(kv("  binary", p));
        }
        rl.push(kv("  offload", &format!("ngl {} · ctx {} · fa {}", self.cfg.llama.gpu_layers, self.cfg.llama.ctx_size, self.cfg.llama.flash_attn)));
        rl.push(Line::raw(""));
        rl.push(Line::from(vec![Span::styled("Cache        ", Style::new().fg(Color::Cyan).bold()), Span::raw(format!("keep {} · max {} loaded", self.cfg.server.keep_alive, self.cfg.server.max_loaded))]));
        rl.push(kv("  MLX", &format!("prompt cache {} entries{}", self.cfg.mlx.prompt_cache_size, if self.cfg.mlx.prompt_cache_gib > 0.0 { format!(" · ≤{} GiB", self.cfg.mlx.prompt_cache_gib) } else { String::new() })));
        rl.push(kv("  GGUF", &format!("KV {}/{} · reuse {} · ctx {}", self.cfg.llama.cache_type_k, self.cfg.llama.cache_type_v, self.cfg.llama.cache_reuse, self.cfg.llama.ctx_size)));
        rl.push(Line::from(Span::styled("  edit in Settings (4) or the bar panel", Style::new().fg(Color::DarkGray))));
        rl.push(Line::raw(""));
        let n_mlx = self.models.iter().filter(|m| m.format == Format::Mlx).count();
        let disk: u64 = self.models.iter().map(|m| m.size).sum();
        rl.push(Line::from(vec![
            Span::styled("Library      ", Style::new().fg(Color::Cyan).bold()),
            Span::raw(format!("{} MLX · {} GGUF · {} on disk", n_mlx, self.models.len() - n_mlx, human(disk))),
        ]));
        f.render_widget(Paragraph::new(rl).wrap(Wrap { trim: false }).block(panel("Runtimes")), right);
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
        let narrow = area.width < 100;
        let running = self.running_names();
        let rows: Vec<Row> = self
            .models
            .iter()
            .map(|m| {
                let loaded = running.contains(&m.name);
                let fits = crate::compat::gpu_fit(m.size, self.cfg.llama.ctx_size as u64) != "too big";
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
                ])
            })
            .collect();
        let (widths, header): (Vec<Constraint>, Vec<&str>) = if narrow {
            (vec![Constraint::Length(1), Constraint::Min(12), Constraint::Length(5), Constraint::Length(6)], vec!["", "Name", "Fmt", "Size"])
        } else {
            (
                vec![Constraint::Length(1), Constraint::Min(30), Constraint::Length(5), Constraint::Length(11), Constraint::Length(8), Constraint::Length(7), Constraint::Length(6), Constraint::Length(22)],
                vec!["", "Name", "Fmt", "Quant", "Params", "Size", "Ctx", "Caps"],
            )
        };
        let t = Table::new(rows, widths)
        .header(Row::new(header).style(Style::new().fg(Color::DarkGray)))
        .row_highlight_style(Style::new().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
        .block(panel(&if narrow { format!("Local models ({})", self.models.len()) } else { format!("Local models ({})  ·  {}", self.models.len(), self.cfg.hf_cache().display()) }));
        f.render_stateful_widget(t, area, &mut self.models_sel);
        if self.models.is_empty() {
            let inner = area.inner(ratatui::layout::Margin { horizontal: 2, vertical: 2 });
            f.render_widget(Paragraph::new("No models yet. Go to Discover (3) to download MLX or GGUF models.").fg(Color::DarkGray), inner);
        }
    }

    fn draw_discover(&mut self, f: &mut Frame, area: Rect) {
        let [main, dl] = Layout::vertical([Constraint::Min(6), Constraint::Length(5)]).areas(area);
        let [list, detail] = Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)]).areas(main);
        let total_mem = sys::mem().0;

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
                        Cell::from(human(size)).fg(if size * 5 / 4 < total_mem { Color::Reset } else { Color::Red }),
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
                    let v = (!self.kind_gguf).then(|| crate::compat::assess_mlx(&self.cfg, &h.id, None));
                    let (mk, color) = match v.as_ref().map(|v| v.level) {
                        Some("unlikely") => ("✗ ", Color::Red),
                        Some("untested") => ("? ", Color::Yellow),
                        Some("qualified") => ("✓ ", Color::Green),
                        _ => ("  ", Color::Reset),
                    };
                    Row::new(vec![
                        Cell::from(Line::from(vec![Span::styled(mk, Style::new().fg(color)), Span::raw(h.id.clone())])),
                        Cell::from(compact(h.downloads)),
                        Cell::from(compact(h.likes)),
                        Cell::from(h.last_modified.get(..10).unwrap_or("").to_string()).fg(Color::DarkGray),
                    ])
                })
                .collect();
            let title = format!(
                "Hugging Face · {} · \"{}\"{}",
                if self.kind_gguf { "GGUF" } else { "MLX" },
                self.query,
                if self.searching { " · searching…" } else { "" }
            );
            let t = Table::new(rows, [Constraint::Min(30), Constraint::Length(7), Constraint::Length(6), Constraint::Length(10)])
                .header(Row::new(["Repo", "⬇", "♥", "Updated"]).style(Style::new().fg(Color::DarkGray)))
                .row_highlight_style(Style::new().bg(Color::DarkGray).bold())
                .block(panel(&title));
            f.render_stateful_widget(t, list, &mut self.hits_sel);
        }

        // Detail panel
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
                    Line::from("Press / to search, ⏎ to open a repo."),
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
                            let fit = crate::compat::gpu_fit(*size, self.cfg.llama.ctx_size as u64);
                            let color = match fit {
                                "fits" => Color::Green,
                                "tight" => Color::Yellow,
                                _ => Color::Red,
                            };
                            Row::new(vec![Cell::from(label.clone()), Cell::from(human(*size)), Cell::from(fit).fg(color)])
                        })
                        .collect();
                    let block = panel(&rv.repo);
                    let inner = block.inner(detail);
                    f.render_widget(block, detail);
                    let vh = if verdict.is_some() { 4 } else { 0 };
                    let [vrect, trect] = Layout::vertical([Constraint::Length(vh), Constraint::Min(2)]).areas(inner);
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
                    let t = Table::new(rows, [Constraint::Min(16), Constraint::Length(7), Constraint::Length(8)])
                        .header(Row::new(["Variant", "Size", "GPU mem"]).style(Style::new().fg(Color::DarkGray)))
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
        let mut display_sel = None;
        let mut section = "";
        for (i, (sec, k, help)) in SETTINGS.iter().enumerate() {
            if *sec != section {
                if !section.is_empty() {
                    rows.push(Row::new(vec![Cell::from("")]));
                }
                section = sec;
                rows.push(Row::new(vec![Cell::from(Span::styled(format!("▍{}", sec.to_uppercase()), Style::new().fg(Color::Magenta).bold()))]));
            }
            if self.settings_sel.selected() == Some(i) {
                display_sel = Some(rows.len());
            }
            let short = k.split_once('.').map(|(_, b)| b).unwrap_or(k);
            let mut cells = vec![Cell::from(format!("  {short}")).fg(Color::Cyan), Cell::from(get_setting(&self.cfg, k))];
            if !narrow {
                cells.push(Cell::from(*help).fg(Color::DarkGray));
            }
            rows.push(Row::new(cells));
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
