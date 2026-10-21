//! "Will this MLX model run on the Apple GPU here?"
//!
//! Two sources of truth, no guessing from model names alone:
//! - a one-off probe of the installed omarchy-mlx runtime that tries each
//!   quantization mode's matmul + embedding gather on the GPU (cached per
//!   runtime version);
//! - the omarchy-mlx serving catalog's qualification records.
//! From those it gives a verdict and, when a model is unlikely to work or
//! untested, a recommended alternative to download instead.

use crate::config::{home, state_dir, which, Config};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::{LazyLock, Mutex};

const PROBE: &str = include_str!("compat_probe.py");

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Probe {
    /// Runtime fingerprint the probe ran against.
    pub runtime: String,
    pub mlx: String,
    /// mode -> true, or the error text.
    pub modes: BTreeMap<String, Value>,
    /// GPU working-set budget in bytes (Asahi exposes ~half of RAM to the GPU).
    #[serde(default)]
    pub gpu_budget: u64,
}

static PROBE_CACHE: LazyLock<Mutex<Option<Probe>>> = LazyLock::new(|| Mutex::new(None));
static CATALOG: LazyLock<Vec<Value>> = LazyLock::new(load_catalog);

/// Package version (or legacy wheel version) identifies the runtime build.
fn runtime_key() -> String {
    let pkg = std::process::Command::new("pacman")
        .args(["-Q", "omarchy-mlx"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    pkg.or_else(|| crate::sys::legacy_version().map(|v| format!("legacy {v}"))).unwrap_or_else(|| "unknown".into())
}

fn cache_path() -> std::path::PathBuf {
    state_dir().join("compat-probe.json")
}

/// Cached probe result; runs the probe (a few seconds, once per runtime) if needed.
pub fn probe(cfg: &Config) -> Option<Probe> {
    let key = runtime_key();
    if let Some(p) = PROBE_CACHE.lock().unwrap().as_ref() {
        if p.runtime == key && p.gpu_budget > 0 {
            return Some(p.clone());
        }
    }
    if let Some(p) = std::fs::read(cache_path()).ok().and_then(|b| serde_json::from_slice::<Probe>(&b).ok()) {
        if p.runtime == key && p.gpu_budget > 0 {
            *PROBE_CACHE.lock().unwrap() = Some(p.clone());
            return Some(p);
        }
    }
    let py = cfg.mlx.command.first().cloned().unwrap_or_else(|| "mlx-omarchy".into());
    which(&py)?;
    let script = state_dir().join("compat_probe.py");
    let _ = std::fs::create_dir_all(state_dir());
    std::fs::write(&script, PROBE).ok()?;
    let out = std::process::Command::new(&py).arg("-I").arg(&script).output().ok()?;
    let line = String::from_utf8_lossy(&out.stdout).lines().rev().find(|l| l.starts_with('{'))?.to_string();
    let v: Value = serde_json::from_str(&line).ok()?;
    let p = Probe {
        runtime: key,
        mlx: v["mlx"].as_str().unwrap_or("").to_string(),
        modes: v["modes"].as_object()?.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        gpu_budget: v["gpu_budget"].as_u64().unwrap_or(0),
    };
    let _ = std::fs::write(cache_path(), serde_json::to_vec_pretty(&p).unwrap_or_default());
    crate::sys::log(&format!("compat probe ({}): {}", p.runtime, p.modes.iter().map(|(k, v)| format!("{k}={}", v == &Value::Bool(true))).collect::<Vec<_>>().join(" ")));
    *PROBE_CACHE.lock().unwrap() = Some(p.clone());
    Some(p)
}

fn load_catalog() -> Vec<Value> {
    let mut cands = vec![home().join(".local/share/mlx-omarchy/mlx_omarchy_serve/catalog.json")];
    for root in ["/usr/lib/omarchy-mlx", "/usr/share/omarchy-mlx"] {
        find_named(std::path::Path::new(root), "catalog.json", 7, &mut cands);
    }
    // Prefer the packaged runtime's catalog over a leftover legacy copy.
    cands.reverse();
    for c in cands {
        if !c.to_string_lossy().contains("serve") {
            continue;
        }
        if let Some(m) = std::fs::read_to_string(&c).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok()).and_then(|v| v["models"].as_array().cloned()) {
            return m;
        }
    }
    vec![]
}

pub fn catalog() -> &'static [Value] {
    &CATALOG
}

