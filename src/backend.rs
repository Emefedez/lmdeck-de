//! Backend process manager: spawns mlx_lm.server / llama-server per model,
//! waits until it can actually generate, keeps it resident per keep_alive,
//! and evicts least-recently-used models when `max_loaded` is reached.

use crate::config::{log_dir, parse_duration, which, Config};
use crate::models::{self, Format, LocalModel};
use crate::sys;
use crate::usage::EVENTS;
use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};
use tokio::process::{Child, Command};

const MLX_SHIM: &str = include_str!("mlx_fast_server.py");

/// Book-keeping for the load in progress (for progress bars).
#[derive(Clone)]
struct LoadInfo {
    model: String,
    size: u64,
    shmem0: u64,
    started: Instant,
    phase: &'static str,
    log: Option<std::path::PathBuf>,
    /// Log length when this load began; warm-up progress is parsed after it.
    log_offset: u64,
}

pub struct Running {
    pub name: String,
    pub model: LocalModel,
    pub port: u16,
    pub pid: u32,
    pub log: std::path::PathBuf,
    /// Backend-relevant config at spawn; differs from the current one => stale.
    pub sig: String,
    child: tokio::sync::Mutex<Child>,
    pub loaded_at: SystemTime,
    pub load_secs: f64,
    /// Launcher notes for this load (e.g. "8-bit KV not supported; using f16").
    pub note: String,
    last_used: Mutex<Instant>,
    keep_alive: Mutex<Option<Duration>>,
    in_flight: AtomicUsize,
    stopping: AtomicBool,
    /// Backend log size and time when the current burst of requests began (for progress).
    work_start: Mutex<Option<(u64, Instant)>>,
}

