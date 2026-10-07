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
    pub hooks: HooksCfg,
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
    /// Skip the memory admission check entirely.
    pub mem_check: bool,
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
    pub extra_args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LlamaCfg {
    /// Path to llama-server. Empty = lmdeck's own build, then $PATH.
    pub server_path: String,
    pub gpu_layers: i32,
    pub ctx_size: u32,
    /// "on" | "off" | "auto"
    pub flash_attn: String,
    /// 0 = let llama.cpp decide.
    pub threads: u32,
    pub parallel: u32,
    pub extra_args: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// Git ref built by "Build llama.cpp".
    pub git_ref: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HooksCfg {
    /// Shell command run before a model loads (e.g. `tuned-adm profile asahi-performance`).
    pub on_load: String,
    /// Shell command run when the last model unloads.
    pub on_idle: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelOverride {
    pub ctx_size: Option<u32>,
    pub keep_alive: Option<String>,
    pub extra_args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerCfg::default(),
            models: ModelsCfg::default(),
            download: DownloadCfg::default(),
            mlx: MlxCfg::default(),
            llama: LlamaCfg::default(),
            hooks: HooksCfg::default(),
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
            mem_check: true,
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
            batching: false,
            extra_args: vec![],
            env: BTreeMap::new(),
        }
    }
}

impl Default for LlamaCfg {
    fn default() -> Self {
        Self {
            server_path: String::new(),
            gpu_layers: 999,
            ctx_size: 8192,
            flash_attn: "auto".into(),
            threads: 0,
            parallel: 1,
            extra_args: vec![],
            env: BTreeMap::new(),
            git_ref: "master".into(),
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

pub fn config_path() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| home().join(".config")).join("lmdeck/config.toml")
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