pub fn find_named(dir: &std::path::Path, name: &str, depth: u32, out: &mut Vec<std::path::PathBuf>) {
    if depth == 0 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            let n = e.file_name();
            let n = n.to_string_lossy();
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

fn catalog_entry(repo: &str) -> Option<&'static Value> {
    catalog().iter().find(|c| c["repo"].as_str().map(|r| r.eq_ignore_ascii_case(repo)).unwrap_or(false))
}

fn qualified(c: &Value) -> bool {
    let q = &c["qualification"];
    q["generation"]["status"] == "qualified" && q["http"]["status"] == "qualified"
}

/// Quantization mode implied by a repo name (used before config.json is known).
pub fn mode_from_name(repo: &str) -> Option<&'static str> {
    let l = repo.to_lowercase();
    ["nvfp4", "mxfp4", "mxfp8", "nvfp8"].into_iter().find(|m| l.contains(m))
}

#[derive(Debug, Clone, Serialize)]
pub struct Verdict {
    /// "qualified" | "ok" | "untested" | "unlikely"
    pub level: &'static str,
    pub reason: String,
    /// A download that should work instead (MLX repo id), when known.
    pub recommend: Option<String>,
}

/// Verdict for an MLX repo. `mode` from config.json when known, else from the name.
pub fn assess_mlx(cfg: &Config, repo: &str, mode: Option<&str>) -> Verdict {
    let mode = mode.map(str::to_lowercase).or_else(|| mode_from_name(repo).map(str::to_string));
    let entry = catalog_entry(repo);
    let recommend = || recommend_for(repo, entry);
    if let (Some(m), Some(p)) = (mode.as_deref(), probe_cached_only()) {
        if let Some(v) = p.modes.get(m) {
            if v != &Value::Bool(true) {
                return Verdict {
                    level: "unlikely",
                    reason: format!("{m} quantization has no GPU kernels in this omarchy-mlx ({}); affine-quantized models work", p.mlx),
                    recommend: recommend(),
                };
            }
        }
    }
    let _ = cfg;
    match entry {
        Some(c) if qualified(c) => Verdict { level: "qualified", reason: "qualified by omarchy-mlx on Apple GPU".into(), recommend: None },
        Some(_) => Verdict { level: "untested", reason: "listed by omarchy-mlx but not qualified yet".into(), recommend: recommend() },
        None => Verdict { level: "ok", reason: String::new(), recommend: None },
    }
}

/// Probe result without triggering a probe run (cheap; for list rendering).
pub fn probe_cached_only() -> Option<Probe> {
    let mut cache = PROBE_CACHE.lock().unwrap();
    if let Some(p) = cache.as_ref() {
        return Some(p.clone());
    }
    let p = std::fs::read(cache_path()).ok().and_then(|b| serde_json::from_slice::<Probe>(&b).ok());
    cache.clone_from(&p);
    p
}

/// One-glyph marker for list rows.
pub fn marker(level: &str) -> &'static str {
    match level {
        "qualified" => "✓",
        "untested" => "?",
        "unlikely" => "✗",
        _ => "",
    }
}

/// A qualified catalog sibling (same family), else the affine 4-bit name of the same repo.
fn recommend_for(repo: &str, entry: Option<&Value>) -> Option<String> {
    let family = entry.and_then(|e| e["family"].as_str()).map(str::to_string).or_else(|| {
        // Infer the family from the name by dropping the quant suffix.
        let base = repo.rsplit('/').next()?.to_lowercase();
        let cut = base.rfind('-')?;
        Some(base[..cut].to_string())
    })?;
    if let Some(c) = catalog().iter().find(|c| {
        c["family"].as_str() == Some(family.as_str()) && qualified(c) && c["repo"].as_str() != Some(repo) && c["serve"]["backend"].as_str() != Some("module")
    }) {
        return c["repo"].as_str().map(str::to_string);
    }
    let (org, name) = repo.split_once('/')?;
    let cut = name.rfind('-')?;
    let alt = format!("{org}/{}-4bit", &name[..cut]);
    (alt != repo).then_some(alt)
}

/// True when an error text means "this needs a GPU kernel omarchy-mlx lacks".
pub fn is_gpu_gap(err: &str) -> bool {
    err.contains("not implemented for the Omarchy Vulkan backend") || err.contains("No GPU kernel exists")
}

