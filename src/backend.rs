//! Backend process manager: spawns mlx_lm.server / llama-server per model,
//! waits until it can actually generate, keeps it resident per keep_alive,
//! and evicts least-recently-used models when `max_loaded` is reached.

use crate::config::{log_dir, parse_duration, which, Config};
use crate::models::{self, Format, LocalModel};
use crate::sys;
use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};
use tokio::process::{Child, Command};

const MLX_SHIM: &str = include_str!("mlx_fast_server.py");

pub struct Running {
    pub name: String,
    pub model: LocalModel,
    pub port: u16,
    pub pid: u32,
    pub log: std::path::PathBuf,
    child: tokio::sync::Mutex<Child>,
    pub loaded_at: SystemTime,
    pub load_secs: f64,
    last_used: Mutex<Instant>,
    keep_alive: Mutex<Option<Duration>>,
    in_flight: AtomicUsize,
    stopping: AtomicBool,
}

impl Running {
    pub fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
    /// Model id the backend expects in request bodies.
    pub fn backend_model_id(&self) -> &str {
        match self.model.format {
            Format::Mlx => "default_model",
            Format::Gguf => &self.name,
        }
    }
    pub fn expires_in(&self) -> Option<Duration> {
        let ka = (*self.keep_alive.lock().unwrap())?;
        if self.in_flight.load(Ordering::Relaxed) > 0 {
            return Some(ka);
        }
        Some(ka.saturating_sub(self.last_used.lock().unwrap().elapsed()))
    }
}

/// Held for the duration of a proxied request; keeps the model from being evicted.
pub struct InFlight(pub Arc<Running>);
impl Drop for InFlight {
    fn drop(&mut self) {
        *self.0.last_used.lock().unwrap() = Instant::now();
        self.0.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}
fn enter(r: &Arc<Running>) -> InFlight {
    r.in_flight.fetch_add(1, Ordering::Relaxed);
    *r.last_used.lock().unwrap() = Instant::now();
    InFlight(r.clone())
}

#[derive(Serialize)]
pub struct RunningView {
    pub name: String,
    pub format: &'static str,
    pub port: u16,
    pub pid: u32,
    pub size: u64,
    pub rss: u64,
    pub in_flight: usize,
    pub loaded_secs: u64,
    pub load_secs: f64,
    pub expires_in_secs: Option<u64>,
    pub log: String,
}

pub struct Manager {
    pub cfg: RwLock<Config>,
    running: Mutex<Vec<Arc<Running>>>,
    load_lock: tokio::sync::Mutex<()>,
    cache: Mutex<Option<(Instant, Arc<Vec<LocalModel>>)>>,
    pub http: reqwest::Client,
}

impl Manager {
    pub fn new(cfg: Config) -> Arc<Self> {
        Arc::new(Self {
            cfg: RwLock::new(cfg),
            running: Mutex::new(Vec::new()),
            load_lock: tokio::sync::Mutex::new(()),
            cache: Mutex::new(None),
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .pool_idle_timeout(Duration::from_secs(30))
                .tcp_nodelay(true)
                .build()
                .unwrap(),
        })
    }

    pub fn cfg(&self) -> Config {
        self.cfg.read().unwrap().clone()
    }

    pub fn models(&self) -> Arc<Vec<LocalModel>> {
        let mut c = self.cache.lock().unwrap();
        if let Some((t, m)) = c.as_ref() {
            if t.elapsed() < Duration::from_secs(5) {
                return m.clone();
            }
        }
        let m = Arc::new(models::scan(&self.cfg()));
        *c = Some((Instant::now(), m.clone()));
        m
    }

    pub fn invalidate(&self) {
        *self.cache.lock().unwrap() = None;
    }

    pub fn resolve(&self, name: &str) -> Result<LocalModel> {
        let ms = self.models();
        let cfg = self.cfg();
        if let Some(m) = models::resolve(name, &ms, &cfg) {
            return Ok(m.clone());
        }
        self.invalidate();
        let ms = self.models();
        models::resolve(name, &ms, &cfg)
            .cloned()
            .ok_or_else(|| anyhow!("model '{name}' not found, try pulling it first"))
    }

    pub fn running(&self) -> Vec<Arc<Running>> {
        self.running.lock().unwrap().clone()
    }

    pub fn views(&self) -> Vec<RunningView> {
        self.running()
            .iter()
            .map(|r| RunningView {
                name: r.name.clone(),
                format: r.model.format.as_str(),
                port: r.port,
                pid: r.pid,
                size: r.model.size,
                rss: sys::tree_rss(r.pid),
                in_flight: r.in_flight.load(Ordering::Relaxed),
                loaded_secs: r.loaded_at.elapsed().unwrap_or_default().as_secs(),
                load_secs: r.load_secs,
                expires_in_secs: r.expires_in().map(|d| d.as_secs()),
                log: r.log.display().to_string(),
            })
            .collect()
    }

