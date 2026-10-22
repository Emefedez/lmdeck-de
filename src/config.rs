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
    /// Tool name globs always kept for local models, even when local_tools would drop them
    /// (wins over tool_deny). See toolrules.rs.
    pub tool_allow: Vec<String>,
    /// Tool name globs never sent to local models (tool groups included).
    pub tool_deny: Vec<String>,
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
    /// Prompt path and chunk: "off" = decode windows, or 1024/2048/4096 rows on the GPU's
    /// matrix units. Measured on a 10,609-token prompt (2026-10-22): 14.7 / 109.0 / 123.8 /
    /// 134.7 tok/s; the chunk buffers take 0 / 1.60 / 2.55 / 4.46 GB of the arena.
    pub prompt_path: String,
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
            prompt_path: "off".into(),
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
    /// Favourite models (full names): marked ★ and listed first in Models.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub favorites: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UsageCfg {
    /// Publish token usage to the Omarchy agents bar panel.
    pub omarchy_record: bool,
    /// Record each model's decode speed and system power while it generates (the grader's
    /// measured numbers). Off by default: performance first.
    pub perf: bool,
}

impl Default for UsageCfg {
    fn default() -> Self {
        Self { omarchy_record: true, perf: false }
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
            tool_allow: Vec::new(),
            tool_deny: Vec::new(),
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
/// How a setting is edited in the TUI.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Text,
    Number,
    Bool,
    Choice(&'static [&'static str]),
    /// A GiB slider (←/→ in steps of 0.5).
    Gib,
    /// Strata chunked prompt slider (off / 1024 / 2048 / 4096).
    PromptChunk,
    /// A prompt file, opened in the editor.
    Prompt,
}

pub const STRATA_PROMPT_CHUNKS: &[&str] = &["off", "1024", "2048", "4096"];

pub fn prompt_chunk_step(cur: &str, dir: i32) -> &'static str {
    let idx = STRATA_PROMPT_CHUNKS.iter().position(|&c| c == cur).unwrap_or(0);
    let next = (idx as i32 + dir).clamp(0, (STRATA_PROMPT_CHUNKS.len() - 1) as i32) as usize;
    STRATA_PROMPT_CHUNKS[next]
}

pub fn prompt_chunk_cycle(cur: &str) -> &'static str {
    let idx = STRATA_PROMPT_CHUNKS.iter().position(|&c| c == cur).unwrap_or(0);
    STRATA_PROMPT_CHUNKS[(idx + 1) % STRATA_PROMPT_CHUNKS.len()]
}

/// Benchmark measurements from a 10,609-token prompt (engine count, Q2_0, arena ~13.0 GB, greedy, 163 tokens)
/// on Apple M1 Max 32 GB, Asahi Linux.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StrataPromptMetrics {
    pub mode: &'static str,
    pub read_s: f64,
    pub tok_s: f64,
    pub speedup: f64,
    pub buffer_gib: f64,
    pub decode_cache_gib: f64,
    pub note: &'static str,
}

impl StrataPromptMetrics {
    /// Speed star rating derived from prompt reading tok/s.
    pub fn speed_stars(&self) -> u8 {
        if self.tok_s >= 130.0 {
            5
        } else if self.tok_s >= 120.0 {
            4
        } else if self.tok_s >= 50.0 {
            3
        } else if self.tok_s >= 25.0 {
            2
        } else {
            1
        }
    }

    /// Usability / performance rating derived from decode cache left in arena.
    pub fn usability_stars(&self) -> u8 {
        if self.decode_cache_gib >= 12.5 {
            5
        } else if self.decode_cache_gib >= 11.0 {
            4
        } else if self.decode_cache_gib >= 10.0 {
            3
        } else if self.decode_cache_gib >= 8.0 {
            2
        } else {
            1
        }
    }

    /// Noise & heat star rating (5 = quietest & coolest; matrix unit load causes heat).
    pub fn noise_heat_stars(&self) -> u8 {
        if self.buffer_gib <= 0.05 {
            5
        } else if self.buffer_gib <= 2.0 {
            4
        } else if self.buffer_gib <= 3.0 {
            3
        } else if self.buffer_gib <= 5.0 {
            2
        } else {
            1
        }
    }
}

