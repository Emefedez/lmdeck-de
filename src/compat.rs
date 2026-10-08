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
/// Bytes the MLX weights occupy on the GPU, from the safetensors headers. With `binned`,
/// each tensor over 1 MiB is rounded up to a power of two the way mlx-omarchy builds after
/// 2026-10-05 (allocator commit b20211a0) allocate it, which can add ~30% to a model.
pub fn mlx_weight_bytes(dir: &std::path::Path, binned: bool) -> Option<u64> {
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
            if k == "__metadata__" {
                continue;
            }
            let off = t["data_offsets"].as_array()?;
            let size = off.get(1)?.as_u64()?.saturating_sub(off.first()?.as_u64()?);
            total += if binned && size > 1 << 20 { size.next_power_of_two() } else { size };
        }
        files += 1;
    }
    (files > 0).then_some(total)
}

/// Whether the MLX runtime behind `python` rounds GPU buffers up to powers of two
/// (mlx-omarchy dev builds stamped after 2026-10-05 18:40 UTC).
pub fn runtime_bins_buffers(python_or_wrapper: &str) -> bool {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, bool>>> = std::sync::OnceLock::new();
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
    let stamp = std::fs::read_dir(&venv).ok().into_iter().flatten().flatten()
        .filter_map(|pyd| std::fs::read_dir(pyd.path().join("site-packages")).ok())
        .flatten().flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .find_map(|n| n.strip_prefix("mlx_omarchy-").and_then(|r| r.split(".dev").nth(1)).map(|d| d.chars().take(12).collect::<String>()))
        .and_then(|d| d.parse::<u64>().ok());
    let bins = stamp.is_some_and(|s| s > 202610051840);
    cache.lock().unwrap().insert(python_or_wrapper.to_string(), bins);
    bins
}

pub fn gpu_need(weights: u64, ctx_tokens: u64) -> u64 {
    weights + weights / 12 + (ctx_tokens * 96 * 1024).min(4 << 30) + (768 << 20)
}

/// "fits" | "tight" | "too big" against the GPU budget (with 8% headroom).
pub fn gpu_fit(weights: u64, ctx_tokens: u64) -> &'static str {
    let budget = gpu_budget() as f64;
    let need = gpu_need(weights, ctx_tokens) as f64;
    if need <= budget * 0.80 {
        "fits"
    } else if need <= budget * 0.92 {
        "tight"
    } else {
        "too big"
    }
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
    fn binned_weights_on_local_27b() {
        let home = std::env::var("HOME").unwrap();
        let Some(dir) = std::fs::read_dir(format!("{home}/.cache/huggingface/hub/models--mlx-community--Qwen3.8-27B-4bit/snapshots")).ok().and_then(|mut d| d.next()).and_then(|e| e.ok()).map(|e| e.path()) else { return };
        let plain = super::mlx_weight_bytes(&dir, false).unwrap();
        let binned = super::mlx_weight_bytes(&dir, true).unwrap();
        eprintln!("plain {:.2} GiB binned {:.2} GiB", plain as f64 / (1u64 << 30) as f64, binned as f64 / (1u64 << 30) as f64);
        assert!(binned >= plain);
        assert!(super::runtime_bins_buffers(&format!("{home}/.local/bin/mlx-omarchy")));
        assert!(!super::runtime_bins_buffers("/usr/bin/mlx-omarchy"));
    }
}