impl Running {
    pub fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
    /// Model id the backend expects in request bodies.
    pub fn backend_model_id(&self) -> &str {
        match self.model.format {
            Format::Mlx => "default_model",
            Format::Gguf | Format::Strata => &self.name,
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
        EVENTS.bump();
    }
}
fn enter(r: &Arc<Running>) -> InFlight {
    EVENTS.bump();
    if r.in_flight.fetch_add(1, Ordering::Relaxed) == 0 {
        let len = std::fs::metadata(&r.log).map(|m| m.len()).unwrap_or(0);
        *r.work_start.lock().unwrap() = Some((len, Instant::now()));
    }
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
    /// Settings changed since load; reload the model to apply them.
    pub stale: bool,
    pub note: String,
    pub device: String,
    /// Progress of the request(s) in flight: phase, prompt tokens read/total, rate, ETA.
    pub work: Option<serde_json::Value>,
}

/// Seconds of day from a "YYYY-MM-DD HH:MM:SS,mmm" log prefix.
fn log_secs(line: &str) -> Option<f64> {
    let t = line.get(11..23)?;
    let (hms, ms) = t.split_once(',')?;
    let mut it = hms.split(':').map(|x| x.parse::<f64>().ok());
    Some(it.next()?? * 3600.0 + it.next()?? * 60.0 + it.next()?? + ms.parse::<f64>().ok()? / 1000.0)
}

/// Progress of in-flight work from the backend log written since `offset`.
fn work_progress(log: &std::path::Path, offset: u64, started: Instant) -> serde_json::Value {
    let text = sys::read_since(log, offset, 64 * 1024);
    // (secs of day, done, total); mlx_lm: "Prompt processing progress: a/b",
    // llama.cpp: "... n_tokens = a, ... progress = f".
    let mut pts: Vec<(Option<f64>, u64, u64)> = Vec::new();
    for l in text.lines() {
        if let Some(r) = l.split("Prompt processing progress:").nth(1) {
            if let Some((a, b)) = r.trim().split_once('/') {
                if let (Ok(a), Ok(b)) = (a.trim().parse(), b.trim().parse()) {
                    pts.push((log_secs(l), a, b));
                }
            }
        } else if l.contains("prompt processing progress") {
            let num = |k: &str| l.split(k).nth(1).and_then(|r| r.split(|c: char| c == ',' || c.is_whitespace()).find(|s| !s.is_empty())).and_then(|s| s.parse::<f64>().ok());
            if let (Some(n), Some(p)) = (num("n_tokens ="), num("progress =")) {
                if p > 0.0 {
                    pts.push((None, n as u64, (n / p).round() as u64));
                }
            }
        }
    }
    let elapsed = started.elapsed().as_secs_f64();
    let Some(&(t1, done, total)) = pts.last() else {
        return serde_json::json!({"phase": "waiting", "elapsed_secs": elapsed});
    };
    if done >= total {
        return serde_json::json!({"phase": "generating", "prompt_tokens": total, "elapsed_secs": elapsed});
    }
    // Rate from the first and last progress lines of this prompt.
    let first = pts.iter().rev().take_while(|p| p.2 == total).last().copied();
    let rate = match (first, t1) {
        (Some((Some(t0), d0, _)), Some(t1)) if t1 > t0 && done > d0 => Some((done - d0) as f64 / (t1 - t0)),
        _ => None,
    };
    serde_json::json!({
        "phase": "reading prompt",
        "prompt_done": done,
        "prompt_tokens": total,
        "tokens_per_sec": rate,
        "eta_secs": rate.map(|r| (total - done) as f64 / r),
        "elapsed_secs": elapsed,
    })
}

pub struct Manager {
    pub cfg: RwLock<Config>,
    running: Mutex<Vec<Arc<Running>>>,
    loading: Mutex<Option<String>>,
    load_info: Mutex<Option<LoadInfo>>,
    /// PIDs of backends we killed that had not exited yet.
    exiting: Mutex<Vec<u32>>,
    load_lock: tokio::sync::Mutex<()>,
    cache: Mutex<Option<(Instant, Arc<Vec<LocalModel>>)>>,
    pub http: reqwest::Client,
}

impl Manager {
    pub fn new(cfg: Config) -> Arc<Self> {
        Arc::new(Self {
            cfg: RwLock::new(cfg),
            running: Mutex::new(Vec::new()),
            loading: Mutex::new(None),
            load_info: Mutex::new(None),
            exiting: Mutex::new(Vec::new()),
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
                stale: r.sig != backend_sig(&self.cfg(), &r.model),
                note: r.note.clone(),
                device: self.cfg().overrides.get(&r.name).and_then(|o| o.device.clone()).unwrap_or_else(|| "gpu".into()),
                work: (r.in_flight.load(Ordering::Relaxed) > 0)
                    .then(|| *r.work_start.lock().unwrap())
                    .flatten()
                    .map(|(off, t)| work_progress(&r.log, off, t)),
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
        self.loading.lock().unwrap().replace(m.name.clone());
        *self.load_info.lock().unwrap() = Some(LoadInfo {
            model: m.name.clone(),
            size: m.size,
            shmem0: sys::shmem(),
            started: Instant::now(),
            phase: "preparing",
            log: None,
            log_offset: 0,
        });
        EVENTS.bump();
        let res = self.load_locked(&m, ka).await;
        self.loading.lock().unwrap().take();
        self.load_info.lock().unwrap().take();
        EVENTS.bump();
        let r = res?;
        let guard = enter(&r);
        self.running.lock().unwrap().push(r);
        Ok((guard, t0.elapsed().as_secs_f64()))
    }

    async fn load_locked(&self, m: &LocalModel, ka: Option<Duration>) -> Result<Arc<Running>> {
        self.make_room(m).await?;
        // After eviction, so freed memory counts toward the heap we may raise.
        let heap = self.heap_plan(m)?;
        self.spawn(m, ka, heap).await
    }

    /// Model currently being loaded, if any.
    pub fn loading(&self) -> Option<String> {
        self.loading.lock().unwrap().clone()
    }

    /// Unload and load again (picks up changed settings), keeping keep_alive.
    pub async fn restart(&self, name: &str) -> Result<f64> {
        let m = self.resolve(name)?;
        let ka = self.find(&m.name).map(|r| *r.keep_alive.lock().unwrap());
        if let Some(r) = self.find(&m.name) {
            self.stop(&r).await;
        }
        let ka_s = match ka {
            Some(Some(d)) => Some(format!("{}s", d.as_secs())),
            Some(None) => Some("-1".to_string()),
            None => None,
        };
        let (_g, secs) = self.acquire(&m.name, ka_s.as_deref()).await?;
        Ok(secs)
    }

    /// " The selected MLX runtime rounds GPU buffers up …: +X GiB" when that matters for `m`.
    fn binning_note(&self, m: &LocalModel) -> String {
        let cfg = self.cfg();
        let Some(rounding) = cfg.mlx.command.first().map(|c| crate::compat::runtime_buffer_rounding(c)) else { return String::new() };
        let Some(what) = rounding.describe().filter(|_| m.format == Format::Mlx) else { return String::new() };
        match (crate::compat::mlx_weight_bytes(&m.path, rounding), crate::compat::mlx_weight_bytes(&m.path, crate::compat::BufferRounding::Pages)) {
            // Below ~0.25 GiB it is noise next to the planner's allowances.
            (Some(b), Some(p)) if b > p + (256 << 20) => format!(
                " The selected MLX runtime {what}: +{:.1} GiB for this model; the Omarchy package runtime does not (TUI: i → 1).",
                gib(b - p)
            ),
            _ => String::new(),
        }
    }

    fn on_gpu(&self, m: &LocalModel) -> bool {
        self.cfg().overrides.get(&m.name).and_then(|o| o.device.as_deref()) != Some("cpu")
    }

    /// Estimated GPU bytes for `m` (weights + runtime + KV at its context).
    pub fn gpu_need(&self, m: &LocalModel) -> u64 {
        self.gpu_need_ctx(m, self.plan_ctx(m))
    }

    /// The context the planner charges KV for. MLX serves with a prompt cache that grows
    /// into the heap ceiling rather than being pre-allocated, so this is the overhead the
    /// admission guard reserves up front: a per-model `ctx_size` override when set (clamped
    /// to the model's own maximum), else the default plan context.
    fn plan_ctx(&self, m: &LocalModel) -> u64 {
        if m.format != Format::Mlx {
            return MLX_PLAN_CTX;
        }
        match self.cfg().overrides.get(&m.name).and_then(|o| o.ctx_size) {
            Some(c) if c > 0 => {
                let want = (c as u64).max(MLX_MIN_CTX);
                crate::compat::mlx_model_max_ctx(&m.path).map_or(want, |max| want.min(max))
            }
            _ => MLX_PLAN_CTX,
        }
    }

    /// MLX: exact weights + attention cache for `mlx_ctx` tokens (from config.json) + runtime and
    /// prompt-cache allowance. Falls back to the generic estimate when the config is unknown.
    fn gpu_need_ctx(&self, m: &LocalModel, mlx_ctx: u64) -> u64 {
        let cfg = self.cfg();
        match m.format {
            Format::Gguf => {
                let ctx = cfg.overrides.get(&m.name).and_then(|o| o.ctx_size).unwrap_or(cfg.llama.ctx_size) as u64;
                crate::compat::gpu_need(m.size, ctx)
            }
            // The experts stream from the SSD into an arena the engine sizes itself from what is
            // free, so admission only needs the floor: the dense weights, buffers and a small arena.
            Format::Strata => if cfg.strata.engine == "mock" { 0 } else { STRATA_FLOOR },
            Format::Mlx => {
                let rounding = cfg.mlx.command.first().map_or(crate::compat::BufferRounding::Pages, |c| crate::compat::runtime_buffer_rounding(c));
                let weights = crate::compat::mlx_weight_bytes(&m.path, rounding).unwrap_or(m.size);
                match crate::compat::mlx_kv_per_token(&m.path) {
                    Some(per_tok) => weights + per_tok * mlx_ctx + MLX_RUNTIME + MLX_CACHE_ALLOWANCE,
                    None => crate::compat::gpu_need(weights, 8192),
                }
            }
        }
    }

    /// Decide the GPU heap (HK_SYSMEM) for loading `m`. Ok(None) = stock heap.
    /// Per process only: nothing global changes, and it ends with the backend.
    fn heap_plan(&self, m: &LocalModel) -> Result<Option<u64>> {
        let cfg = self.cfg();
        if !self.on_gpu(m) || (m.format == Format::Strata && cfg.strata.engine == "mock") {
            return Ok(None);
        }
        partial_layers().lock().unwrap().remove(&m.name);
        let need = self.gpu_need(m);
        let stock = crate::compat::gpu_budget();
        let gi = |b: u64| gib(b);
        let reserve = (cfg.server.mem_reserve_gib * (1u64 << 30) as f64) as u64
            + if crate::sys::muvm_rss().is_some() { (cfg.server.vm_reserve_gib.max(0.0) * (1u64 << 30) as f64) as u64 } else { 0 };
        let (total, avail) = sys::mem();
        let policy = cfg.server.gpu_heap.trim().to_lowercase();
        let heap = match policy.as_str() {
            "stock" => stock,
            "auto" | "" => {
                // Strata always gets the safe free memory: whatever the arena cannot hold is read
                // from the SSD on every token, so a bigger arena is directly faster.
                if need <= stock * 92 / 100 && m.format != Format::Strata {
                    return Ok(None);
                }
                // Room the model can really take: what is free now (already net of
                // muvm/FEX/desktop) minus the reserve, never above MemTotal - reserve.
                let cap = avail.saturating_sub(reserve).min(total.saturating_sub(reserve));
                // MLX needs are exact (weights from the files, KV from the config): a fixed margin.
                // GGUF keeps the proportional one.
                let margin = |n: u64| if m.format != Format::Gguf { 512 << 20 } else { n / 12 + (512 << 20) };
                let mut need = need;
                let mut want = need + margin(need);
                if want > cap && m.format == Format::Mlx {
                    let short = self.gpu_need_ctx(m, MLX_MIN_CTX);
                    if short + margin(short) <= cap {
                        sys::log(&format!("{}: planning for {}k context (not {}k) to fit free memory", m.name, MLX_MIN_CTX / 1024, MLX_PLAN_CTX / 1024));
                        need = short;
                        want = short + margin(short);
                    }
                }
                if want > cap && m.format == Format::Gguf && cfg.llama.auto_offload {
                    // Put only as many layers on the GPU as fit; the rest run on the CPU from
                    // the memory-mapped file (reclaimable pages, so a tight machine slows down
                    // instead of freezing).
                    if let Some(blocks) = gguf_block_count(&m.path).filter(|b| *b > 0) {
                        let usable = cap.saturating_sub(512 << 20) as f64;
                        let ngl = ((blocks as f64 * usable / need as f64).floor() as i32).clamp(0, blocks as i32);
                        if ngl >= 1 {
                            partial_layers().lock().unwrap().insert(m.name.clone(), ngl);
                            sys::log(&format!("{}: {ngl}/{blocks} layers on the GPU (needs ~{:.1} GiB, {:.1} GiB available)", m.name, gi(need), gi(cap)));
                            return Ok(Some((cap & !((1u64 << 20) - 1)).max(stock)).filter(|h| *h != stock));
                        }
                    }
                }
                if want > cap {
                    bail!(
                        "{} needs ~{:.1} GiB of GPU memory, but only {:.1} GiB can be given to it right now \
                         ({:.1} GiB free, {:.1} GiB kept for the desktop{}).{} Close apps or pick a smaller model{}",
                        m.name,
                        gi(need),
                        gi(cap),
                        gi(avail),
                        gi(reserve),
                        if crate::sys::muvm_rss().is_some() { " and a running muvm VM" } else { "" },
                        self.binning_note(m),
                        crate::compat::recommend_fitting("chat").map(|r| format!(" (e.g. {r})")).unwrap_or_default()
                    );
                }
                // The heap is a ceiling, not an allocation: give the backend everything that is
                // safe (free RAM minus the reserves) so long prompts have room for their KV cache.
                cap.max(want).max(stock) & !((1u64 << 20) - 1)
            }
            n => (n.parse::<f64>().unwrap_or(0.0) * (1u64 << 30) as f64) as u64,
        };
        // "auto" already sized the heap with its own margin (need + 512 MiB within the safe free
        // RAM); a second 8% here refused a 17.2 GiB model with 18.2 GiB available.
        let auto = matches!(policy.as_str(), "auto" | "");
        if cfg.server.gpu_mem_check && !auto && need > heap * 92 / 100 {
            bail!(
                "{} is too big for the GPU heap: needs ~{:.1} GiB, heap is {:.1} GiB (server.gpu_heap = {}). \
                 Overcommitting the GPU can hang the whole machine, so lmdeck refuses. Set server.gpu_heap to auto, or pick a smaller model{}",
                m.name,
                gi(need),
                gi(heap),
                cfg.server.gpu_heap,
                crate::compat::recommend_fitting("chat").map(|r| format!(" (e.g. {r})")).unwrap_or_default()
            );
        }
        Ok((heap != stock).then_some(heap))
    }

    /// Dry run of the admission decision for `name` (nothing is loaded).
    pub fn plan_preview(&self, name: &str) -> serde_json::Value {
        let m = match self.resolve(name) {
            Ok(m) => m,
            Err(e) => return serde_json::json!({"error": format!("{e:#}")}),
        };
        let (need_ram, avail_ram) = self.mem_need(&m);
        let plan = self.heap_plan(&m);
        serde_json::json!({
            "model": m.name,
            "device": if self.on_gpu(&m) { "gpu" } else { "cpu" },
            "gpu_need": self.gpu_need(&m),
            "stock_heap": crate::compat::gpu_budget(),
            "heap": plan.as_ref().ok().cloned().flatten(),
            "ram_need": need_ram,
            "ram_available": avail_ram,
            "muvm_rss": sys::muvm_rss(),
            "ok": plan.is_ok() && (!self.cfg().server.mem_check || need_ram <= avail_ram),
            "error": plan.err().map(|e| format!("{e:#}")),
        })
    }

    /// A killed backend that never exited still holds its GPU memory (e.g. a
    /// hung GPU). Loading another model on top of it can exhaust RAM and
    /// freeze the machine, so refuse until it is gone.
    fn check_zombies(&self) -> Result<()> {
        let mut z = self.exiting.lock().unwrap();
        z.retain(|pid| std::path::Path::new(&format!("/proc/{pid}")).exists());
        if let Some(pid) = z.first() {
            bail!(
                "a previous model backend (pid {pid}) was stopped but has not exited, so its GPU memory is still held \
                 (the GPU may be hung). Loading another model now could freeze the machine; force-stop lmdeck or reboot first"
            );
        }
        Ok(())
    }

    async fn make_room(&self, m: &LocalModel) -> Result<()> {
        let cfg = self.cfg();
        self.check_zombies()?;
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
        let need = if (m.format == Format::Mlx && self.on_gpu(m)) || m.format == Format::Strata {
            // Unified memory: the GPU estimate is the RAM it takes (at the shortest planned context).
            self.gpu_need_ctx(m, MLX_MIN_CTX)
        } else {
            m.size + m.size / 10 + (ctx * 128 * 1024).min(8 << 30) + (512 << 20)
        };
        let avail = sys::mem().1.saturating_sub((cfg.server.mem_reserve_gib * (1u64 << 30) as f64) as u64);
        (need, avail)
    }

    fn fits(&self, m: &LocalModel) -> bool {
        let (need, avail) = self.mem_need(m);
        need <= avail
    }

    async fn spawn(&self, m: &LocalModel, ka: Option<Duration>, heap: Option<u64>) -> Result<Arc<Running>> {
        let cfg = self.cfg();
        let ov = cfg.overrides.get(&m.name).cloned().unwrap_or_default();
        let port = free_port()?;
        let mut cwd: Option<std::path::PathBuf> = None;
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
                a.extend(["--prompt-cache-size".into(), cfg.mlx.prompt_cache_size.to_string()]);
                // Each cached prompt can hold GBs of KV; always bound the total.
                let cache_bytes = if cfg.mlx.prompt_cache_gib > 0.0 {
                    (cfg.mlx.prompt_cache_gib * (1u64 << 30) as f64) as u64
                } else {
                    // A quarter of the heap, but never more than what the weights and the planned
                    // attention cache leave free in it (a 19 GiB model in a 21 GiB heap).
                    let h = heap.unwrap_or_else(crate::compat::gpu_budget);
                    let used = self.gpu_need_ctx(m, self.plan_ctx(m)).saturating_sub(MLX_CACHE_ALLOWANCE);
                    (h / 4).min(h.saturating_sub(used)).max(MLX_CACHE_ALLOWANCE)
                };
                a.extend(["--prompt-cache-bytes".into(), cache_bytes.to_string()]);
                a.extend(["--prefill-step-size".into(), cfg.mlx.prefill_step_size.max(64).to_string()]);
                a.extend(cfg.mlx.extra_args.clone());
                let mut env: Vec<(String, String)> = cfg.mlx.env.clone().into_iter().collect();
                env.push(("LMDECK_MLX_BATCH".into(), if cfg.mlx.batching { "1" } else { "0" }.into()));
                env.push(("LMDECK_MLX_CACHE_LIMIT_GIB".into(), cfg.mlx.cache_limit_gib.to_string()));
                env.push(("LMDECK_MLX_DECODE_ATTN".into(), cfg.mlx.decode_attention.clone()));
                env.push(("LMDECK_MLX_CHECKPOINTS".into(), if cfg.mlx.prefix_checkpoints { "1" } else { "0" }.into()));
                env.push(("LMDECK_MLX_KV_BITS".into(), cfg.mlx.kv_bits.to_string()));
                env.push(("LMDECK_MLX_KV_GROUP".into(), cfg.mlx.kv_group_size.to_string()));
                env.push(("LMDECK_MLX_KV_START".into(), cfg.mlx.quantized_kv_start.to_string()));
                env.push(("LMDECK_MLX_MAX_KV".into(), cfg.mlx.max_kv_size.to_string()));
                env.push(("LMDECK_MLX_DEVICE".into(), ov.device.clone().unwrap_or_else(|| "gpu".into())));
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
                    if ov.device.as_deref() == Some("cpu") {
                        "0".into()
                    } else if let Some(n) = ov.gpu_layers.or_else(|| partial_layers().lock().unwrap().get(&m.name).copied()) {
                        n.to_string()
                    } else {
                        cfg.llama.gpu_layers.to_string()
                    },
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
                a.extend(["-ctk".into(), cfg.llama.cache_type_k.clone(), "-ctv".into(), cfg.llama.cache_type_v.clone()]);
                if cfg.llama.cache_reuse > 0 {
                    a.extend(["--cache-reuse".into(), cfg.llama.cache_reuse.to_string()]);
                }
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
            Format::Strata => {
                let dir = cfg.strata.dir();
                let python = dir.join(".venv/bin/python");
                if !python.exists() || !dir.join("serve/server.py").exists() {
                    bail!("Strata is not installed: Dashboard → T (or `lmdeck strata install`) sets it up in {}", dir.display());
                }
                let mock = cfg.strata.engine == "mock";
                if !mock {
                    let run: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&m.path)?)
                        .with_context(|| format!("reading {}", m.path.display()))?;
                    let exe = run["exe"].as_str().unwrap_or("");
                    if exe.is_empty() || !std::path::Path::new(exe).exists() {
                        bail!(
                            "{}: the Strata engine for Apple GPUs under Linux is not built yet (the Vulkan port, {} branch {}); nothing was loaded",
                            m.name, cfg.strata.repo, cfg.strata.git_ref
                        );
                    }
                }
                let ready = crate::config::state_dir().join(format!("strata-{port}.ready.json"));
                std::fs::create_dir_all(crate::config::state_dir())?;
                let _ = std::fs::remove_file(&ready);
                let mut a = vec![
                    python.display().to_string(),
                    dir.join("serve/server.py").display().to_string(),
                    "--engine".into(),
                    if mock { "mock".into() } else { "strata".into() },
                    "--config".into(),
                    m.path.display().to_string(),
                    "--host".into(),
                    "127.0.0.1".into(),
                    "--port".into(),
                    port.to_string(),
                    "--ready-file".into(),
                    ready.display().to_string(),
                ];
                if mock {
                    // The template opens a thinking block; the script closes it so the text is the answer.
                    a.extend(["--script".into(), "</think>\n\nHello from Strata's mock engine, served through lmdeck.".into()]);
                }
                a.extend(cfg.strata.extra_args.clone());
                let mut env: Vec<(String, String)> = cfg.strata.env.clone().into_iter().collect();
                let icd = cfg.strata.vk_icd();
                if icd.exists() {
                    env.push(("VK_DRIVER_FILES".into(), icd.display().to_string()));
                }
                if let Some(h) = heap {
                    // The arena the engine may fill with experts: the heap planned for it.
                    env.push(("STRATA_VK_ARENA_BYTES".into(), h.saturating_sub(STRATA_FLOOR).to_string()));
                }
                env.push(("PYTHONPATH".into(), dir.display().to_string()));
                cwd = Some(dir);
                (a, env)
            }
        };
        argv.extend(ov.extra_args.clone());
        env.extend(ov.env.clone());
        if let Some(h) = heap {
            // Per-process GPU heap for this backend only (Honeykrisp HK_SYSMEM).
            env.push(("HK_SYSMEM".into(), h.to_string()));
            sys::log(&format!("{}: GPU heap raised to {:.1} GiB for this backend", m.name, gib(h)));
        }
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
        {
            let (total, avail) = sys::mem();
            sys::log(&format!(
                "loading {} ({}) on :{port} [free {:.1}/{:.1} GiB, shmem {:.1} GiB]",
                m.name, m.format.as_str(), gib(avail), gib(total), gib(sys::shmem())
            ));
        }