/// GPU memory budget in bytes: from the MLX probe, else the Vulkan heap via
/// vulkaninfo, else half of RAM (what Asahi exposes).
pub fn gpu_budget() -> u64 {
    if let Some(b) = probe_cached_only().map(|p| p.gpu_budget).filter(|b| *b > 0) {
        return b;
    }
    crate::sys::mem().0 / 2
}

/// Estimated GPU bytes a model needs: weights + ~8% runtime + KV/workspace.
/// How an mlx-omarchy build sizes GPU buffers. Every build rounds to 4 KiB pages; above 1 MiB
/// some round further so freed buffers can be reused across neighbouring sizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufferRounding {
    /// Pages only (package 0.7.28 and builds before 2026-10-05 18:40 UTC).
    Pages,
    /// Next power of two (allocator commit b20211a0 until PR #47): can add ~30-50% to a model.
    PowerOfTwo,
    /// N size classes per power of two: upstream 8 since PR #47 (v0.7.32), lmdeck's fix 32.
    PerOctave(u64),
}

impl BufferRounding {
    pub fn round(self, size: u64) -> u64 {
        const PAGE: u64 = 4096;
        const THRESHOLD: u64 = 1 << 20;
        if size <= PAGE {
            return PAGE;
        }
        if size > THRESHOLD {
            match self {
                Self::Pages => {}
                Self::PowerOfTwo => return size.next_power_of_two(),
                Self::PerOctave(n) => {
                    let octave = 1u64 << (63 - size.leading_zeros());
                    let step = (octave / n).max(PAGE);
                    return size.div_ceil(step) * step;
                }
            }
        }
        size.div_ceil(PAGE) * PAGE
    }

    /// "rounds GPU buffers up to …" for user-facing notes; None when only pages.
    pub fn describe(self) -> Option<String> {
        match self {
            Self::Pages => None,
            Self::PowerOfTwo => Some("rounds GPU buffers up to powers of two (often +30-40% for big models)".into()),
            Self::PerOctave(n) => Some(format!("rounds GPU buffers up to {n} size classes per power of two (about +{:.0}% for big models)", 25.0 / n as f64)),
        }
    }
}

/// Bytes the MLX weights occupy on the GPU, from the safetensors headers, with each tensor
/// rounded the way the runtime's allocator sizes it.
pub fn mlx_weight_bytes(dir: &std::path::Path, rounding: BufferRounding) -> Option<u64> {
    use std::io::Read;
    let mut total = 0u64;
    let mut files = 0;
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("safetensors") {
            continue;
        }
        let mut f = std::fs::File::open(&p).ok()?;
        let mut len = [0u8; 8];
        f.read_exact(&mut len).ok()?;
        let n = u64::from_le_bytes(len);
        if n > 64 << 20 {
            return None;
        }
        let mut hdr = vec![0u8; n as usize];
        f.read_exact(&mut hdr).ok()?;
        let v: serde_json::Value = serde_json::from_slice(&hdr).ok()?;
        for (k, t) in v.as_object()? {
            // mlx_lm drops vision towers when serving text (e.g. qwen3_5.sanitize), so they
            // never reach the GPU: ~0.86 GiB on Qwen3.8-27B.
            if k == "__metadata__" || k.starts_with("vision_tower") || k.starts_with("model.visual") || k.starts_with("visual.") {
                continue;
            }
            let off = t["data_offsets"].as_array()?;
            let size = off.get(1)?.as_u64()?.saturating_sub(off.first()?.as_u64()?);
            total += rounding.round(size);
        }
        files += 1;
    }
    (files > 0).then_some(total)
}

/// The model's own maximum context (max_position_embeddings) from config.json, if declared.
pub fn mlx_model_max_ctx(dir: &std::path::Path) -> Option<u64> {
    let c: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("config.json")).ok()?).ok()?;
    let t = c.get("text_config").filter(|t| t.is_object()).unwrap_or(&c);
    t["max_position_embeddings"].as_u64().filter(|m| *m > 0)
}

