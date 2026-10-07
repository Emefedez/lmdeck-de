//! Terminal UI. Event-driven: redraws only on input or background updates,
//! polls the daemon once per second. Uses ANSI palette colors so the
//! Omarchy theme applies.

use crate::client::{self, Daemon};
use crate::config::{log_dir, Config};
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
}

#[derive(Clone)]
enum Confirmed {
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
        catalog: load_catalog(),
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

/// The omarchy-mlx curated serving catalog (qualification status per model).
fn load_catalog() -> Vec<Value> {
    let mut cands = vec![
        crate::config::home().join(".local/share/mlx-omarchy/mlx_omarchy_serve/catalog.json"),
        crate::config::home().join(".local/share/mlx-omarchy/catalog.json"),
    ];
    for root in ["/usr/lib/omarchy-mlx", "/usr/share/omarchy-mlx"] {
        find_named(std::path::Path::new(root), "catalog.json", 7, &mut cands);
    }
    for c in cands {
        if !c.to_string_lossy().contains("serve") && !c.starts_with(crate::config::home()) {
            continue;
        }
        if let Ok(s) = std::fs::read_to_string(&c) {
            if let Ok(v) = serde_json::from_str::<Value>(&s) {
                if let Some(m) = v.get("models").and_then(|m| m.as_array()) {
                    return m.clone();
                }
            }
        }
    }
    vec![]
}

fn find_named(dir: &std::path::Path, name: &str, depth: u32, out: &mut Vec<std::path::PathBuf>) {
    if depth == 0 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            let n = e.file_name();
            let n = n.to_string_lossy();
            // Don't crawl all of site-packages; the serve package is what we want.
            let in_site = dir.file_name().map(|d| d == "site-packages").unwrap_or(false);
            if n == "__pycache__" || (in_site && !n.starts_with("mlx_omarchy")) {
                continue;
            }
            find_named(&p, name, depth - 1, out);
        } else if e.file_name() == name {
            out.push(p);
        }
    }
}

// ---------- settings table ----------

const SETTINGS: &[(&str, &str)] = &[
    ("server.host", "Listen address (127.0.0.1 keeps it local)"),
    ("server.port", "Port (11434 = Ollama's, so clients work unchanged)"),
    ("server.keep_alive", "Idle time before unloading: 5m, 1h, 0, -1 (forever)"),
    ("server.max_loaded", "Models resident at once"),
    ("server.load_timeout_secs", "Max seconds to load a model"),
    ("server.mem_check", "Refuse loads that would not fit in RAM (true/false)"),
    ("server.mem_reserve_gib", "RAM kept free for the desktop (GiB)"),
    ("server.uclamp_boost", "Run backends under uclampset -m 1024 (helps on power-saver profiles)"),
    ("models.hf_cache", "Hugging Face cache dir (empty = default)"),
    ("models.extra_dirs", "Extra dirs with loose .gguf files (comma separated)"),
    ("download.connections", "Parallel connections per large file"),
    ("download.parallel_files", "Files downloaded at once"),
    ("download.gguf_default_quant", "Quant used when a GGUF pull has no :tag"),
    ("download.gguf_mmproj", "Also fetch vision projector (mmproj) for GGUF"),
    ("mlx.command", "Command that starts the MLX server"),
    ("mlx.fast_path", "Patch out mlx_lm.server's per-token GPU sync (~2x decode)"),
    ("mlx.batching", "Batched generator: only for concurrent requests (slower per stream)"),
    ("mlx.extra_args", "Extra mlx_lm.server args (e.g. --prompt-cache-size 4)"),
    ("mlx.env", "Env for MLX backend, K=V,K=V (e.g. MLX_OMARCHY_GDN_RAW_REPEAT=0)"),
    ("llama.server_path", "llama-server binary (empty = lmdeck build, then PATH)"),
    ("llama.gpu_layers", "Layers offloaded to the GPU (999 = all)"),
    ("llama.ctx_size", "Context window for GGUF models"),
    ("llama.flash_attn", "Flash attention: on / off / auto"),
    ("llama.threads", "CPU threads (0 = auto)"),
    ("llama.parallel", "Parallel request slots"),
    ("llama.extra_args", "Extra llama-server args (e.g. --cache-type-k q8_0)"),
    ("llama.env", "Env for llama backend, K=V,K=V"),
    ("llama.git_ref", "llama.cpp branch/tag to build"),
    ("hooks.on_load", "Shell cmd before first model loads (e.g. tuned-adm profile asahi-performance)"),
    ("hooks.on_idle", "Shell cmd after last model unloads"),
];