pub const STRATA_PROMPT_BENCHMARKS: &[StrataPromptMetrics] = &[
    StrataPromptMetrics {
        mode: "off",
        read_s: 722.8,
        tok_s: 14.7,
        speedup: 1.0,
        buffer_gib: 0.0,
        decode_cache_gib: 13.0,
        note: "Decode windows (baseline): quiet & cool, leaves 13.0 GB decode cache",
    },
    StrataPromptMetrics {
        mode: "1024",
        read_s: 97.3,
        tok_s: 109.0,
        speedup: 7.4,
        buffer_gib: 1.60,
        decode_cache_gib: 11.4,
        note: "7.4x prompt speedup with low 1.60 GB chunk buffer overhead",
    },
    StrataPromptMetrics {
        mode: "2048",
        read_s: 85.7,
        tok_s: 123.8,
        speedup: 8.4,
        buffer_gib: 2.55,
        decode_cache_gib: 10.4,
        note: "Best balance on 32 GB: 8.4x speedup, 10.4 GB decode cache left",
    },
    StrataPromptMetrics {
        mode: "4096",
        read_s: 78.7,
        tok_s: 134.7,
        speedup: 9.2,
        buffer_gib: 4.46,
        decode_cache_gib: 8.5,
        note: "Fastest prompt read (9.2x), but chunk buffers take 4.46 GB of arena",
    },
];

pub fn strata_prompt_metrics(mode: &str) -> Option<StrataPromptMetrics> {
    STRATA_PROMPT_BENCHMARKS.iter().find(|b| b.mode == mode).copied()
}

pub struct Setting {
    pub section: &'static str,
    pub key: &'static str,
    pub kind: Kind,
    pub help: &'static str,
}

const MODE: Kind = Kind::Choice(&["prepend", "replace", "fallback"]);

