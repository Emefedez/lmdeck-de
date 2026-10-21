//! Configuration (`~/.config/lmdeck/config.toml`) and well-known paths.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerCfg,
    pub models: ModelsCfg,
    pub download: DownloadCfg,
    pub mlx: MlxCfg,
    pub llama: LlamaCfg,
    pub strata: StrataCfg,
    pub hooks: HooksCfg,
    pub usage: UsageCfg,
    pub vscode: VscodeCfg,
    pub ui: UiCfg,
    /// Short names: `alias = "org/repo[:tag]"`.
    pub aliases: BTreeMap<String, String>,
    /// Per-model overrides keyed by full model name.
    pub overrides: BTreeMap<String, ModelOverride>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerCfg {
    pub host: String,
    pub port: u16,
    /// Ollama-style keep alive: "5m", "30s", "1h", "0" (unload now), "-1" (forever).
    pub keep_alive: String,
    /// Max models resident at once. 1 keeps the GPU/unified memory for one model.
    pub max_loaded: usize,
    /// Seconds to wait for a backend to come up and answer the warm-up request.
    pub load_timeout_secs: u64,
    /// Refuse to load when estimated need exceeds MemAvailable minus this reserve.
    pub mem_reserve_gib: f64,
    /// Extra RAM kept free while the muvm VM is running (GiB).
    pub vm_reserve_gib: f64,
    /// Tools forwarded from Anthropic-style clients (Claude Code): "lean" (default) drops MCP
    /// tools and non-essential tools and shortens descriptions; "core" drops only MCP tools
    /// (mcp__*, which can add 100k+ tokens); "all" keeps everything.
    pub local_tools: String,
    /// "lean": drop Claude Code prompt sections a local model can't use (MCP server
    /// instructions, Claude model notes); in groups mode the MCP notes move into the groups.
    pub local_prompt: String,
    /// Skip the memory admission check entirely.
    pub mem_check: bool,
    /// Refuse GPU loads that exceed the GPU memory budget (Asahi gives the GPU
    /// ~half of RAM; overcommitting it can hang the whole machine).
    pub gpu_mem_check: bool,
    /// GPU heap per model backend (HK_SYSMEM, per process only):
    /// "auto" = raise above the stock 50%-of-RAM heap only when a model needs
    /// it, capped by available RAM minus the reserve; "stock" = never raise;
    /// a number = that many GiB.
    pub gpu_heap: String,
    /// Run backends with `uclampset -m 1024` so the CPU-side dispatch thread
    /// is not parked at low clocks by schedutil (big win for decode latency).
    pub uclamp_boost: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelsCfg {
    /// Hugging Face hub cache. Empty = $HF_HUB_CACHE / $HF_HOME/hub / ~/.cache/huggingface/hub.
    pub hf_cache: String,
    /// Extra directories scanned for loose *.gguf files.
    pub extra_dirs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DownloadCfg {
    /// Parallel HTTP range connections per large file.
    pub connections: usize,
    /// Files downloaded concurrently.
    pub parallel_files: usize,
    /// Quant picked when pulling a GGUF repo without a `:tag`.
    pub gguf_default_quant: String,
    /// Also fetch mmproj-*.gguf (vision projector) for GGUF repos.
    pub gguf_mmproj: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MlxCfg {
    /// Command prefix that starts an OpenAI-compatible MLX server.
    pub command: Vec<String>,
    /// Swap `-m mlx_lm.server` for lmdeck's launcher that removes a per-token
    /// GPU sync in mlx_lm.server (~2x decode on omarchy-mlx).
    pub fast_path: bool,
    /// Use mlx_lm's batched generator (only helps with concurrent requests).
    pub batching: bool,
    /// Save a prompt-cache checkpoint at the start of each answer so the next turn reuses the
    /// conversation even on hybrid models whose cache can't be rewound (fast path only).
    pub prefix_checkpoints: bool,
    /// Single-token decode over >= 1k tokens of context: "composed" (two matmuls + softmax,
    /// 1.2-1.8x faster on omarchy-mlx) or "fused" (the backend's SDPA kernel).
    pub decode_attention: String,
    /// mlx_lm prompt (KV) cache entries kept between requests; 0 disables reuse.
    pub prompt_cache_size: u32,
    /// Cap on prompt cache memory in GiB (0 = auto: a quarter of the GPU heap).
    pub prompt_cache_gib: f64,
    /// Cap on MLX's cache of freed GPU buffers in GiB (0 = MLX's default, the whole memory
    /// limit: one long prompt's temporaries then stay allocated until the model unloads).
    pub cache_limit_gib: f64,
    /// Context window advertised to clients (Copilot, Claude Code) for MLX
    /// models; their native windows (e.g. 262k) would exhaust GPU memory.
    pub max_context: u32,
    /// Prompt tokens processed per step. Smaller = less peak memory on long
    /// prompts (agent system prompts are 20k+ tokens), slightly slower prefill.
    pub prefill_step_size: u32,
    /// Quantize the KV cache to 4 or 8 bits (0 = f16). Needs runtime support;
    /// the launcher probes it and falls back to f16.
    pub kv_bits: u32,
    pub kv_group_size: u32,
    /// Start quantizing once the context passes this many tokens.
    pub quantized_kv_start: u32,
    /// Rotating KV window in tokens (0 = unbounded).
    pub max_kv_size: u32,
    pub extra_args: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// How the MLX system prompt (prompts/mlx.md, prompts/models/*.md) combines with the
    /// client's: "prepend" / "replace" / "fallback" (only when the client sends none).
    pub system_prompt_mode: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LlamaCfg {
    /// Path to llama-server. Empty = lmdeck's own build, then $PATH.
    pub server_path: String,
    pub gpu_layers: i32,
    /// When a GGUF model does not fit whole, offload only as many layers as fit instead of refusing.
    pub auto_offload: bool,
    pub ctx_size: u32,
    /// "on" | "off" | "auto"
    pub flash_attn: String,
    /// 0 = let llama.cpp decide.
    pub threads: u32,
    pub parallel: u32,
    /// KV cache element types: f16, q8_0, q4_0 (quantized KV saves memory).
    pub cache_type_k: String,
    pub cache_type_v: String,
    /// Reuse cached KV chunks of at least this many tokens via shifting (0 = off).
    pub cache_reuse: u32,
    pub extra_args: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// Git ref built by "Build llama.cpp".
    pub git_ref: String,
    /// The GGUF system prompt's mode (prompts/llama.md): "prepend" / "replace" / "fallback".
    pub system_prompt_mode: String,
}

/// Strata (Qwen3.8-Flash-Next, 125B MoE with experts streamed from the SSD) through its own
/// server. On Apple GPUs under Linux the engine is the Vulkan port in `repo`/`git_ref`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StrataCfg {
    /// Install directory (the checkout, its .venv and the run configs strata-<model>.json).
    /// Empty = ~/.local/share/lmdeck/strata.
    pub dir: String,
    pub repo: String,
    pub git_ref: String,
    /// "strata" (the real engine) or "mock" (Strata's scripted test engine: checks lmdeck's
    /// plumbing without a model or a GPU; lists the model strata/mock).
    pub engine: String,
    /// Vulkan driver (ICD json) the engine runs on; empty = honeykrisp-strata's v3 build.
    pub vk_icd: String,
    /// How the system prompt in prompts/strata.md combines with the client's.
    pub system_prompt_mode: String,
    pub extra_args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

impl Default for StrataCfg {
    fn default() -> Self {
        Self {
            dir: String::new(),
            repo: "https://github.com/Emefedez/strata-asahi".into(),
            git_ref: "vulkan-asahi".into(),
            engine: "strata".into(),
            vk_icd: String::new(),
            system_prompt_mode: "prepend".into(),
            extra_args: vec![],
            env: BTreeMap::new(),
        }
    }
}

impl StrataCfg {
    pub fn dir(&self) -> PathBuf {
        if self.dir.trim().is_empty() { data_dir().join("strata") } else { expand(&self.dir) }
    }
    pub fn vk_icd(&self) -> PathBuf {
        if self.vk_icd.trim().is_empty() {
            home().join(".local/lib/honeykrisp-strata/v3/share/vulkan/icd.d/asahi_icd.aarch64.json")
        } else {
            expand(&self.vk_icd)
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HooksCfg {
    /// Shell command run before a model loads (e.g. `tuned-adm profile asahi-performance`).
    pub on_load: String,
    /// Shell command run when the last model unloads.
    pub on_idle: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VscodeCfg {
    /// Copilot fallback: "auto" = use a local model for chat / inline Fix while
    /// VS Code reports the Copilot quota exhausted; "always"; "off".
    pub fallback: String,
    /// Model for the fallback (empty = loaded model if it calls tools, else
    /// the smallest local tool-calling model).
    pub model: String,
}

impl Default for VscodeCfg {
    fn default() -> Self {
        Self { fallback: "auto".into(), model: String::new() }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UiCfg {
    /// Editor for prompts and the config file (e.g. "nvim", "code"); empty = $VISUAL, $EDITOR, nvim.
    pub editor: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UsageCfg {
    /// Publish token usage to the Omarchy agents bar panel.
    pub omarchy_record: bool,
}

impl Default for UsageCfg {
    fn default() -> Self {
        Self { omarchy_record: true }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelOverride {
    pub ctx_size: Option<u32>,
    /// "cpu" runs this model on the CPU (discouraged; for models whose ops
    /// have no Apple GPU kernel yet). Unset/"gpu" = Apple GPU.
    pub device: Option<String>,
    pub keep_alive: Option<String>,
    /// GGUF: layers on the GPU for this model (overrides llama.gpu_layers and the auto plan).
    pub gpu_layers: Option<i32>,
    pub extra_args: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// Mode for this model's system prompt (the prompt itself is prompts/models/<model>.md).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt_mode: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerCfg::default(),
            models: ModelsCfg::default(),
            download: DownloadCfg::default(),
            mlx: MlxCfg::default(),
            llama: LlamaCfg::default(),
            strata: StrataCfg::default(),
            hooks: HooksCfg::default(),
            usage: UsageCfg::default(),
            vscode: VscodeCfg::default(),
            ui: UiCfg::default(),
            aliases: BTreeMap::new(),
            overrides: BTreeMap::new(),
        }
    }
}

impl Default for ServerCfg {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 11434,
            keep_alive: "5m".into(),
            max_loaded: 1,
            load_timeout_secs: 900,
            mem_reserve_gib: 3.0,
            vm_reserve_gib: 2.0,
            local_tools: "lean".into(),
            local_prompt: "lean".into(),
            mem_check: true,
            gpu_mem_check: true,
            gpu_heap: "auto".into(),
            uclamp_boost: true,
        }
    }
}

impl Default for ModelsCfg {
    fn default() -> Self {
        Self { hf_cache: String::new(), extra_dirs: vec![] }
    }
}

impl Default for DownloadCfg {
    fn default() -> Self {
        Self {
            connections: 6,
            parallel_files: 3,
            gguf_default_quant: "Q4_K_M".into(),
            gguf_mmproj: false,
        }
    }
}

impl Default for MlxCfg {
    fn default() -> Self {
        Self {
            command: vec!["mlx-omarchy".into(), "-m".into(), "mlx_lm.server".into()],
            fast_path: true,
            prefix_checkpoints: true,
            decode_attention: "composed".into(),
            batching: false,
            prompt_cache_size: 10,
            prompt_cache_gib: 0.0,
            cache_limit_gib: 1.0,
            prefill_step_size: 512,
            max_context: 65536,
            kv_bits: 0,
            kv_group_size: 64,
            quantized_kv_start: 512,
            max_kv_size: 0,
            extra_args: vec![],
            env: BTreeMap::new(),
            system_prompt_mode: "prepend".into(),
        }
    }
}

impl Default for LlamaCfg {
    fn default() -> Self {
        Self {
            server_path: String::new(),
            gpu_layers: 999,
            auto_offload: true,
            ctx_size: 8192,
            flash_attn: "auto".into(),
            threads: 0,
            parallel: 1,
            cache_type_k: "f16".into(),
            cache_type_v: "f16".into(),
            cache_reuse: 256,
            extra_args: vec![],
            env: BTreeMap::new(),
            git_ref: "master".into(),
            system_prompt_mode: "prepend".into(),
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let p = config_path();
        match std::fs::read_to_string(&p) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("parsing {}", p.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", p.display())),
        }
    }

    pub fn save(&self) -> Result<()> {
        let p = config_path();
        std::fs::create_dir_all(p.parent().unwrap())?;
        let tmp = p.with_extension("toml.tmp");
        std::fs::write(&tmp, toml::to_string_pretty(self)?)?;
        std::fs::rename(tmp, &p)?;
        Ok(())
    }

    /// Change the config file: re-read it, apply `f`, and write back only the keys that changed.
    /// Comments, key order and edits made elsewhere (an editor, the bar panel, the daemon, another
    /// lmdeck window) are kept; nothing is written when nothing changed.
    pub fn edit(f: impl FnOnce(&mut Config) -> Result<(), String>) -> Result<Config, String> {
        let p = config_path();
        let text = match std::fs::read_to_string(&p) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(format!("reading {}: {e}", p.display())),
        };
        let (out, cfg) = edit_text(&text, f).map_err(|e| format!("{}: {e}", p.display()))?;
        if let Some(out) = out {
            std::fs::create_dir_all(p.parent().unwrap()).map_err(|e| e.to_string())?;
            let tmp = p.with_extension("toml.tmp");
            std::fs::write(&tmp, out).and_then(|_| std::fs::rename(&tmp, &p)).map_err(|e| format!("writing {}: {e}", p.display()))?;
        }
        Ok(cfg)
    }

    pub fn base_url(&self) -> String {
        let host = if self.server.host == "0.0.0.0" { "127.0.0.1" } else { &self.server.host };
        format!("http://{}:{}", host, self.server.port)
    }

    pub fn hf_cache(&self) -> PathBuf {
        if !self.models.hf_cache.is_empty() {
            return expand(&self.models.hf_cache);
        }
        if let Ok(p) = std::env::var("HF_HUB_CACHE") {
            return PathBuf::from(p);
        }
        if let Ok(p) = std::env::var("HF_HOME") {
            return PathBuf::from(p).join("hub");
        }
        home().join(".cache/huggingface/hub")
    }

    pub fn llama_server(&self) -> Option<PathBuf> {
        if !self.llama.server_path.is_empty() {
            return Some(expand(&self.llama.server_path));
        }
        let own = llama_dir().join("bin/llama-server");
        if own.exists() {
            return Some(own);
        }
        which("llama-server")
    }
}

/// Parse an Ollama keep_alive value. `None` = never expire.
pub fn parse_duration(s: &str) -> Option<std::time::Duration> {
    use std::time::Duration;
    let s = s.trim();
    if s.starts_with('-') {
        return None;
    }
    if let Ok(n) = s.parse::<f64>() {
        return Some(Duration::from_secs_f64(n));
    }
    let (num, unit) = s.split_at(s.find(|c: char| c.is_alphabetic()).unwrap_or(s.len()));
    let n: f64 = num.parse().unwrap_or(5.0);
    let secs = match unit {
        "ms" => n / 1000.0,
        "s" => n,
        "m" => n * 60.0,
        "h" => n * 3600.0,
        _ => n * 60.0,
    };
    Some(Duration::from_secs_f64(secs))
}

pub fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"))
}

pub fn expand(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        home().join(rest)
    } else {
        PathBuf::from(p)
    }
}

/// The config file's text after `f`, or None when nothing changed.
fn edit_text(text: &str, f: impl FnOnce(&mut Config) -> Result<(), String>) -> Result<(Option<String>, Config), String> {
    let before: Config = toml::from_str(text).map_err(|e| e.to_string())?;
    let mut after = before.clone();
    f(&mut after)?;
    let table = |c: &Config| match toml::Value::try_from(c) {
        Ok(toml::Value::Table(t)) => Ok(t),
        Ok(_) => Err("config is not a table".to_string()),
        Err(e) => Err(e.to_string()),
    };
    let (old, new) = (table(&before)?, table(&after)?);
    if old == new {
        return Ok((None, after));
    }
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| e.to_string())?;
    merge_changes(doc.as_table_mut(), &old, &new);
    Ok((Some(doc.to_string()), after))
}

/// Write the keys that differ between `old` and `new` into `doc`, leaving everything else as is.
fn merge_changes(doc: &mut dyn toml_edit::TableLike, old: &toml::Table, new: &toml::Table) {
    let empty = toml::Table::new();
    for (k, nv) in new {
        let ov = old.get(k);
        if ov == Some(nv) {
            continue;
        }
        if let toml::Value::Table(nt) = nv {
            let ot = match ov { Some(toml::Value::Table(t)) => t, _ => &empty };
            if let Some(sub) = doc.get_mut(k).and_then(|i| i.as_table_like_mut()) {
                merge_changes(sub, ot, nt);
                continue;
            }
            if doc.get(k).is_none() {
                let mut t = toml_edit::Table::new();
                t.set_implicit(true);
                merge_changes(&mut t, &empty, nt);
                doc.insert(k, toml_edit::Item::Table(t));
                continue;
            }
        }
        let line = format!("x = {}", toml::Value::to_string(nv));
        let item = match nv {
            toml::Value::Table(_) => toml::to_string(&toml::Table::from_iter([("x".to_string(), nv.clone())]))
                .ok()
                .and_then(|t| t.parse::<toml_edit::DocumentMut>().ok())
                .and_then(|d| d.get("x").cloned()),
            _ => line.parse::<toml_edit::DocumentMut>().ok().and_then(|d| d.get("x").cloned()),
        };
        if let Some(item) = item {
            doc.insert(k, item);
        }
    }
    for k in old.keys() {
        if !new.contains_key(k) {
            doc.remove(k);
        }
    }
}

pub fn config_path() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| home().join(".config")).join("lmdeck/config.toml")
}

pub fn config_dir_or_home() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| home().join(".config"))
}

pub fn data_dir() -> PathBuf {
    dirs::data_dir().unwrap_or_else(|| home().join(".local/share")).join("lmdeck")
}

pub fn state_dir() -> PathBuf {
    dirs::state_dir().unwrap_or_else(|| home().join(".local/state")).join("lmdeck")
}

pub fn log_dir() -> PathBuf {
    state_dir().join("logs")
}

pub fn llama_dir() -> PathBuf {
    data_dir().join("llama.cpp")
}

pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .chain([home().join(".local/bin"), PathBuf::from("/usr/bin")])
        .map(|d| d.join(name))
        .find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata().map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

// ---------- editable settings (shared by TUI, daemon API and bar panel) ----------

/// (section, key, help). Sections group the TUI Settings tab and the bar panel.
pub const SETTINGS: &[(&str, &str, &str)] = &[
    ("Server", "server.host", "Listen address: 127.0.0.1 = this machine, 0.0.0.0 = LAN"),
    ("Server", "server.port", "Port (11434 = Ollama's, so clients work unchanged)"),
    ("Server", "server.load_timeout_secs", "Max seconds to load a model"),
    ("Server", "server.uclamp_boost", "Run backends under uclampset -m 1024 (helps on power-saver)"),
    ("Memory & residency", "server.keep_alive", "Idle time before unloading: 5m, 1h, 0, -1 (forever)"),
    ("Memory & residency", "server.max_loaded", "Models resident at once"),
    ("Memory & residency", "server.mem_check", "Refuse loads that would not fit in RAM (true/false)"),
    ("Memory & residency", "server.mem_reserve_gib", "RAM kept free for the desktop (GiB)"),
    ("Coding agents", "server.local_prompt", "Claude Code prompt for local models: lean (drop MCP instructions and Claude model notes) / original"),
    ("Coding agents", "server.local_tools", "Claude Code tools sent to local models: lean (no MCP, essential tools, short descriptions) / groups (lean + MCP on demand) / core (no MCP) / all"),
    ("Memory & residency", "server.vm_reserve_gib", "Extra RAM kept free while the muvm VM runs (GiB)"),
    ("Memory & residency", "server.gpu_heap", "GPU heap per model: auto (raise only when needed, per process) / stock (50% of RAM) / N GiB"),
    ("Memory & residency", "server.gpu_mem_check", "Refuse GPU loads over the GPU memory budget (turning this off can freeze the machine)"),
    ("System prompts", "mlx.system_prompt", "System prompt for MLX models, in prompts/mlx.md: Enter opens it in your editor ({model} {backend} {date} expand); per model: p in Models"),
    ("System prompts", "mlx.system_prompt_mode", "MLX: prepend (before the client's) / replace (instead of it) / fallback (only when the client sends none)"),
    ("System prompts", "llama.system_prompt", "System prompt for GGUF models, in prompts/llama.md: Enter opens it in your editor; per model: p in Models"),
    ("System prompts", "llama.system_prompt_mode", "GGUF: prepend / replace / fallback"),
    ("KV cache · MLX", "mlx.kv_bits", "MLX KV quantization: 0 = f16, 8 or 4 bits (probed; falls back to f16)"),
    ("KV cache · MLX", "mlx.kv_group_size", "MLX KV quantization group size (32 / 64 / 128)"),
    ("KV cache · MLX", "mlx.quantized_kv_start", "MLX: quantize KV once the context passes N tokens"),
    ("KV cache · MLX", "mlx.max_kv_size", "MLX rotating KV window in tokens (0 = unbounded)"),
    ("KV cache · MLX", "mlx.prompt_cache_size", "MLX prompt caches kept between requests (0 = no reuse)"),
    ("KV cache · MLX", "mlx.prompt_cache_gib", "Cap on MLX prompt cache memory in GiB (0 = auto: ¼ of the GPU heap)"),
    ("KV cache · MLX", "mlx.cache_limit_gib", "Cap on MLX's free-buffer cache in GiB (0 = unlimited: long prompts then keep GPU memory until unload)"),
    ("KV cache · MLX", "mlx.max_context", "Context window advertised to clients for MLX models (tokens)"),
    ("KV cache · MLX", "mlx.prefill_step_size", "Prompt tokens per prefill step (smaller = less peak memory on long prompts)"),
    ("KV cache · GGUF", "llama.ctx_size", "GGUF context window (KV is allocated for all of it)"),
    ("KV cache · GGUF", "llama.cache_type_k", "GGUF K cache type: f16 / q8_0 / q4_0 (q8_0 halves KV memory)"),
    ("KV cache · GGUF", "llama.cache_type_v", "GGUF V cache type: f16 / q8_0 / q4_0 (needs flash attention)"),
    ("KV cache · GGUF", "llama.cache_reuse", "GGUF: reuse cached prompt chunks ≥ N tokens (0 = off)"),
    ("MLX backend", "mlx.command", "Command that starts the MLX server"),
    ("MLX backend", "mlx.fast_path", "Patch out mlx_lm.server's per-token GPU sync (~2x decode)"),
    ("MLX backend", "mlx.decode_attention", "Decode attention over long contexts: composed (faster on omarchy-mlx: 4.8 -> 8.5 tok/s at 12k on a 27B) / fused"),
    ("MLX backend", "mlx.prefix_checkpoints", "Reuse the conversation between turns on hybrid models (agent follow-ups ~99 s -> ~13 s)"),
    ("MLX backend", "mlx.batching", "Batched generator: only for concurrent requests (slower per stream)"),
    ("MLX backend", "mlx.extra_args", "Extra mlx_lm.server args"),
    ("MLX backend", "mlx.env", "Env for the MLX backend, K=V,K=V (e.g. MLX_OMARCHY_GDN_RAW_REPEAT=0)"),
    ("llama.cpp backend", "llama.server_path", "llama-server binary (empty = lmdeck build, then PATH)"),
    ("llama.cpp backend", "llama.gpu_layers", "Layers offloaded to the GPU (999 = all)"),
    ("llama.cpp backend", "llama.auto_offload", "GGUF too big for free memory: put only the layers that fit on the GPU (true/false)"),
    ("llama.cpp backend", "llama.flash_attn", "Flash attention: on / off / auto"),
    ("llama.cpp backend", "llama.threads", "CPU threads (0 = auto)"),
    ("llama.cpp backend", "llama.parallel", "Parallel request slots"),
    ("llama.cpp backend", "llama.extra_args", "Extra llama-server args"),
    ("llama.cpp backend", "llama.env", "Env for the llama backend, K=V,K=V"),
    ("llama.cpp backend", "llama.git_ref", "llama.cpp branch/tag to build"),
    ("Strata backend", "strata.dir", "Strata install dir (empty = ~/.local/share/lmdeck/strata); install or update: T on the Dashboard"),
    ("Strata backend", "strata.git_ref", "Branch of the Strata checkout (vulkan-asahi = the Apple GPU port)"),
    ("Strata backend", "strata.engine", "strata (the real engine) / mock (scripted test engine, no model or GPU: lists strata/mock)"),
    ("Strata backend", "strata.vk_icd", "Vulkan driver for the engine (ICD json; empty = honeykrisp-strata v3)"),
    ("Strata backend", "strata.extra_args", "Extra Strata server args"),
    ("Strata backend", "strata.env", "Env for the Strata server and engine, K=V,K=V"),
    ("System prompts", "strata.system_prompt", "System prompt for Strata models, in prompts/strata.md: Enter opens it in your editor"),
    ("System prompts", "strata.system_prompt_mode", "Strata: prepend / replace / fallback"),
    ("Downloads & storage", "models.hf_cache", "Hugging Face cache dir (empty = default)"),
    ("Downloads & storage", "models.extra_dirs", "Extra dirs with loose .gguf files (comma separated)"),
    ("Downloads & storage", "download.connections", "Parallel connections per large file"),
    ("Downloads & storage", "download.parallel_files", "Files downloaded at once"),
    ("Downloads & storage", "download.gguf_default_quant", "Quant used when a GGUF pull has no :tag"),
    ("Downloads & storage", "download.gguf_mmproj", "Also fetch the vision projector (mmproj) for GGUF"),
    ("Integration", "hooks.on_load", "Shell cmd before the first model loads (e.g. tuned-adm profile asahi-performance)"),
    ("Integration", "hooks.on_idle", "Shell cmd after the last model unloads"),
    ("Integration", "vscode.fallback", "VS Code Copilot fallback: auto (local model while Copilot quota is exhausted) / always / off"),
    ("Integration", "vscode.model", "Model for the VS Code fallback (empty = automatic; must support tool calling)"),
    ("Integration", "ui.editor", "Editor for prompts and the config (e.g. nvim, code); empty = $VISUAL / $EDITOR / nvim"),
    ("Integration", "usage.omarchy_record", "Show lmdeck token usage in the Omarchy agents bar panel"),
];

pub fn kv_join(m: &std::collections::BTreeMap<String, String>) -> String {
    m.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(",")
}
pub fn kv_parse(s: &str) -> std::collections::BTreeMap<String, String> {
    s.split(',').filter_map(|p| p.split_once('=')).map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).collect()
}
fn words(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_string).collect()
}