    fn find(&self, name: &str) -> Option<Arc<Running>> {
        self.running.lock().unwrap().iter().find(|r| r.name == name).cloned()
    }

    fn keep_alive(&self, m: &LocalModel, req: Option<&str>) -> Option<Duration> {
        let cfg = self.cfg();
        let s = req
            .map(str::to_string)
            .or_else(|| cfg.overrides.get(&m.name).and_then(|o| o.keep_alive.clone()))
            .unwrap_or(cfg.server.keep_alive);
        parse_duration(&s)
    }

    /// Ensure `name` is loaded and ready; returns an in-flight guard.
    pub async fn acquire(&self, name: &str, keep_alive: Option<&str>) -> Result<(InFlight, f64)> {
        let m = self.resolve(name)?;
        let ka = self.keep_alive(&m, keep_alive);
        if let Some(r) = self.find(&m.name) {
            if keep_alive.is_some() {
                *r.keep_alive.lock().unwrap() = ka;
            }
            return Ok((enter(&r), 0.0));
        }
        let _g = self.load_lock.lock().await;
        if let Some(r) = self.find(&m.name) {
            return Ok((enter(&r), 0.0));
        }
        let t0 = Instant::now();
        self.make_room(&m).await?;
        let r = self.spawn(&m, ka).await?;
        let guard = enter(&r);
        self.running.lock().unwrap().push(r);
        Ok((guard, t0.elapsed().as_secs_f64()))
    }