/// Every setting, in the order the TUI shows them.
pub const SETTINGS: &[Setting] = &[
    Setting { section: "Server", key: "server.host", kind: Kind::Text, help: "Listen address: 127.0.0.1 = this machine, 0.0.0.0 = LAN" },
    Setting { section: "Server", key: "server.port", kind: Kind::Number, help: "Port (11434 = Ollama's, so clients work unchanged)" },
    Setting { section: "Server", key: "server.keep_alive", kind: Kind::Text, help: "Idle time before unloading: 5m, 1h, 0, -1 (forever)" },
    Setting { section: "Server", key: "server.max_loaded", kind: Kind::Number, help: "Models resident at once" },
    Setting { section: "Server", key: "server.load_timeout_secs", kind: Kind::Number, help: "Max seconds to load a model" },
    Setting { section: "Server", key: "server.uclamp_boost", kind: Kind::Bool, help: "Run backends under uclampset -m 1024 (helps on power-saver)" },
    Setting { section: "Memory", key: "server.mem_check", kind: Kind::Bool, help: "Refuse loads that would not fit in RAM" },
    Setting { section: "Memory", key: "server.gpu_mem_check", kind: Kind::Bool, help: "Refuse GPU loads over the GPU memory budget (turning this off can freeze the machine)" },
    Setting { section: "Memory", key: "server.mem_reserve_gib", kind: Kind::Gib, help: "RAM kept free for the desktop (GiB)" },
    Setting { section: "Memory", key: "server.vm_reserve_gib", kind: Kind::Gib, help: "Extra RAM kept free while the muvm VM runs (GiB)" },
    Setting { section: "Memory", key: "server.gpu_heap", kind: Kind::Gib, help: "GPU heap per model: auto (raise only when needed, per process) / stock (50% of RAM) / N GiB" },
    Setting { section: "MLX", key: "mlx.command", kind: Kind::Text, help: "Command that starts the MLX server (choose the runtime in Backends)" },
    Setting { section: "MLX", key: "mlx.max_context", kind: Kind::Number, help: "Context window advertised to clients for MLX models (tokens)" },
    Setting { section: "MLX", key: "mlx.fast_path", kind: Kind::Bool, help: "Patch out mlx_lm.server's per-token GPU sync (~2x decode)" },
    Setting { section: "MLX", key: "mlx.decode_attention", kind: Kind::Choice(&["composed", "fused"]), help: "Decode attention over long contexts: composed (faster on omarchy-mlx: 4.8 -> 8.5 tok/s at 12k on a 27B) / fused" },
    Setting { section: "MLX", key: "mlx.prefix_checkpoints", kind: Kind::Bool, help: "Reuse the conversation between turns on hybrid models (agent follow-ups ~99 s -> ~13 s)" },
    Setting { section: "MLX", key: "mlx.batching", kind: Kind::Bool, help: "Batched generator: only for concurrent requests (slower per stream)" },
    Setting { section: "MLX", key: "mlx.prefill_step_size", kind: Kind::Number, help: "Prompt tokens per prefill step (smaller = less peak memory on long prompts)" },
    Setting { section: "MLX", key: "mlx.kv_bits", kind: Kind::Choice(&["0", "8", "4"]), help: "MLX KV quantization: 0 = f16, 8 or 4 bits (probed; falls back to f16)" },
    Setting { section: "MLX", key: "mlx.kv_group_size", kind: Kind::Choice(&["32", "64", "128"]), help: "MLX KV quantization group size (32 / 64 / 128)" },
    Setting { section: "MLX", key: "mlx.quantized_kv_start", kind: Kind::Number, help: "MLX: quantize KV once the context passes N tokens" },
    Setting { section: "MLX", key: "mlx.max_kv_size", kind: Kind::Number, help: "MLX rotating KV window in tokens (0 = unbounded)" },
    Setting { section: "MLX", key: "mlx.prompt_cache_size", kind: Kind::Number, help: "MLX prompt caches kept between requests (0 = no reuse)" },
    Setting { section: "MLX", key: "mlx.prompt_cache_gib", kind: Kind::Number, help: "Cap on MLX prompt cache memory in GiB (0 = auto: ¼ of the GPU heap)" },
    Setting { section: "MLX", key: "mlx.cache_limit_gib", kind: Kind::Number, help: "Cap on MLX's free-buffer cache in GiB (0 = unlimited: long prompts then keep GPU memory until unload)" },
    Setting { section: "MLX", key: "mlx.extra_args", kind: Kind::Text, help: "Extra mlx_lm.server args" },
    Setting { section: "MLX", key: "mlx.env", kind: Kind::Text, help: "Env for the MLX backend, K=V,K=V (e.g. MLX_OMARCHY_GDN_RAW_REPEAT=0)" },
    Setting { section: "GGUF (llama.cpp)", key: "llama.server_path", kind: Kind::Text, help: "llama-server binary (empty = lmdeck build, then PATH)" },
    Setting { section: "GGUF (llama.cpp)", key: "llama.git_ref", kind: Kind::Text, help: "llama.cpp branch or tag that Backends builds" },
    Setting { section: "GGUF (llama.cpp)", key: "llama.ctx_size", kind: Kind::Number, help: "GGUF context window (KV is allocated for all of it)" },
    Setting { section: "GGUF (llama.cpp)", key: "llama.gpu_layers", kind: Kind::Number, help: "Layers offloaded to the GPU (999 = all)" },
    Setting { section: "GGUF (llama.cpp)", key: "llama.auto_offload", kind: Kind::Bool, help: "GGUF too big for free memory: put only the layers that fit on the GPU" },
    Setting { section: "GGUF (llama.cpp)", key: "llama.flash_attn", kind: Kind::Choice(&["auto", "on", "off"]), help: "Flash attention: on / off / auto" },
    Setting { section: "GGUF (llama.cpp)", key: "llama.cache_type_k", kind: Kind::Choice(&["f16", "q8_0", "q4_0"]), help: "GGUF K cache type: f16 / q8_0 / q4_0 (q8_0 halves KV memory)" },
    Setting { section: "GGUF (llama.cpp)", key: "llama.cache_type_v", kind: Kind::Choice(&["f16", "q8_0", "q4_0"]), help: "GGUF V cache type: f16 / q8_0 / q4_0 (needs flash attention)" },
    Setting { section: "GGUF (llama.cpp)", key: "llama.cache_reuse", kind: Kind::Number, help: "GGUF: reuse cached prompt chunks ≥ N tokens (0 = off)" },
    Setting { section: "GGUF (llama.cpp)", key: "llama.threads", kind: Kind::Number, help: "CPU threads (0 = auto)" },
    Setting { section: "GGUF (llama.cpp)", key: "llama.parallel", kind: Kind::Number, help: "Parallel request slots" },
    Setting { section: "GGUF (llama.cpp)", key: "llama.extra_args", kind: Kind::Text, help: "Extra llama-server args" },
    Setting { section: "GGUF (llama.cpp)", key: "llama.env", kind: Kind::Text, help: "Env for the llama backend, K=V,K=V" },
    Setting { section: "Strata", key: "strata.dir", kind: Kind::Text, help: "Strata install dir (empty = ~/.local/share/lmdeck/strata); install and update in Backends" },
    Setting { section: "Strata", key: "strata.git_ref", kind: Kind::Text, help: "Branch of the Strata checkout (vulkan-asahi = the Apple GPU port)" },
    Setting { section: "Strata", key: "strata.engine", kind: Kind::Choice(&["strata", "mock"]), help: "strata (the real engine) / mock (scripted test engine, no model or GPU: lists strata/mock)" },
    Setting { section: "Strata", key: "strata.vk_icd", kind: Kind::Text, help: "Vulkan driver for the engine (ICD json; empty = honeykrisp-strata v3)" },
    Setting { section: "Strata", key: "strata.extra_args", kind: Kind::Text, help: "Extra Strata server args" },
    Setting { section: "Strata", key: "strata.env", kind: Kind::Text, help: "Env for the Strata server and engine, K=V,K=V" },
    Setting { section: "Strata", key: "strata.prompt_path", kind: Kind::PromptChunk, help: "Strata prompt path: off (decode windows) / 1024 / 2048 / 4096 (GPU chunked path; 2048 best on 32 GB)" },
    Setting { section: "System prompts", key: "mlx.system_prompt", kind: Kind::Prompt, help: "System prompt for MLX models (prompts/mlx.md, opens in your editor; {model} {backend} {date} expand). Per model: e in Models" },
    Setting { section: "System prompts", key: "mlx.system_prompt_mode", kind: MODE, help: "MLX: prepend (before the client's) / replace (instead of it) / fallback (only when the client sends none)" },
    Setting { section: "System prompts", key: "llama.system_prompt", kind: Kind::Prompt, help: "System prompt for GGUF models (prompts/llama.md, opens in your editor). Per model: e in Models" },
    Setting { section: "System prompts", key: "llama.system_prompt_mode", kind: MODE, help: "GGUF: prepend / replace / fallback" },
    Setting { section: "System prompts", key: "strata.system_prompt", kind: Kind::Prompt, help: "System prompt for Strata models (prompts/strata.md, opens in your editor)" },
    Setting { section: "System prompts", key: "strata.system_prompt_mode", kind: MODE, help: "Strata: prepend / replace / fallback" },
    Setting { section: "Coding agents", key: "server.local_prompt", kind: Kind::Choice(&["lean", "original"]), help: "Claude Code prompt for local models: lean (drop MCP instructions and Claude model notes) / original" },
    Setting { section: "Coding agents", key: "server.tool_allow", kind: Kind::Text, help: "Tool globs always sent to local models, even if local_tools drops them (wins over deny), e.g. mcp__rea__demangle_swift" },
    Setting { section: "Coding agents", key: "server.tool_deny", kind: Kind::Text, help: "Tool globs never sent to local models, e.g. mcp__rea__* WebSearch (lmdeck tools shows each tool's status)" },
    Setting { section: "Coding agents", key: "server.local_tools", kind: Kind::Choice(&["lean", "groups", "core", "all"]), help: "Claude Code tools sent to local models: lean (no MCP, essential tools, short descriptions) / groups (lean + MCP on demand) / core (no MCP) / all" },
    Setting { section: "Downloads", key: "models.hf_cache", kind: Kind::Text, help: "Hugging Face cache dir (empty = default)" },
    Setting { section: "Downloads", key: "models.extra_dirs", kind: Kind::Text, help: "Extra dirs with loose .gguf files (comma separated)" },
    Setting { section: "Downloads", key: "download.connections", kind: Kind::Number, help: "Parallel connections per large file" },
    Setting { section: "Downloads", key: "download.parallel_files", kind: Kind::Number, help: "Files downloaded at once" },
    Setting { section: "Downloads", key: "download.gguf_default_quant", kind: Kind::Text, help: "Quant used when a GGUF pull has no :tag" },
    Setting { section: "Downloads", key: "download.gguf_mmproj", kind: Kind::Bool, help: "Also fetch the vision projector (mmproj) for GGUF" },
    Setting { section: "Integration", key: "ui.editor", kind: Kind::Text, help: "Editor for prompts and the config (e.g. nvim, code); empty = $VISUAL / $EDITOR / nvim" },
    Setting { section: "Integration", key: "hooks.on_load", kind: Kind::Text, help: "Shell cmd before the first model loads (e.g. tuned-adm profile asahi-performance)" },
    Setting { section: "Integration", key: "hooks.on_idle", kind: Kind::Text, help: "Shell cmd after the last model unloads" },
    Setting { section: "Integration", key: "vscode.fallback", kind: Kind::Choice(&["auto", "always", "off"]), help: "VS Code Copilot fallback: auto (local model while Copilot quota is exhausted) / always / off" },
    Setting { section: "Integration", key: "vscode.model", kind: Kind::Text, help: "Model for the VS Code fallback (empty = automatic; must support tool calling)" },
    Setting { section: "Integration", key: "usage.omarchy_record", kind: Kind::Bool, help: "Show lmdeck token usage in the Omarchy agents bar panel" },
    Setting { section: "Integration", key: "usage.perf", kind: Kind::Bool, help: "Record each model's speed and system power while it generates, for the grader (off: the grader estimates them). Costs one sensor read every 2 s while a request runs and ~10 KB" },
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
        "server.tool_allow" => c.server.tool_allow.join(" "),
        "server.tool_deny" => c.server.tool_deny.join(" "),
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
        "usage.perf" => c.usage.perf.to_string(),
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
        "strata.prompt_path" => c.strata.prompt_path.clone(),
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
        "server.tool_allow" => c.server.tool_allow = words(v),
        "server.tool_deny" => c.server.tool_deny = words(v),
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
        "usage.perf" => c.usage.perf = boolean(v)?,
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
        "strata.prompt_path" => match v {
            "off" | "1024" | "2048" | "4096" => c.strata.prompt_path = v.into(),
            _ => return Err("use off, 1024, 2048 or 4096".into()),
        },
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
/// When a change to `key` takes effect.
pub fn applies(key: &str) -> &'static str {
    if matches!(key, "server.host" | "server.port") {
        "after a server restart"
    } else if needs_reload(key) {
        "on the next load (loaded models are marked for reload)"
    } else {
        "right away"
    }
}

pub fn needs_reload(key: &str) -> bool {
    if key.ends_with(".system_prompt") || key.ends_with(".system_prompt_mode") || key.starts_with("ui.") {
        return false; // applied per request
    }
    key.starts_with("mlx.")
        || (key.starts_with("llama.") && key != "llama.git_ref")
        || matches!(key, "strata.engine" | "strata.vk_icd" | "strata.extra_args" | "strata.env" | "strata.prompt_path")
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

#[cfg(test)]
mod real_file {
    /// `cargo test edit_real_config -- --ignored --nocapture`: the diff an edit would make to
    /// your config.toml (nothing is written).
    #[test]
    #[ignore]
    fn edit_real_config() {
        let text = std::fs::read_to_string(super::config_path()).unwrap();
        let (out, _) = super::edit_text(&text, |c| { c.server.mem_reserve_gib += 0.5; Ok(()) }).unwrap();
        let out = out.unwrap();
        let (a, b): (Vec<_>, Vec<_>) = (text.lines().collect(), out.lines().collect());
        println!("lines {} -> {}", a.len(), b.len());
        for (i, (x, y)) in a.iter().zip(&b).enumerate() {
            if x != y {
                println!("{i}: - {x}\n{i}: + {y}");
            }
        }
    }
}