pub fn get_setting(c: &Config, key: &str) -> String {
    match key {
        "server.host" => c.server.host.clone(),
        "server.port" => c.server.port.to_string(),
        "server.keep_alive" => c.server.keep_alive.clone(),
        "server.max_loaded" => c.server.max_loaded.to_string(),
        "server.load_timeout_secs" => c.server.load_timeout_secs.to_string(),
        "server.mem_check" => c.server.mem_check.to_string(),
        "server.local_tools" => c.server.local_tools.clone(),
        "server.local_prompt" => c.server.local_prompt.clone(),
        "server.gpu_mem_check" => c.server.gpu_mem_check.to_string(),
        "server.gpu_heap" => c.server.gpu_heap.clone(),
        "server.mem_reserve_gib" => c.server.mem_reserve_gib.to_string(),
        "server.vm_reserve_gib" => c.server.vm_reserve_gib.to_string(),
        "server.uclamp_boost" => c.server.uclamp_boost.to_string(),
        "models.hf_cache" => c.models.hf_cache.clone(),
        "models.extra_dirs" => c.models.extra_dirs.join(","),
        "download.connections" => c.download.connections.to_string(),
        "download.parallel_files" => c.download.parallel_files.to_string(),
        "download.gguf_default_quant" => c.download.gguf_default_quant.clone(),
        "download.gguf_mmproj" => c.download.gguf_mmproj.to_string(),
        "mlx.command" => c.mlx.command.join(" "),
        "mlx.fast_path" => c.mlx.fast_path.to_string(),
        "mlx.prefix_checkpoints" => c.mlx.prefix_checkpoints.to_string(),
        "mlx.decode_attention" => c.mlx.decode_attention.clone(),
        "mlx.batching" => c.mlx.batching.to_string(),
        "mlx.prompt_cache_size" => c.mlx.prompt_cache_size.to_string(),
        "mlx.prompt_cache_gib" => c.mlx.prompt_cache_gib.to_string(),
        "mlx.cache_limit_gib" => c.mlx.cache_limit_gib.to_string(),
        "mlx.prefill_step_size" => c.mlx.prefill_step_size.to_string(),
        "mlx.kv_bits" => c.mlx.kv_bits.to_string(),
        "mlx.kv_group_size" => c.mlx.kv_group_size.to_string(),
        "mlx.quantized_kv_start" => c.mlx.quantized_kv_start.to_string(),
        "mlx.max_kv_size" => c.mlx.max_kv_size.to_string(),
        "mlx.extra_args" => c.mlx.extra_args.join(" "),
        "mlx.env" => kv_join(&c.mlx.env),
        "llama.server_path" => c.llama.server_path.clone(),
        "llama.gpu_layers" => c.llama.gpu_layers.to_string(),
        "llama.auto_offload" => c.llama.auto_offload.to_string(),
        "llama.ctx_size" => c.llama.ctx_size.to_string(),
        "llama.flash_attn" => c.llama.flash_attn.clone(),
        "llama.threads" => c.llama.threads.to_string(),
        "llama.parallel" => c.llama.parallel.to_string(),
        "llama.cache_type_k" => c.llama.cache_type_k.clone(),
        "llama.cache_type_v" => c.llama.cache_type_v.clone(),
        "llama.cache_reuse" => c.llama.cache_reuse.to_string(),
        "llama.extra_args" => c.llama.extra_args.join(" "),
        "llama.env" => kv_join(&c.llama.env),
        "llama.git_ref" => c.llama.git_ref.clone(),
        "hooks.on_load" => c.hooks.on_load.clone(),
        "hooks.on_idle" => c.hooks.on_idle.clone(),
        "usage.omarchy_record" => c.usage.omarchy_record.to_string(),
        "vscode.fallback" => c.vscode.fallback.clone(),
        "vscode.model" => c.vscode.model.clone(),
        "mlx.max_context" => c.mlx.max_context.to_string(),
        "mlx.system_prompt" => crate::sysprompt::to_line(&crate::sysprompt::backend_text(crate::models::Format::Mlx)),
        "mlx.system_prompt_mode" => c.mlx.system_prompt_mode.clone(),
        "llama.system_prompt" => crate::sysprompt::to_line(&crate::sysprompt::backend_text(crate::models::Format::Gguf)),
        "ui.editor" => c.ui.editor.clone(),
        "strata.dir" => c.strata.dir.clone(),
        "strata.git_ref" => c.strata.git_ref.clone(),
        "strata.engine" => c.strata.engine.clone(),
        "strata.vk_icd" => c.strata.vk_icd.clone(),
        "strata.extra_args" => c.strata.extra_args.join(" "),
        "strata.env" => kv_join(&c.strata.env),
        "strata.system_prompt" => crate::sysprompt::to_line(&crate::sysprompt::backend_text(crate::models::Format::Strata)),
        "strata.system_prompt_mode" => c.strata.system_prompt_mode.clone(),
        "llama.system_prompt_mode" => c.llama.system_prompt_mode.clone(),
        _ => String::new(),
    }
}