        let mut cmd = Command::new(&argv[0]);
        if let Some(d) = &cwd {
            cmd.current_dir(d);
        }
        cmd.args(&argv[1..])
            .envs(env)
            .stdin(Stdio::null())
            .stdout(Stdio::from(lf.try_clone()?))
            .stderr(Stdio::from(lf))
            .process_group(0)
            .kill_on_drop(true);
        let child = cmd.spawn().with_context(|| format!("starting {}", argv[0]))?;
        if let Some(li) = self.load_info.lock().unwrap().as_mut() {
            li.log = Some(log.clone());
            li.log_offset = std::fs::metadata(&log).map(|m| m.len()).unwrap_or(0);
            li.phase = "starting";
            // Baseline right before the backend allocates anything.
            li.shmem0 = sys::shmem();
        }
        let pid = child.id().unwrap_or(0);
        let t0 = Instant::now();
        let r = Arc::new(Running {
            name: m.name.clone(),
            model: m.clone(),
            port,
            pid,
            log: log.clone(),
            sig: backend_sig(&cfg, m),
            child: tokio::sync::Mutex::new(child),
            loaded_at: SystemTime::now(),
            load_secs: 0.0,
            note: String::new(),
            last_used: Mutex::new(Instant::now()),
            keep_alive: Mutex::new(ka),
            in_flight: AtomicUsize::new(0),
            stopping: AtomicBool::new(false),
            work_start: Mutex::new(None),
        });
        if let Err(e) = self.wait_ready(&r, Duration::from_secs(cfg.server.load_timeout_secs)).await {
            self.kill(&r).await;
            let summary = sys::error_summary(&log);
            let hint = if crate::compat::is_gpu_gap(&summary) {
                let rec = (m.format == Format::Mlx)
                    .then(|| crate::compat::assess_mlx(&cfg, m.repo.as_deref().unwrap_or(&m.name), m.qmode.as_deref()).recommend)
                    .flatten();
                format!(
                    " — this model needs a GPU op omarchy-mlx doesn't have yet. Options: run it on the CPU (discouraged, much slower){}",
                    rec.map(|r| format!(", or download {r} instead")).unwrap_or_default()
                )
            } else {
                String::new()
            };
            return Err(anyhow!("{} failed to load: {summary}{hint}  [{e:#}; log: {}]", m.name, log.display()));
        }
        let secs = t0.elapsed().as_secs_f64();
        sys::log(&format!("{} ready in {secs:.1}s", m.name));
        // Arc::get_mut is fine: nobody else holds it yet.
        let mut r = r;
        if let Some(rm) = Arc::get_mut(&mut r) {
            rm.load_secs = secs;
            let mut note = launcher_notes(&log);
            if let Some(h) = heap {
                let extra = format!("GPU heap {:.1} GiB (raised for this model only)", gib(h));
                note = if note.is_empty() { extra } else { format!("{note} · {extra}") };
            }
            rm.note = note;
        }
        Ok(r)
    }