/// Attention-cache bytes per token for an MLX model, from its config.json (f16 K and V on the
/// full-attention layers only; hybrid models such as Qwen3.5/3.6/3.8 keep a fixed-size state on
/// their linear-attention layers instead).
pub fn mlx_kv_per_token(dir: &std::path::Path) -> Option<u64> {
    let c: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("config.json")).ok()?).ok()?;
    let t = c.get("text_config").filter(|t| t.is_object()).unwrap_or(&c);
    let layers = t["num_hidden_layers"].as_u64()?;
    let heads = t["num_attention_heads"].as_u64().unwrap_or(1);
    let kv_heads = t["num_key_value_heads"].as_u64().unwrap_or(heads);
    let head_dim = t["head_dim"].as_u64().or_else(|| t["hidden_size"].as_u64().map(|h| h / heads.max(1)))?;
    let full = if let Some(types) = t["layer_types"].as_array() {
        types.iter().filter(|l| l.as_str().is_some_and(|s| s.contains("full") || s == "attention")).count() as u64
    } else if let Some(every) = t["full_attention_interval"].as_u64().filter(|n| *n > 0) {
        layers / every
    } else {
        layers
    };
    Some(2 * full * kv_heads * head_dim * 2)
}

/// How the MLX runtime behind `python` (or its launcher) rounds GPU buffers, from the
/// installed mlx_omarchy wheel's dev stamp (UTC).
pub fn runtime_buffer_rounding(python_or_wrapper: &str) -> BufferRounding {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, BufferRounding>>> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(v) = cache.lock().unwrap().get(python_or_wrapper) {
        return *v;
    }
    // Wrappers are `exec "<venv>/bin/python" "$@"`; follow them to the venv.
    let mut py = std::path::PathBuf::from(python_or_wrapper);
    if let Ok(text) = std::fs::read_to_string(&py) {
        if let Some(target) = text.lines().find_map(|l| l.trim().strip_prefix("exec ")).and_then(|r| r.split('"').nth(1)) {
            py = target.into();
        }
    }
    let venv = py.parent().and_then(|b| b.parent()).map(|v| v.join("lib")).unwrap_or_default();
    let wheel = std::fs::read_dir(&venv).ok().into_iter().flatten().flatten()
        .filter_map(|pyd| std::fs::read_dir(pyd.path().join("site-packages")).ok())
        .flatten().flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .find(|n| n.starts_with("mlx_omarchy-"));
    let rounding = rounding_for_wheel(wheel.as_deref().unwrap_or(""));
    cache.lock().unwrap().insert(python_or_wrapper.to_string(), rounding);
    rounding
}

fn rounding_for_wheel(name: &str) -> BufferRounding {
    let stamp = name.split(".dev").nth(1).map(|d| d.chars().take(12).collect::<String>()).and_then(|d| d.parse::<u64>().ok());
    match stamp {
        // b20211a0 (power-of-two bins) .. ad831f09 (PR #47, 8 per octave, 2026-10-09 07:27 UTC)
        Some(s) if s > 202610090727 => BufferRounding::PerOctave(8),
        Some(s) if s > 202610051840 => BufferRounding::PowerOfTwo,
        _ => BufferRounding::Pages,
    }
}

pub fn gpu_need(weights: u64, ctx_tokens: u64) -> u64 {
    weights + weights / 12 + (ctx_tokens * 96 * 1024).min(4 << 30) + (768 << 20)
}

/// "fits" (stock heap) | "tight" (needs a raised heap and free memory) | "too big" against the
/// GPU budget.
pub fn gpu_fit(weights: u64, ctx_tokens: u64) -> &'static str {
    let budget = gpu_budget() as f64;
    let need = gpu_need(weights, ctx_tokens) as f64;
    // lmdeck raises the heap per load, so the ceiling is RAM minus the desktop, not the stock 50%.
    let ceiling = crate::sys::mem().0.saturating_sub(8 << 30) as f64;
    if need <= budget * 0.80 {
        "fits"
    } else if need <= ceiling.max(budget * 0.92) {
        "tight"
    } else {
        "too big"
    }
}

/// Approximate download-time sizing; actual architecture and tensor allocation
/// are only known after download. Include context and today's free memory.
#[derive(Debug, Clone, Serialize)]
pub struct HardwareAdvice {
    pub fit: &'static str,
    pub estimated_bytes: u64,
    pub gpu_budget_bytes: u64,
    pub available_bytes: u64,
    pub context_tokens: u64,
    pub advice: String,
}

/// Download-time RAM estimate used by both discovery filters and hardware advice.
pub fn estimated_memory(cfg: &Config, weights: u64, format: &str) -> u64 {
    let context = if format == "mlx" {
        if cfg.mlx.max_kv_size > 0 { cfg.mlx.max_context.min(cfg.mlx.max_kv_size) } else { cfg.mlx.max_context }
    } else { cfg.llama.ctx_size } as u64;
    gpu_need(weights, context)
}