pub fn set_setting(c: &mut Config, key: &str, v: &str) -> Result<(), String> {
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
        "server.local_prompt" => match v {
            "lean" | "original" => c.server.local_prompt = v.into(),
            _ => return Err("lean or original".into()),
        },
        "server.local_tools" => match v {
            "lean" | "groups" | "core" | "all" => c.server.local_tools = v.into(),
            _ => return Err("lean, groups, core or all".into()),
        },
        "server.gpu_mem_check" => c.server.gpu_mem_check = boolean(v)?,
        "server.gpu_heap" => {
            if v != "auto" && v != "stock" && v.parse::<f64>().map(|g| !(4.0..=60.0).contains(&g)).unwrap_or(true) {
                return Err("use auto, stock, or a size in GiB (4–60)".into());
            }
            c.server.gpu_heap = v.into()
        }
        "server.mem_reserve_gib" => c.server.mem_reserve_gib = v.parse().map_err(|_| "not a number".to_string())?,
        "server.vm_reserve_gib" => c.server.vm_reserve_gib = v.parse().map_err(|_| "not a number".to_string())?,
        "server.uclamp_boost" => c.server.uclamp_boost = boolean(v)?,
        "models.hf_cache" => c.models.hf_cache = v.into(),
        "models.extra_dirs" => c.models.extra_dirs = v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
        "download.connections" => c.download.connections = num(v)?.clamp(1, 32) as usize,
        "download.parallel_files" => c.download.parallel_files = num(v)?.clamp(1, 16) as usize,
        "download.gguf_default_quant" => c.download.gguf_default_quant = v.into(),
        "download.gguf_mmproj" => c.download.gguf_mmproj = boolean(v)?,
        "mlx.command" => c.mlx.command = words(v),
        "mlx.fast_path" => c.mlx.fast_path = boolean(v)?,
        "mlx.prefix_checkpoints" => c.mlx.prefix_checkpoints = boolean(v)?,
        "mlx.decode_attention" => match v {
            "composed" | "fused" => c.mlx.decode_attention = v.into(),
            _ => return Err("composed or fused".into()),
        },
        "mlx.batching" => c.mlx.batching = boolean(v)?,
        "mlx.prompt_cache_size" => c.mlx.prompt_cache_size = num(v)? as u32,
        "mlx.prompt_cache_gib" => c.mlx.prompt_cache_gib = v.parse().map_err(|_| "not a number".to_string())?,
        "mlx.cache_limit_gib" => c.mlx.cache_limit_gib = v.parse::<f64>().map_err(|_| "not a number".to_string())?.max(0.0),
        "mlx.prefill_step_size" => c.mlx.prefill_step_size = (num(v)? as u32).clamp(64, 8192),
        "mlx.kv_bits" => {
            let b = num(v)? as u32;
            if ![0, 4, 8].contains(&b) {
                return Err("use 0, 4 or 8".into());
            }
            c.mlx.kv_bits = b
        }
        "mlx.kv_group_size" => {
            let g = num(v)? as u32;
            if ![32, 64, 128].contains(&g) {
                return Err("use 32, 64 or 128".into());
            }
            c.mlx.kv_group_size = g
        }
        "mlx.quantized_kv_start" => c.mlx.quantized_kv_start = num(v)? as u32,
        "mlx.max_kv_size" => c.mlx.max_kv_size = num(v)? as u32,
        "mlx.extra_args" => c.mlx.extra_args = words(v),
        "mlx.env" => c.mlx.env = kv_parse(v),
        "llama.server_path" => c.llama.server_path = v.into(),
        "llama.gpu_layers" => c.llama.gpu_layers = v.parse().map_err(|_| "not a number".to_string())?,
        "llama.auto_offload" => c.llama.auto_offload = v.parse().map_err(|_| "true or false".to_string())?,
        "llama.ctx_size" => c.llama.ctx_size = num(v)? as u32,
        "llama.flash_attn" => {
            if !["on", "off", "auto"].contains(&v) {
                return Err("use on, off or auto".into());
            }
            c.llama.flash_attn = v.into()
        }
        "llama.threads" => c.llama.threads = num(v)? as u32,
        "llama.parallel" => c.llama.parallel = num(v)?.max(1) as u32,
        "llama.cache_type_k" | "llama.cache_type_v" => {
            if !["f16", "bf16", "q8_0", "q5_1", "q5_0", "q4_1", "q4_0", "iq4_nl", "f32"].contains(&v) {
                return Err("use f16, q8_0 or q4_0".into());
            }
            if key == "llama.cache_type_k" { c.llama.cache_type_k = v.into() } else { c.llama.cache_type_v = v.into() }
        }
        "llama.cache_reuse" => c.llama.cache_reuse = num(v)? as u32,
        "llama.extra_args" => c.llama.extra_args = words(v),
        "llama.env" => c.llama.env = kv_parse(v),
        "llama.git_ref" => c.llama.git_ref = v.into(),
        "hooks.on_load" => c.hooks.on_load = v.into(),
        "hooks.on_idle" => c.hooks.on_idle = v.into(),
        "usage.omarchy_record" => c.usage.omarchy_record = boolean(v)?,
        "vscode.fallback" => {
            if !["auto", "always", "off"].contains(&v) {
                return Err("use auto, always or off".into());
            }
            c.vscode.fallback = v.into()
        }
        "vscode.model" => c.vscode.model = v.into(),
        "mlx.max_context" => c.mlx.max_context = (num(v)? as u32).clamp(2048, 1 << 20),
        // Written to prompts/<backend>.md right away (the bar panel edits them as one line).
        "mlx.system_prompt" | "llama.system_prompt" => {
            let f = if key == "mlx.system_prompt" { crate::models::Format::Mlx } else { crate::models::Format::Gguf };
            crate::sysprompt::set_backend_text(f, &crate::sysprompt::from_line(v)).map_err(|e| e.to_string())?
        }
        "ui.editor" => c.ui.editor = v.into(),
        "strata.dir" => c.strata.dir = v.into(),
        "strata.git_ref" => c.strata.git_ref = v.into(),
        "strata.engine" => match v {
            "strata" | "mock" => c.strata.engine = v.into(),
            _ => return Err("strata or mock".into()),
        },
        "strata.vk_icd" => c.strata.vk_icd = v.into(),
        "strata.extra_args" => c.strata.extra_args = words(v),
        "strata.env" => c.strata.env = kv_parse(v),
        "strata.system_prompt" => crate::sysprompt::set_backend_text(crate::models::Format::Strata, &crate::sysprompt::from_line(v)).map_err(|e| e.to_string())?,
        "strata.system_prompt_mode" => {
            if !crate::sysprompt::MODES.contains(&v) {
                return Err("use prepend, replace or fallback".into());
            }
            c.strata.system_prompt_mode = v.into()
        }
        "mlx.system_prompt_mode" | "llama.system_prompt_mode" => {
            if !crate::sysprompt::MODES.contains(&v) {
                return Err("use prepend, replace or fallback".into());
            }
            if key == "mlx.system_prompt_mode" { c.mlx.system_prompt_mode = v.into() } else { c.llama.system_prompt_mode = v.into() }
        }
        _ => return Err("unknown setting".into()),
    }
    Ok(())
}