    async fn make_room(&self, m: &LocalModel) -> Result<()> {
        let cfg = self.cfg();
        let deadline = Instant::now() + Duration::from_secs(cfg.server.load_timeout_secs);
        loop {
            let running = self.running();
            let need_evict = running.len() >= cfg.server.max_loaded.max(1) || (cfg.server.mem_check && !self.fits(m) && !running.is_empty());
            if !need_evict {
                break;
            }
            let idle = running
                .iter()
                .filter(|r| r.in_flight.load(Ordering::Relaxed) == 0)
                .min_by_key(|r| *r.last_used.lock().unwrap());
            match idle {
                Some(r) => {
                    sys::log(&format!("evicting {} to make room for {}", r.name, m.name));
                    self.stop(r).await;
                }
                None => {
                    if Instant::now() > deadline {
                        bail!("all loaded models are busy; timed out waiting to load {}", m.name);
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
        if cfg.server.mem_check && !self.fits(m) {
            let (need, avail) = self.mem_need(m);
            bail!(
                "not enough memory for {}: needs ~{:.1} GiB, {:.1} GiB available (reserve {:.1} GiB). \
                 Unload other apps, pick a smaller quant, or disable server.mem_check",
                m.name,
                gib(need),
                gib(avail),
                cfg.server.mem_reserve_gib
            );
        }
        Ok(())
    }

    /// (estimated need, available after reserve) in bytes.
    pub fn mem_need(&self, m: &LocalModel) -> (u64, u64) {
        let cfg = self.cfg();
        let ctx = cfg.overrides.get(&m.name).and_then(|o| o.ctx_size).unwrap_or(cfg.llama.ctx_size) as u64;
        // weights + ~10% runtime overhead + rough KV/compute workspace scaled by context.
        let need = m.size + m.size / 10 + (ctx * 128 * 1024).min(8 << 30) + (512 << 20);
        let avail = sys::mem().1.saturating_sub((cfg.server.mem_reserve_gib * (1u64 << 30) as f64) as u64);
        (need, avail)
    }

    fn fits(&self, m: &LocalModel) -> bool {
        let (need, avail) = self.mem_need(m);
        need <= avail
    }

    async fn spawn(&self, m: &LocalModel, ka: Option<Duration>) -> Result<Arc<Running>> {
        let cfg = self.cfg();
        let ov = cfg.overrides.get(&m.name).cloned().unwrap_or_default();
        let port = free_port()?;
        let (mut argv, mut env): (Vec<String>, Vec<(String, String)>) = match m.format {
            Format::Mlx => {
                let mut a = cfg.mlx.command.clone();
                if cfg.mlx.fast_path && a.len() >= 2 && a[a.len() - 2..] == ["-m", "mlx_lm.server"] {
                    let shim = crate::config::data_dir().join("mlx_fast_server.py");
                    if std::fs::read_to_string(&shim).ok().as_deref() != Some(MLX_SHIM) {
                        std::fs::create_dir_all(crate::config::data_dir())?;
                        std::fs::write(&shim, MLX_SHIM)?;
                    }
                    a.truncate(a.len() - 2);
                    a.push(shim.display().to_string());
                }
                a.extend(["--model".into(), m.path.display().to_string(), "--host".into(), "127.0.0.1".into(), "--port".into(), port.to_string()]);
                a.extend(cfg.mlx.extra_args.clone());
                let mut env: Vec<(String, String)> = cfg.mlx.env.clone().into_iter().collect();
                env.push(("LMDECK_MLX_BATCH".into(), if cfg.mlx.batching { "1" } else { "0" }.into()));
                (a, env)
            }
            Format::Gguf => {
                let bin = cfg.llama_server().ok_or_else(|| {
                    anyhow!("llama-server not found: build it from the lmdeck Dashboard (key b) or `lmdeck llama build`")
                })?;
                let ctx = ov.ctx_size.unwrap_or(cfg.llama.ctx_size);
                let mut a = vec![
                    bin.display().to_string(),
                    "-m".into(),
                    m.path.display().to_string(),
                    "--host".into(),
                    "127.0.0.1".into(),
                    "--port".into(),
                    port.to_string(),
                    "-ngl".into(),
                    cfg.llama.gpu_layers.to_string(),
                    "-c".into(),
                    ctx.to_string(),
                    "-fa".into(),
                    cfg.llama.flash_attn.clone(),
                    "-np".into(),
                    cfg.llama.parallel.max(1).to_string(),
                    "--jinja".into(),
                    "--no-webui".into(),
                    "--alias".into(),
                    m.name.clone(),
                ];
                if cfg.llama.threads > 0 {
                    a.extend(["-t".into(), cfg.llama.threads.to_string()]);
                }
                if let Some(mm) = &m.mmproj {
                    a.extend(["--mmproj".into(), mm.display().to_string()]);
                }
                if m.capabilities().contains(&"embedding") {
                    a.push("--embeddings".into());
                }
                a.extend(cfg.llama.extra_args.clone());
                (a, cfg.llama.env.clone().into_iter().collect())
            }
        };
        argv.extend(ov.extra_args.clone());
        env.extend(ov.env.clone());
        if cfg.server.uclamp_boost {
            if let Some(u) = which("uclampset") {
                argv.splice(0..0, [u.display().to_string(), "-m".into(), "1024".into()]);
            }
        }

        if self.running().is_empty() && !cfg.hooks.on_load.is_empty() {
            sys::run_hook(&cfg.hooks.on_load).await;
        }

        std::fs::create_dir_all(log_dir())?;
        let log = log_dir().join(format!("{}.log", m.name.replace(['/', ':'], "_")));
        let lf = std::fs::OpenOptions::new().create(true).append(true).open(&log)?;
        use std::io::Write;
        let _ = writeln!(&lf, "\n===== {} lmdeck: {}", sys::now_rfc3339(), argv.join(" "));
        sys::log(&format!("loading {} ({}) on :{port}", m.name, m.format.as_str()));

        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .envs(env)
            .stdin(Stdio::null())
            .stdout(Stdio::from(lf.try_clone()?))
            .stderr(Stdio::from(lf))
            .process_group(0)
            .kill_on_drop(true);
        let child = cmd.spawn().with_context(|| format!("starting {}", argv[0]))?;
        let pid = child.id().unwrap_or(0);
        let t0 = Instant::now();
        let r = Arc::new(Running {
            name: m.name.clone(),
            model: m.clone(),
            port,
            pid,
            log: log.clone(),
            child: tokio::sync::Mutex::new(child),
            loaded_at: SystemTime::now(),
            load_secs: 0.0,
            last_used: Mutex::new(Instant::now()),
            keep_alive: Mutex::new(ka),
            in_flight: AtomicUsize::new(0),
            stopping: AtomicBool::new(false),
        });
        if let Err(e) = self.wait_ready(&r, Duration::from_secs(cfg.server.load_timeout_secs)).await {
            self.kill(&r).await;
            return Err(e.context(format!("{} failed to start (log: {})\n{}", m.name, log.display(), sys::tail(&log, 15))));
        }
        let secs = t0.elapsed().as_secs_f64();
        sys::log(&format!("{} ready in {secs:.1}s", m.name));
        // Arc::get_mut is fine: nobody else holds it yet.
        let mut r = r;
        if let Some(rm) = Arc::get_mut(&mut r) {
            rm.load_secs = secs;
        }
        Ok(r)
    }

    async fn wait_ready(&self, r: &Running, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let health = format!("{}/health", r.base());
        loop {
            if let Some(st) = r.child.lock().await.try_wait()? {
                bail!("backend exited with {st}");
            }
            if Instant::now() > deadline {
                bail!("timed out after {}s", timeout.as_secs());
            }
            if let Ok(resp) = self.http.get(&health).timeout(Duration::from_secs(2)).send().await {
                if resp.status().is_success() {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        if r.model.format == Format::Mlx {
            // mlx_lm.server answers /health before its generator thread has loaded the
            // weights. A 1-token request blocks until the model is loaded and also
            // compiles the Vulkan pipelines so the first real request is fast.
            let body = serde_json::json!({
                "model": "default_model",
                "messages": [{"role": "user", "content": "hi"}],
                "max_tokens": 1,
                "stream": false
            });
            let fut = self.http.post(format!("{}/v1/chat/completions", r.base())).json(&body).send();
            let mut fut = Box::pin(fut);
            loop {
                tokio::select! {
                    res = &mut fut => {
                        let res = res?;
                        if !res.status().is_success() {
                            bail!("warm-up failed: HTTP {} {}", res.status(), res.text().await.unwrap_or_default());
                        }
                        break;
                    }
                    _ = tokio::time::sleep(Duration::from_millis(500)) => {
                        if let Some(st) = r.child.lock().await.try_wait()? {
                            bail!("backend exited with {st} while loading");
                        }
                        if Instant::now() > deadline {
                            bail!("timed out loading weights after {}s", timeout.as_secs());
                        }
                    }
                }
            }
        }
        Ok(())
    }

    async fn kill(&self, r: &Running) {
        r.stopping.store(true, Ordering::Relaxed);
        let pgid = r.pid as i32;
        if pgid > 0 {
            unsafe { libc::kill(-pgid, libc::SIGTERM) };
        }
        let mut child = r.child.lock().await;
        if tokio::time::timeout(Duration::from_secs(8), child.wait()).await.is_err() {
            if pgid > 0 {
                unsafe { libc::kill(-pgid, libc::SIGKILL) };
            }
            let _ = child.kill().await;
        }
    }

    pub async fn stop(&self, r: &Arc<Running>) {
        self.running.lock().unwrap().retain(|x| !Arc::ptr_eq(x, r));
        self.kill(r).await;
        sys::log(&format!("unloaded {}", r.name));
        if self.running().is_empty() {
            let h = self.cfg().hooks.on_idle;
            if !h.is_empty() {
                sys::run_hook(&h).await;
            }
        }
    }

    pub async fn unload(&self, name: &str) -> Result<()> {
        let n = self.resolve(name).map(|m| m.name).unwrap_or_else(|_| name.to_string());
        match self.find(&n) {
            Some(r) => {
                self.stop(&r).await;
                Ok(())
            }
            None => bail!("{name} is not loaded"),
        }
    }

    pub async fn unload_all(&self) {
        for r in self.running() {
            self.stop(&r).await;
        }
    }

    /// Expire idle models and reap crashed backends.
    pub async fn reap(&self) {
        for r in self.running() {
            let exited = r.child.lock().await.try_wait().ok().flatten();
            if let Some(st) = exited {
                if !r.stopping.load(Ordering::Relaxed) {
                    sys::log(&format!("{} backend exited unexpectedly ({st}); see {}", r.name, r.log.display()));
                }
                self.running.lock().unwrap().retain(|x| !Arc::ptr_eq(x, &r));
                continue;
            }
            if r.in_flight.load(Ordering::Relaxed) == 0 && r.expires_in() == Some(Duration::ZERO) {
                self.stop(&r).await;
            }
        }
    }

    /// Apply a keep_alive to an already loaded model (Ollama `keep_alive: 0` = unload now).
    pub async fn set_keep_alive(&self, name: &str, ka: &str) -> Result<bool> {
        let m = self.resolve(name)?;
        if let Some(r) = self.find(&m.name) {
            let d = parse_duration(ka);
            if d == Some(Duration::ZERO) && r.in_flight.load(Ordering::Relaxed) == 0 {
                self.stop(&r).await;
                return Ok(true);
            }
            *r.keep_alive.lock().unwrap() = d;
            *r.last_used.lock().unwrap() = Instant::now();
        }
        Ok(false)
    }
}

fn free_port() -> Result<u16> {
    let l = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(l.local_addr()?.port())
}

pub fn gib(b: u64) -> f64 {
    b as f64 / (1u64 << 30) as f64
}