    /// The exception line if the backend's log (since this load began) shows a thread crash.
    fn backend_thread_died(&self, r: &Running) -> Option<String> {
        let offset = self.load_info.lock().unwrap().as_ref().map(|li| li.log_offset).unwrap_or(0);
        let text = sys::read_since(&r.log, offset, 64 * 1024);
        if !text.contains("Exception in thread") {
            return None;
        }
        Some(sys::error_summary(&r.log))
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
        self.set_phase("loading");
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
                        // mlx_lm.server keeps serving HTTP when its generator thread dies
                        // (e.g. Vulkan out of memory), so the request above would hang.
                        if let Some(dead) = self.backend_thread_died(r) {
                            bail!("backend's generator thread crashed while loading: {dead}");
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

    fn set_phase(&self, phase: &'static str) {
        if let Some(li) = self.load_info.lock().unwrap().as_mut() {
            li.phase = phase;
        }
    }

    /// Progress of the load in flight, for the TUI / panel / CLI.
    /// Weights: growth of GPU shmem since the load began vs the model size.
    /// Warm-up: the backend's own "Prompt processing progress: a/b" lines.
    pub fn load_progress(&self) -> Option<serde_json::Value> {
        let li = self.load_info.lock().unwrap().clone()?;
        let loaded = sys::shmem().saturating_sub(li.shmem0).min(li.size);
        let frac = if li.size > 0 { loaded as f64 / li.size as f64 } else { 0.0 };
        let elapsed = li.started.elapsed().as_secs_f64();
        let mut phase = li.phase;
        // llama.cpp uploads weights before /health turns OK.
        if phase == "starting" && frac > 0.02 {
            phase = "loading";
        }
        let mut warm: Option<(u64, u64)> = None;
        if let Some(log) = &li.log {
            let text = sys::read_since(log, li.log_offset, 16 * 1024);
            {
                if let Some(line) = text.lines().rev().find(|l| l.contains("Prompt processing progress:")) {
                    if let Some((a, b)) = line.rsplit(':').next().and_then(|t| t.trim().split_once('/')) {
                        if let (Ok(a), Ok(b)) = (a.trim().parse::<u64>(), b.trim().parse::<u64>()) {
                            warm = Some((a, b));
                        }
                    }
                }
            }
        }
        if phase == "loading" && (frac >= 0.97 || warm.is_some()) {
            phase = "warming up";
        }
        // Overall: weights are ~90% of the wait, the warm-up the rest.
        let overall = match phase {
            "preparing" | "starting" => 0.0,
            "warming up" => 0.9 + 0.1 * warm.map(|(a, b)| a as f64 / b.max(1) as f64).unwrap_or(0.0),
            _ => 0.9 * frac.min(1.0),
        };
        let rate = if elapsed > 1.0 { loaded as f64 / elapsed } else { 0.0 };
        let eta = (phase == "loading" && rate > 1e6).then(|| (li.size.saturating_sub(loaded)) as f64 / rate);
        Some(serde_json::json!({
            "model": li.model,
            "phase": phase,
            "progress": overall.clamp(0.0, 0.99),
            "loaded_bytes": loaded,
            "total_bytes": li.size,
            "elapsed_secs": elapsed,
            "eta_secs": eta,
            "warmup": warm.map(|(a, b)| serde_json::json!([a, b])),
        }))
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
            let _ = child.start_kill();
            // A process stuck in the GPU driver ignores SIGKILL until the GPU
            // lets go; don't block on it, remember it instead.
            if tokio::time::timeout(Duration::from_secs(5), child.wait()).await.is_err() {
                sys::log(&format!("{} backend (pid {}) did not exit after SIGKILL; GPU may be hung", r.name, r.pid));
                self.exiting.lock().unwrap().push(r.pid);
            }
        }
    }

    pub async fn stop(&self, r: &Arc<Running>) {
        self.running.lock().unwrap().retain(|x| !Arc::ptr_eq(x, r));
        self.kill(r).await;
        EVENTS.bump();
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

/// Warnings the MLX launcher printed for the current load (after the last spawn marker).
fn launcher_notes(log: &std::path::Path) -> String {
    let text = sys::read_tail(log, 64 * 1024);
    let start = text.rfind("===== ").unwrap_or(0);
    text[start..]
        .lines()
        .filter_map(|l| l.strip_prefix("lmdeck: "))
        .filter(|l| l.contains("not supported") || l.contains("not applicable"))
        .map(|l| l.split(" (").next().unwrap_or(l).to_string() + l.rsplit(';').next().map(|t| format!(";{t}")).unwrap_or_default().as_str())
        .collect::<Vec<_>>()
        .join(" · ")
}

/// Fingerprint of everything that shapes a backend process for `m`.
/// GPU layer counts chosen by the admission planner for GGUF loads that do not fit whole.
fn partial_layers() -> &'static Mutex<std::collections::HashMap<String, i32>> {
    static M: std::sync::OnceLock<Mutex<std::collections::HashMap<String, i32>>> = std::sync::OnceLock::new();
    M.get_or_init(Default::default)
}

/// `<arch>.block_count` from a GGUF file's metadata (transformer layers).
pub fn gguf_block_count(path: &std::path::Path) -> Option<u32> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::io::BufReader::new(std::fs::File::open(path).ok()?);
    let mut b8 = [0u8; 8];
    let mut b4 = [0u8; 4];
    f.read_exact(&mut b4).ok()?;
    if &b4 != b"GGUF" { return None; }
    f.read_exact(&mut b4).ok()?; // version
    f.read_exact(&mut b8).ok()?; // tensor count
    f.read_exact(&mut b8).ok()?;
    let kvs = u64::from_le_bytes(b8);
    fn skip(f: &mut std::io::BufReader<std::fs::File>, ty: u32) -> Option<()> {
        let fixed = |n: i64, f: &mut std::io::BufReader<std::fs::File>| f.seek(SeekFrom::Current(n)).ok().map(|_| ());
        let mut b8 = [0u8; 8];
        match ty {
            0 | 1 | 7 => fixed(1, f),
            2 | 3 => fixed(2, f),
            4 | 5 | 6 => fixed(4, f),
            10 | 11 | 12 => fixed(8, f),
            8 => { f.read_exact(&mut b8).ok()?; fixed(u64::from_le_bytes(b8) as i64, f) }
            9 => {
                let mut t = [0u8; 4];
                f.read_exact(&mut t).ok()?;
                f.read_exact(&mut b8).ok()?;
                for _ in 0..u64::from_le_bytes(b8) { skip(f, u32::from_le_bytes(t))?; }
                Some(())
            }
            _ => None,
        }
    }
    for _ in 0..kvs.min(4096) {
        f.read_exact(&mut b8).ok()?;
        let mut key = vec![0u8; (u64::from_le_bytes(b8) as usize).min(1 << 16)];
        f.read_exact(&mut key).ok()?;
        f.read_exact(&mut b4).ok()?;
        let ty = u32::from_le_bytes(b4);
        if key.ends_with(b".block_count") && (ty == 4 || ty == 5) {
            f.read_exact(&mut b4).ok()?;
            return Some(u32::from_le_bytes(b4));
        }
        skip(&mut f, ty)?;
    }
    None
}

#[cfg(test)]
mod work_tests {
    #[test]
    fn prefill_progress_from_log() {
        let p = std::env::temp_dir().join("lmdeck-work-test.log");
        std::fs::write(&p, "old\n2026-10-09 00:13:13,000 - INFO - Prompt processing progress: 0/6000\n2026-10-09 00:13:23,000 - INFO - Prompt processing progress: 1000/6000\n").unwrap();
        let w = super::work_progress(&p, 4, std::time::Instant::now());
        assert_eq!(w["phase"], "reading prompt");
        assert_eq!(w["prompt_done"], 1000);
        assert_eq!(w["tokens_per_sec"], 100.0);
        assert_eq!(w["eta_secs"], 50.0);
        std::fs::write(&p, "2026-10-09 00:13:13,000 - INFO - Prompt processing progress: 6000/6000\n").unwrap();
        assert_eq!(super::work_progress(&p, 0, std::time::Instant::now())["phase"], "generating");
        let _ = std::fs::remove_file(p);
    }
}

/// Context the MLX planner sizes the attention cache for (agents send long prompts); a tight
/// machine falls back to MLX_MIN_CTX before refusing.
const MLX_PLAN_CTX: u64 = 32768;
/// Strata's admission floor: the dense weights and buffers (~5 GiB on Qwen3.8-Flash-Next Q2_0)
/// plus the smallest useful expert arena; the engine fills the rest of its heap with experts.
const STRATA_FLOOR: u64 = 9 << 30;
const MLX_MIN_CTX: u64 = 8192;
/// Activations at prefill_step_size 512 plus Vulkan pipelines and scratch.
const MLX_RUNTIME: u64 = 640 << 20;
/// Prompt-cache checkpoints kept on the GPU when the heap is tight.
const MLX_CACHE_ALLOWANCE: u64 = 512 << 20;

pub fn backend_sig(cfg: &Config, m: &LocalModel) -> String {
    // System prompts are applied per request: changing one must not restart the backend.
    let ov = cfg.overrides.get(&m.name).map(|o| {
        let mut o = o.clone();
        o.system_prompt_mode = None;
        serde_json::to_string(&o).unwrap_or_default()
    }).unwrap_or_default();
    let section = match m.format {
        Format::Mlx => {
            let mut s = cfg.mlx.clone();
            s.system_prompt_mode = String::new();
            serde_json::to_string(&s).unwrap_or_default()
        }
        Format::Gguf => {
            let mut s = cfg.llama.clone();
            s.system_prompt_mode = String::new();
            serde_json::to_string(&s).unwrap_or_default()
        }
        Format::Strata => {
            let mut s = cfg.strata.clone();
            s.system_prompt_mode = String::new();
            serde_json::to_string(&s).unwrap_or_default()
        }
    };
    format!("{section}|{ov}|{}", cfg.server.uclamp_boost)
}

fn free_port() -> Result<u16> {
    let l = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(l.local_addr()?.port())
}

pub fn gib(b: u64) -> f64 {
    b as f64 / (1u64 << 30) as f64
}