/// Settings whose change only takes effect when a backend is (re)started.
pub fn needs_reload(key: &str) -> bool {
    if key.ends_with(".system_prompt") || key.ends_with(".system_prompt_mode") || key.starts_with("ui.") {
        return false; // applied per request
    }
    key.starts_with("mlx.")
        || (key.starts_with("llama.") && key != "llama.git_ref")
        || matches!(key, "strata.engine" | "strata.vk_icd" | "strata.extra_args" | "strata.env")
        || key == "server.uclamp_boost"
}

#[cfg(test)]
mod tests {
    #[test]
    fn edit_keeps_comments_unknown_keys_and_untouched_values() {
        let text = "# my notes\n[server]\n# keep this\nport = 11434\nkeep_alive = \"30m\" # inline\n\n[mystery]\nx = 1\n";
        let (out, cfg) = super::edit_text(text, |c| { c.server.mem_reserve_gib = 2.5; Ok(()) }).unwrap();
        let out = out.unwrap();
        assert!(out.contains("# my notes") && out.contains("# keep this") && out.contains("# inline"), "{out}");
        assert!(out.contains("[mystery]") && out.contains("keep_alive = \"30m\""), "{out}");
        assert!(out.contains("mem_reserve_gib = 2.5"), "{out}");
        assert_eq!(cfg.server.mem_reserve_gib, 2.5);
        // Nothing changed: nothing to write.
        assert!(super::edit_text(text, |_| Ok(())).unwrap().0.is_none());
        // A new table (a per-model override) is added without touching the rest.
        let (out, _) = super::edit_text(text, |c| { c.overrides.entry("org/m".into()).or_default().ctx_size = Some(8192); Ok(()) }).unwrap();
        let out = out.unwrap();
        assert!(out.contains("ctx_size = 8192") && out.contains("# keep this"), "{out}");
        let back: super::Config = toml::from_str(&out).unwrap();
        assert_eq!(back.overrides["org/m"].ctx_size, Some(8192));
    }
}