pub fn hardware_advice(cfg: &Config, weights: u64, format: &str) -> HardwareAdvice {
    // Reuse one snapshot across all variants in a render/search response.
    // Scanning /proc once per row would make large result lists expensive.
    type Snapshot = (std::time::Instant, u64, u64, bool);
    static MEMORY: LazyLock<Mutex<Option<Snapshot>>> = LazyLock::new(|| Mutex::new(None));
    let (total, available, vm_running) = {
        let mut cached = MEMORY.lock().unwrap();
        if cached.as_ref().is_none_or(|s| s.0.elapsed() >= std::time::Duration::from_secs(1)) {
            let (total, available) = crate::sys::mem();
            *cached = Some((std::time::Instant::now(), total, available, crate::sys::muvm_rss().is_some()));
        }
        let (_, total, available, vm) = cached.as_ref().unwrap();
        (*total, *available, *vm)
    };
    let reserve = (cfg.server.mem_reserve_gib.max(0.0) * (1u64 << 30) as f64) as u64
        + if vm_running { (cfg.server.vm_reserve_gib.max(0.0) * (1u64 << 30) as f64) as u64 } else { 0 };
    hardware_advice_for(cfg, weights, format, total, available, reserve, gpu_budget())
}

fn hardware_advice_for(cfg: &Config, weights: u64, format: &str, total: u64, available: u64, reserve: u64, stock: u64) -> HardwareAdvice {
    let context = if format == "mlx" {
        if cfg.mlx.max_kv_size > 0 { cfg.mlx.max_context.min(cfg.mlx.max_kv_size) } else { cfg.mlx.max_context }
    } else { cfg.llama.ctx_size } as u64;
    let need = estimated_memory(cfg, weights, format);
    let available = available.saturating_sub(reserve);
    let budget = match cfg.server.gpu_heap.trim().to_lowercase().as_str() {
        "stock" => stock,
        "auto" | "" => stock.max(available.min(total.saturating_sub(reserve))),
        value => value.parse::<f64>().ok().filter(|g| g.is_finite() && *g > 0.0)
            .map(|g| (g * (1u64 << 30) as f64) as u64).unwrap_or(stock),
    };
    let capacity = budget.min(available);
    let fit = if weights == 0 { "unknown" } else if need <= capacity * 80 / 100 { "fits" }
        else if need <= capacity * 92 / 100 { "tight" } else { "too big" };
    let advice = match fit {
        "unknown" => "Weight size is unavailable; check the model before loading.",
        "fits" => "Estimated to fit with room for context and runtime buffers.",
        "tight" => "Limited headroom: reduce context or choose a smaller quant.",
        _ if need <= budget * 92 / 100 => "Close apps or unload models to free memory, then check again.",
        _ if format == "gguf" && cfg.llama.auto_offload => "Choose a smaller quant, reduce context, or use partial GPU offload (slower).",
        _ => "Choose a smaller model or quant, or reduce context.",
    };
    HardwareAdvice { fit, estimated_bytes: need, gpu_budget_bytes: budget, available_bytes: available,
        context_tokens: context, advice: advice.into() }
}

pub fn recommended_choice(choices: &[crate::hf::Choice], advice: &[HardwareAdvice]) -> Option<usize> {
    choices.iter().zip(advice).enumerate().filter(|(_, (_, a))| a.fit == "fits")
        .max_by_key(|(_, (c, _))| (c.default, c.size)).map(|(i, _)| i)
}

/// Largest qualified catalog chat model that fits the GPU budget.
pub fn recommend_fitting(kind: &str) -> Option<String> {
    let budget = gpu_budget() as f64 * 0.80;
    catalog()
        .iter()
        .filter(|c| qualified(c) && c["kind"].as_str().unwrap_or("chat") == kind)
        // Module-backed entries (Bonsai, Laya) need omarchy-mlx's own servers.
        .filter(|c| c["serve"]["backend"].as_str() != Some("module"))
        .filter_map(|c| Some((c["memory"]["weights_bytes"].as_u64()?, c["repo"].as_str()?.to_string())))
        .filter(|(w, _)| (gpu_need(*w, 8192) as f64) <= budget)
        .max_by_key(|(w, _)| *w)
        .map(|(_, r)| r)
}