fn kv_join(m: &std::collections::BTreeMap<String, String>) -> String {
    m.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(",")
}
fn kv_parse(s: &str) -> std::collections::BTreeMap<String, String> {
    s.split(',').filter_map(|p| p.split_once('=')).map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).collect()
}
fn words(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_string).collect()
}

fn get_setting(c: &Config, key: &str) -> String {
    match key {
        "server.host" => c.server.host.clone(),
        "server.port" => c.server.port.to_string(),
        "server.keep_alive" => c.server.keep_alive.clone(),
        "server.max_loaded" => c.server.max_loaded.to_string(),
        "server.load_timeout_secs" => c.server.load_timeout_secs.to_string(),
        "server.mem_check" => c.server.mem_check.to_string(),
        "server.mem_reserve_gib" => c.server.mem_reserve_gib.to_string(),
        "server.uclamp_boost" => c.server.uclamp_boost.to_string(),
        "models.hf_cache" => c.models.hf_cache.clone(),
        "models.extra_dirs" => c.models.extra_dirs.join(","),
        "download.connections" => c.download.connections.to_string(),
        "download.parallel_files" => c.download.parallel_files.to_string(),
        "download.gguf_default_quant" => c.download.gguf_default_quant.clone(),
        "download.gguf_mmproj" => c.download.gguf_mmproj.to_string(),
        "mlx.command" => c.mlx.command.join(" "),
        "mlx.fast_path" => c.mlx.fast_path.to_string(),
        "mlx.batching" => c.mlx.batching.to_string(),
        "mlx.extra_args" => c.mlx.extra_args.join(" "),
        "mlx.env" => kv_join(&c.mlx.env),
        "llama.server_path" => c.llama.server_path.clone(),
        "llama.gpu_layers" => c.llama.gpu_layers.to_string(),
        "llama.ctx_size" => c.llama.ctx_size.to_string(),
        "llama.flash_attn" => c.llama.flash_attn.clone(),
        "llama.threads" => c.llama.threads.to_string(),
        "llama.parallel" => c.llama.parallel.to_string(),
        "llama.extra_args" => c.llama.extra_args.join(" "),
        "llama.env" => kv_join(&c.llama.env),
        "llama.git_ref" => c.llama.git_ref.clone(),
        "hooks.on_load" => c.hooks.on_load.clone(),
        "hooks.on_idle" => c.hooks.on_idle.clone(),
        _ => String::new(),
    }
}

fn set_setting(c: &mut Config, key: &str, v: &str) -> Result<(), String> {
    let v = v.trim();
    let num = |s: &str| s.parse::<u64>().map_err(|_| format!("'{s}' is not a number"));
    let boolean = |s: &str| match s {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" => Ok(false),
        _ => Err(format!("'{s}' is not true/false")),
    };
    match key {
        "server.host" => c.server.host = v.into(),
        "server.port" => c.server.port = num(v)? as u16,
        "server.keep_alive" => c.server.keep_alive = v.into(),
        "server.max_loaded" => c.server.max_loaded = num(v)?.max(1) as usize,
        "server.load_timeout_secs" => c.server.load_timeout_secs = num(v)?,
        "server.mem_check" => c.server.mem_check = boolean(v)?,
        "server.mem_reserve_gib" => c.server.mem_reserve_gib = v.parse().map_err(|_| "not a number".to_string())?,
        "server.uclamp_boost" => c.server.uclamp_boost = boolean(v)?,
        "models.hf_cache" => c.models.hf_cache = v.into(),
        "models.extra_dirs" => c.models.extra_dirs = v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
        "download.connections" => c.download.connections = num(v)?.clamp(1, 32) as usize,
        "download.parallel_files" => c.download.parallel_files = num(v)?.clamp(1, 16) as usize,
        "download.gguf_default_quant" => c.download.gguf_default_quant = v.into(),
        "download.gguf_mmproj" => c.download.gguf_mmproj = boolean(v)?,
        "mlx.command" => c.mlx.command = words(v),
        "mlx.fast_path" => c.mlx.fast_path = boolean(v)?,
        "mlx.batching" => c.mlx.batching = boolean(v)?,
        "mlx.extra_args" => c.mlx.extra_args = words(v),
        "mlx.env" => c.mlx.env = kv_parse(v),
        "llama.server_path" => c.llama.server_path = v.into(),
        "llama.gpu_layers" => c.llama.gpu_layers = v.parse().map_err(|_| "not a number".to_string())?,
        "llama.ctx_size" => c.llama.ctx_size = num(v)? as u32,
        "llama.flash_attn" => {
            if !["on", "off", "auto"].contains(&v) {
                return Err("use on, off or auto".into());
            }
            c.llama.flash_attn = v.into()
        }
        "llama.threads" => c.llama.threads = num(v)? as u32,
        "llama.parallel" => c.llama.parallel = num(v)?.max(1) as u32,
        "llama.extra_args" => c.llama.extra_args = words(v),
        "llama.env" => c.llama.env = kv_parse(v),
        "llama.git_ref" => c.llama.git_ref = v.into(),
        "hooks.on_load" => c.hooks.on_load = v.into(),
        "hooks.on_idle" => c.hooks.on_idle = v.into(),
        _ => return Err("unknown setting".into()),
    }
    Ok(())
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
            Msg::Models => self.rescan(),
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
                        let key = SETTINGS[i].0;
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
                    let name = m.name.clone();
                    self.spawn_daemon_op(format!("loading {name}…"), move |d| async move {
                        let v = d.load(&name).await?;
                        Ok(format!("{name} ready ({:.1}s)", v["load_secs"].as_f64().unwrap_or(0.0)))
                    });
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
        let mut out = Vec::new();
        let has_st = files.iter().any(|f| f.path.ends_with(".safetensors")) && files.iter().any(|f| f.path == "config.json");
        if has_st {
            let size: u64 = files.iter().filter(|f| !f.path.ends_with(".gguf")).map(|f| f.size).sum();
            out.push((rv.repo.clone(), "MLX / safetensors (whole repo)".into(), size));
        }
        for (q, s) in hf::gguf_quants(files) {
            let star = if q.eq_ignore_ascii_case(&self.cfg.download.gguf_default_quant) { " ★" } else { "" };
            out.push((format!("{}:{q}", rv.repo), format!("GGUF {q}{star}"), s));
        }
        out
    }

    fn key_discover(&mut self, k: KeyEvent) -> Option<Action> {
        if self.repo.is_some() {
            let choices = self.repo_choices();
            let n = choices.len();
            let sel = &mut self.repo.as_mut().unwrap().sel;
            match k.code {
                KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') => self.repo = None,
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
                self.buf = get_setting(&self.cfg, SETTINGS[i].0);
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
        let titles: Vec<Line> = TABS.iter().map(|t| Line::from(t.title())).collect();
        let idx = TABS.iter().position(|t| *t == self.tab).unwrap();
        let up = self.status.is_some();
        let [tabs_area, badge] = Layout::horizontal([Constraint::Min(10), Constraint::Length(34)]).areas(top);
        f.render_widget(
            Tabs::new(titles).select(idx).highlight_style(Style::new().fg(Color::Black).bg(Color::Cyan).bold()).divider(" "),
            tabs_area,
        );
        f.render_widget(
            Line::from(vec![
                Span::raw("lmdeck "),
                if up { Span::styled("● serving ", Style::new().fg(Color::Green)) } else { Span::styled("○ stopped ", Style::new().fg(Color::DarkGray)) },
                Span::styled(format!(":{}", self.cfg.server.port), Style::new().fg(Color::DarkGray)),
            ])
            .right_aligned(),
            badge,
        );
        match self.tab {
            Tab::Dashboard => self.draw_dashboard(f, body),
            Tab::Models => self.draw_models(f, body),
            Tab::Discover => self.draw_discover(f, body),
            Tab::Settings => self.draw_settings(f, body),
            Tab::Logs => self.draw_logs(f, body),
        }
        let help = match self.tab {
            Tab::Dashboard => "s start/stop · S service · a unload all · M migrate · U update MLX · B build llama.cpp · D launcher · C clear downloads · r recheck",
            Tab::Models => "⏎ load · u unload · c chat · x context · d delete · y copy name · r rescan",
            Tab::Discover => {
                if self.repo.is_some() {
                    "⏎ download · esc back"
                } else {
                    "/ search · g MLX⇄GGUF · c catalog⇄search · ⏎ open · x cancel download"
                }
            }
            Tab::Settings => "⏎ edit · o open config in $EDITOR",
            Tab::Logs => "m daemon⇄model log · j/k scroll · G end",
        };
        let foot_line = match &self.toast {
            Some((t, _)) => Line::from(Span::styled(t.clone(), Style::new().fg(Color::Yellow))),
            None => Line::from(vec![Span::styled(help, Style::new().fg(Color::DarkGray)), Span::styled("  · 1-5 tabs · q quit", Style::new().fg(Color::DarkGray))]),
        };
        f.render_widget(foot_line, foot);
        self.draw_input(f);
    }

    fn draw_input(&self, f: &mut Frame) {
        let (title, text) = match &self.input {
            Input::None => return,
            Input::Search => (format!("Search Hugging Face ({})", if self.kind_gguf { "GGUF" } else { "MLX" }), format!("{}▏", self.buf)),
            Input::Setting(i) => (format!("{} — {}", SETTINGS[*i].0, SETTINGS[*i].1), format!("{}▏", self.buf)),
            Input::Override(n) => (format!("Context size for {n} (empty = default)"), format!("{}▏", self.buf)),
            Input::Confirm(q, _) => ("Confirm".into(), q.clone()),
        };
        let a = f.area();
        let w = a.width.saturating_sub(8).min(100);
        let r = Rect { x: a.x + (a.width - w) / 2, y: a.y + a.height / 3, width: w, height: 3 };
        f.render_widget(Clear, r);
        f.render_widget(
            Paragraph::new(text).block(Block::bordered().border_type(BorderType::Rounded).title(title).border_style(Style::new().fg(Color::Cyan))),
            r,
        );
    }

    fn draw_dashboard(&self, f: &mut Frame, area: Rect) {
        let [left, right] = Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(area);
        let [srv, loaded, dls] = Layout::vertical([Constraint::Length(9), Constraint::Min(4), Constraint::Length(8)]).areas(left);

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
                    Cell::from(format!("{:.1}s", r["load_secs"].as_f64().unwrap_or(0.0))),
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
        let n_mlx = self.models.iter().filter(|m| m.format == Format::Mlx).count();
        let disk: u64 = self.models.iter().map(|m| m.size).sum();
        rl.push(Line::from(vec![
            Span::styled("Library      ", Style::new().fg(Color::Cyan).bold()),
            Span::raw(format!("{} MLX · {} GGUF · {} on disk", n_mlx, self.models.len() - n_mlx, human(disk))),
        ]));
        f.render_widget(Paragraph::new(rl).wrap(Wrap { trim: false }).block(panel("Runtimes")), right);
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
        let running = self.running_names();
        let rows: Vec<Row> = self
            .models
            .iter()
            .map(|m| {
                let loaded = running.contains(&m.name);
                let fits = m.size + m.size / 10 < sys::mem().0;
                let caps: Vec<&str> = m.capabilities().into_iter().filter(|c| *c != "completion").collect();
                Row::new(vec![
                    Cell::from(if loaded { "●" } else { " " }).fg(Color::Green),
                    Cell::from(m.name.clone()),
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
        let t = Table::new(
            rows,
            [Constraint::Length(1), Constraint::Min(30), Constraint::Length(5), Constraint::Length(11), Constraint::Length(8), Constraint::Length(7), Constraint::Length(6), Constraint::Length(22)],
        )
        .header(Row::new(["", "Name", "Fmt", "Quant", "Params", "Size", "Ctx", "Caps"]).style(Style::new().fg(Color::DarkGray)))
        .row_highlight_style(Style::new().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
        .block(panel(&format!("Local models ({})  ·  {}", self.models.len(), self.cfg.hf_cache().display())));
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
                    Row::new(vec![
                        Cell::from(h.id.clone()),
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
        let total = sys::mem();
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
                            let need = size + size / 10;
                            let (fit, color) = if need + (2u64 << 30) < total.1 {
                                ("fits", Color::Green)
                            } else if need < total.0 {
                                ("tight", Color::Yellow)
                            } else {
                                ("too big", Color::Red)
                            };
                            Row::new(vec![Cell::from(label.clone()), Cell::from(human(*size)), Cell::from(fit).fg(color)])
                        })
                        .collect();
                    let t = Table::new(rows, [Constraint::Min(16), Constraint::Length(7), Constraint::Length(8)])
                        .header(Row::new(["Variant", "Size", "RAM"]).style(Style::new().fg(Color::DarkGray)))
                        .row_highlight_style(Style::new().bg(Color::DarkGray).bold())
                        .block(panel(&rv.repo));
                    f.render_stateful_widget(t, detail, &mut rv.sel);
                }
            }
        }
        self.draw_downloads(f, dl);
    }

    fn draw_settings(&mut self, f: &mut Frame, area: Rect) {
        let rows: Vec<Row> = SETTINGS
            .iter()
            .map(|(k, help)| Row::new(vec![Cell::from(*k).fg(Color::Cyan), Cell::from(get_setting(&self.cfg, k)), Cell::from(*help).fg(Color::DarkGray)]))
            .collect();
        let t = Table::new(rows, [Constraint::Length(28), Constraint::Percentage(35), Constraint::Min(20)])
            .row_highlight_style(Style::new().bg(Color::DarkGray).bold())
            .block(panel(&format!("Settings · {}", crate::config::config_path().display())));
        f.render_stateful_widget(t, area, &mut self.settings_sel);
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