#[cfg(test)]
mod sizing_tests {
    #[test]
    fn kv_per_token_counts_full_attention_layers_only() {
        let d = std::env::temp_dir().join("lmdeck-kv-test");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("config.json"), r#"{"text_config": {"num_hidden_layers": 40, "num_key_value_heads": 2, "num_attention_heads": 16, "head_dim": 256, "full_attention_interval": 4}}"#).unwrap();
        assert_eq!(super::mlx_kv_per_token(&d), Some(20480)); // Qwen3.6-35B-A3B: 10 layers x 2 heads x 256 x K,V x f16
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn buffer_rounding_matches_the_allocators() {
        use super::BufferRounding::*;
        const MIB: u64 = 1 << 20;
        assert_eq!(Pages.round(1), 4096);
        assert_eq!(Pages.round(5 * MIB + 1), 5 * MIB + 4096);
        assert_eq!(PowerOfTwo.round(17 * MIB), 32 * MIB);
        assert_eq!(PerOctave(8).round(17 * MIB), 18 * MIB); // 16 MiB octave, 2 MiB steps
        assert_eq!(PerOctave(32).round(17 * MIB), 17 * MIB);
        assert_eq!(PerOctave(8).round(MIB), MIB); // at the threshold: pages
        assert_eq!(super::rounding_for_wheel("mlx_omarchy-0.32.0.dev202610050725+abc.dist-info"), Pages);
        assert_eq!(super::rounding_for_wheel("mlx_omarchy-0.32.2.dev202610071347+9b5c938.dist-info"), PowerOfTwo);
        assert_eq!(super::rounding_for_wheel("mlx_omarchy-0.32.4.dev202610160529+33630f9c.dist-info"), PerOctave(8));
    }

    #[test]
    fn rounded_weights_on_local_27b() {
        use super::BufferRounding::*;
        let home = std::env::var("HOME").unwrap();
        let Some(dir) = std::fs::read_dir(format!("{home}/.cache/huggingface/hub/models--mlx-community--Qwen3.8-27B-4bit/snapshots")).ok().and_then(|mut d| d.next()).and_then(|e| e.ok()).map(|e| e.path()) else { return };
        let w = |r| super::mlx_weight_bytes(&dir, r).unwrap();
        eprintln!("pages {:.2} / 32 {:.2} / 8 {:.2} / pow2 {:.2} GiB", w(Pages) as f64 / (1u64 << 30) as f64, w(PerOctave(32)) as f64 / (1u64 << 30) as f64, w(PerOctave(8)) as f64 / (1u64 << 30) as f64, w(PowerOfTwo) as f64 / (1u64 << 30) as f64);
        assert!(w(Pages) <= w(PerOctave(32)) && w(PerOctave(32)) <= w(PerOctave(8)) && w(PerOctave(8)) <= w(PowerOfTwo));
        assert_eq!(super::runtime_buffer_rounding("/usr/bin/mlx-omarchy"), Pages);
    }
}

#[cfg(test)]
mod hardware_tests {
    use super::*;
    const GIB: u64 = 1 << 30;
    #[test]
    fn advice_obeys_free_ram_heap_policy_and_backend_context() {
        let mut cfg = Config::default();
        cfg.server.gpu_heap = "stock".into();
        cfg.mlx.max_context = 16384;
        cfg.llama.ctx_size = 4096;
        let a = hardware_advice_for(&cfg, 4 * GIB, "mlx", 32 * GIB, 20 * GIB, 2 * GIB, 16 * GIB);
        assert_eq!(a.fit, "fits");
        assert_eq!(a.context_tokens, 16384);
        let low = hardware_advice_for(&cfg, 4 * GIB, "mlx", 32 * GIB, 6 * GIB, 2 * GIB, 16 * GIB);
        assert_eq!(low.fit, "too big");
        assert!(low.advice.contains("Close apps"));
        let g = hardware_advice_for(&cfg, 4 * GIB, "gguf", 32 * GIB, 20 * GIB, 2 * GIB, 16 * GIB);
        assert!(g.estimated_bytes < a.estimated_bytes);
        cfg.server.gpu_heap = "4".into();
        assert_eq!(hardware_advice_for(&cfg, 4 * GIB, "gguf", 32 * GIB, 20 * GIB, 2 * GIB, 16 * GIB).fit, "too big");
        cfg.server.gpu_heap = "auto".into();
        assert_eq!(hardware_advice_for(&cfg, 16 * GIB, "gguf", 64 * GIB, 40 * GIB, 2 * GIB, 16 * GIB).fit, "fits");
        assert_eq!(hardware_advice_for(&cfg, 0, "gguf", 32 * GIB, 20 * GIB, 2 * GIB, 16 * GIB).fit, "unknown");
    }
}
