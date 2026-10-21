//! Model grader: ranks the local models for a goal (balanced, smartest, fastest, efficient).
//!
//! Nothing runs in the background. The view is built when it is opened, from:
//! - the model's own metadata: total and active parameters (config.json or GGUF keys) and the
//!   quantization's bits per weight, which give a size-based intelligence estimate;
//! - what lmdeck measured on this machine: decode tok/s and system power during generation
//!   (`perf` in usage.json, recorded as requests finish);
//! - free memory now (does it fit).
//!
//! `~/.config/lmdeck/grades.toml` can give any model a score you trust instead of the estimate:
//!
//! ```toml
//! [models."mlx-community/Qwen3.8-27B-4bit"]
//! intelligence = 34   # 0-100, e.g. a public benchmark index
//! ```

use crate::config::{config_path, Config};
use crate::models::{Format, LocalModel};
use crate::usage::Perf;
use std::collections::BTreeMap;

/// Goal name and weights for (intelligence, speed, efficiency).
pub const GOALS: [(&str, [f64; 3]); 4] = [
    ("Balanced", [0.5, 0.3, 0.2]),
    ("Smartest", [0.8, 0.2, 0.0]),
    ("Fastest", [0.3, 0.7, 0.0]),
    ("Efficient", [0.4, 0.1, 0.5]),
];

/// Where a number comes from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Src {
    /// Measured on this machine.
    Measured,
    /// Given in grades.toml.
    User,
    /// Estimated (size, quantization, or another model's measurement).
    Estimate,
}

#[derive(Clone, Debug)]
pub struct Row {
    pub name: String,
    pub format: Format,
    /// Total and active parameters, in billions.
    pub total_b: Option<f64>,
    pub active_b: Option<f64>,
    pub bits: Option<f64>,
    pub intelligence: Option<(f64, Src)>,
    pub tps: Option<(f64, Src)>,
    pub watts: Option<(f64, Src)>,
    /// "fits" | "tight" | "too big"
    pub fit: &'static str,
    pub score: f64,
}

impl Row {
    /// Joules per generated token.
    pub fn joules(&self) -> Option<f64> {
        match (self.watts, self.tps) {
            (Some((w, _)), Some((t, _))) if t > 0.0 => Some(w / t),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct View {
    pub goal: usize,
    pub rows: Vec<Row>,
    pub sel: usize,
}

impl View {
    pub fn set_goal(&mut self, g: usize) {
        self.goal = g.min(GOALS.len() - 1);
        rank(&mut self.rows, self.goal);
        self.sel = 0;
    }
    pub fn next_goal(&mut self, d: i32) {
        let n = GOALS.len() as i32;
        self.set_goal(((self.goal as i32 + d).rem_euclid(n)) as usize);
    }
}

pub fn build(cfg: &Config, models: &[LocalModel]) -> View {
    let perf = crate::usage::read_perf();
    let user = user_scores();
    let mut rows: Vec<Row> = models.iter().map(|m| row(cfg, m, perf.get(&m.name), user.get(&m.name).copied())).collect();
    fill_estimates(&mut rows, models);
    let mut v = View { goal: 0, rows, sel: 0 };
    v.set_goal(0);
    v
}

fn row(cfg: &Config, m: &LocalModel, perf: Option<&Perf>, user: Option<f64>) -> Row {
    let total_b = parse_b(&m.params);
    let active_b = active_params(m).or(total_b);
    let bits = bits_per_weight(m);
    let intelligence = match user {
        Some(u) => Some((u, Src::User)),
        None => match (total_b, active_b) {
            (Some(t), Some(a)) => Some((size_estimate(t, a, bits.unwrap_or(4.0)), Src::Estimate)),
            _ => None,
        },
    };
    Row {
        name: m.name.clone(),
        format: m.format,
        total_b,
        active_b,
        bits,
        intelligence,
        tps: perf.filter(|p| p.gen_tps > 0.0).map(|p| (p.gen_tps, Src::Measured)),
        watts: perf.filter(|p| p.watts > 0.0).map(|p| (p.watts, Src::Measured)),
        fit: fit(cfg, m),
        score: 0.0,
    }
}

/// Size-based intelligence estimate, 0-100: the geometric mean of total and active parameters
/// (a mixture of experts counts for less than its total), on a log scale (1000B = 100), lowered
/// for quantizations under 4 bits per weight. It cannot tell model generations or training
/// apart: put real scores in grades.toml for that.
pub fn size_estimate(total_b: f64, active_b: f64, bits: f64) -> f64 {
    let eff = (total_b * active_b.min(total_b)).sqrt();
    100.0 * quant_factor(bits) * (1.0 + eff).ln() / 1001f64.ln()
}

fn quant_factor(bits: f64) -> f64 {
    const PTS: [(f64, f64); 4] = [(1.58, 0.75), (2.0, 0.85), (3.0, 0.93), (4.0, 1.0)];
    if bits >= PTS[3].0 {
        return 1.0;
    }
    if bits <= PTS[0].0 {
        return PTS[0].1;
    }
    let i = PTS.iter().position(|p| p.0 >= bits).unwrap_or(3).max(1);
    let (a, b) = (PTS[i - 1], PTS[i]);
    a.1 + (b.1 - a.1) * (bits - a.0) / (b.0 - a.0)
}

/// Decode on this GPU is bound by the weights read per token plus a fixed cost per token:
/// tok/s = 1 / (active bytes / bandwidth + overhead). The defaults fit the M1 Max runs in
/// docs/RESULTS.md (0.6B 154, 4B 52, 27B 11-13 tok/s); measured models replace the bandwidth.
const DEFAULT_GBS: f64 = 190.0;
const OVERHEAD_S: f64 = 0.0047;

/// Speed and power for models lmdeck has not measured yet:
/// - tok/s from the model above, its bandwidth calibrated on the measured MLX/GGUF models;
/// - system power while generating is about the same for every GPU-bound model, so the median
///   measured watts stand in.
///
/// Strata streams experts from the SSD, so it only gets measured numbers.
fn fill_estimates(rows: &mut [Row], models: &[LocalModel]) {
    let active_bytes = |r: &Row, m: &LocalModel| -> Option<f64> {
        let (t, a) = (r.total_b?, r.active_b?);
        (t > 0.0 && m.size > 0).then(|| m.size as f64 * (a / t).min(1.0))
    };
    let mut bw: Vec<f64> = rows
        .iter()
        .zip(models)
        .filter(|(r, _)| r.format != Format::Strata)
        .filter_map(|(r, m)| {
            let per_token = 1.0 / r.tps.filter(|t| t.1 == Src::Measured)?.0 - OVERHEAD_S;
            (per_token > 0.0).then(|| active_bytes(r, m).map(|b| b / per_token / 1e9)).flatten()
        })
        .collect();
    let mut watts: Vec<f64> = rows.iter().filter_map(|r| r.watts.map(|w| w.0)).collect();
    let gbs = median(&mut bw).unwrap_or(DEFAULT_GBS);
    let watts = median(&mut watts);
    for (r, m) in rows.iter_mut().zip(models) {
        if r.tps.is_none() && r.format != Format::Strata {
            if let Some(b) = active_bytes(r, m) {
                r.tps = Some((1.0 / (b / (gbs * 1e9) + OVERHEAD_S), Src::Estimate));
            }
        }
        if r.watts.is_none() && r.tps.is_some() {
            r.watts = watts.map(|w| (w, Src::Estimate));
        }
    }
}

fn median(v: &mut [f64]) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    Some(v[v.len() / 2])
}

/// Score = weighted geometric mean of each metric relative to the best row. Speed counts with
/// diminishing returns (ln(1 + tok/s / 5): past reading speed, more matters less). A missing
/// metric counts as half the best; rows that do not fit now go last.
fn rank(rows: &mut [Row], goal: usize) {
    let w = GOALS[goal].1;
    let best = |f: &dyn Fn(&Row) -> Option<f64>| rows.iter().filter_map(f).fold(0.0f64, f64::max);
    let speed = |r: &Row| r.tps.map(|t| (1.0 + t.0 / 5.0).ln());
    let metrics: [&dyn Fn(&Row) -> Option<f64>; 3] = [
        &|r| r.intelligence.map(|i| i.0),
        &speed,
        &|r| r.joules().map(|j| 1.0 / j),
    ];
    let bests: Vec<f64> = metrics.iter().map(|f| best(*f)).collect();
    for r in rows.iter_mut() {
        let mut log = 0.0;
        for (i, f) in metrics.iter().enumerate() {
            let rel = f(r).filter(|v| *v > 0.0 && bests[i] > 0.0).map_or(0.5, |v| v / bests[i]);
            log += w[i] * rel.ln();
        }
        r.score = 100.0 * log.exp();
    }
    let order = |r: &Row| match r.fit { "fits" => 0, "tight" => 1, _ => 2 };
    rows.sort_by(|a, b| order(a).cmp(&order(b)).then(b.score.total_cmp(&a.score)));
}

fn fit(cfg: &Config, m: &LocalModel) -> &'static str {
    match m.format {
        Format::Strata => {
            let free = crate::sys::mem().1.saturating_sub((cfg.server.mem_reserve_gib * (1u64 << 30) as f64) as u64);
            let floor = crate::backend::STRATA_FLOOR;
            if free >= floor + (4 << 30) { "fits" } else if free >= floor { "tight" } else { "too big" }
        }
        f => crate::compat::hardware_advice(cfg, m.size, if f == Format::Mlx { "mlx" } else { "gguf" }).fit,
    }
}

/// "27B" / "0.6B" / "1.5T" -> billions.
fn parse_b(s: &str) -> Option<f64> {
    let s = s.trim();
    let (num, mul) = match s.chars().last()? {
        'B' | 'b' => (&s[..s.len() - 1], 1.0),
        'T' | 't' => (&s[..s.len() - 1], 1000.0),
        'M' | 'm' => (&s[..s.len() - 1], 0.001),
        _ => return None,
    };
    num.parse::<f64>().ok().map(|n| n * mul).filter(|n| *n > 0.0)
}

/// Active parameters (billions) of a mixture of experts: "A3B" in the name, else the expert
/// shapes in config.json (MLX) or the GGUF metadata (GGUF, Strata's native file).
fn active_params(m: &LocalModel) -> Option<f64> {
    if let Some(a) = active_from_name(&m.name) {
        return Some(a);
    }
    let total = parse_b(&m.params)?;
    let moe = match m.format {
        Format::Mlx => moe_from_config(&m.path.join("config.json")),
        Format::Gguf => moe_from_gguf(&m.path),
        Format::Strata => strata_native(m).and_then(|p| moe_from_gguf(&p)),
    }?;
    let expert = |k: f64| moe.layers * k * 3.0 * moe.hidden * moe.ffn / 1e9;
    Some((total - expert(moe.experts) + expert(moe.used)).clamp(total * 0.02, total))
}

fn active_from_name(name: &str) -> Option<f64> {
    let tail = name.rsplit('/').next()?;
    tail.split(['-', '_', '.', ':']).find_map(|part| {
        let p = part.strip_prefix('A').or_else(|| part.strip_prefix('a'))?;
        parse_b(p).filter(|_| p.ends_with(['B', 'b']))
    })
}

struct Moe {
    layers: f64,
    hidden: f64,
    ffn: f64,
    experts: f64,
    used: f64,
}

fn moe_from_config(path: &std::path::Path) -> Option<Moe> {
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    let c = if v.get("text_config").is_some_and(|t| t.is_object()) { &v["text_config"] } else { &v };
    let num = |keys: &[&str]| keys.iter().find_map(|k| c[*k].as_f64());
    Some(Moe {
        layers: num(&["num_hidden_layers", "n_layer"])?,
        hidden: num(&["hidden_size", "d_model"])?,
        ffn: num(&["moe_intermediate_size", "expert_intermediate_size"])?,
        experts: num(&["num_experts", "num_local_experts", "n_routed_experts"])?,
        used: num(&["num_experts_per_tok", "num_experts_per_token", "top_k"])?,
    })
}

fn moe_from_gguf(path: &std::path::Path) -> Option<Moe> {
    let k = crate::backend::gguf_u32s(path, &[".block_count", ".embedding_length", ".expert_feed_forward_length", ".expert_count", ".expert_used_count"]);
    let f = |i: usize| k.get(i).copied().flatten().map(f64::from);
    Some(Moe { layers: f(0)?, hidden: f(1)?, ffn: f(2)?, experts: f(3)?, used: f(4)? })
}

/// The `--native` GGUF of a Strata run config.
fn strata_native(m: &LocalModel) -> Option<std::path::PathBuf> {
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&m.path).ok()?).ok()?;
    let args = v["args"].as_array()?;
    let i = args.iter().position(|a| a == "--native")?;
    args.get(i + 1)?.as_str().map(std::path::PathBuf::from)
}

/// Bits per weight from the quantization label (4bit, Q4_K_M, IQ3_S, Q2_0, bf16, ternary).
fn bits_per_weight(m: &LocalModel) -> Option<f64> {
    let q = m.quant.to_ascii_lowercase();
    let name = m.name.to_ascii_lowercase();
    if name.contains("ternary") || q.contains("ternary") || q.contains("1.58") {
        return Some(1.58);
    }
    if matches!(q.as_str(), "bf16" | "f16" | "fp16") {
        return Some(16.0);
    }
    if q == "f32" {
        return Some(32.0);
    }
    if let Some(n) = q.strip_suffix("bit").and_then(|n| n.parse::<f64>().ok()) {
        return Some(n);
    }
    let digits: String = q.trim_start_matches('i').trim_start_matches('q').chars().take_while(|c| c.is_ascii_digit()).collect();
    let base = digits.parse::<f64>().ok()?;
    // K-quants carry scales on top of their nominal bits.
    Some(if q.contains("_k") { base + 0.5 } else { base })
}

fn user_scores() -> BTreeMap<String, f64> {
    #[derive(serde::Deserialize, Default)]
    struct File {
        #[serde(default)]
        models: BTreeMap<String, Entry>,
    }
    #[derive(serde::Deserialize)]
    struct Entry {
        intelligence: f64,
    }
    let text = std::fs::read_to_string(config_path().with_file_name("grades.toml")).unwrap_or_default();
    let f: File = toml::from_str(&text).unwrap_or_default();
    f.models.into_iter().map(|(k, e)| (k, e.intelligence)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(name: &str, quant: &str) -> LocalModel {
        LocalModel {
            name: name.into(),
            format: Format::Mlx,
            repo: None,
            path: Default::default(),
            files: vec![],
            size: 0,
            quant: quant.into(),
            qmode: None,
            arch: String::new(),
            params: String::new(),
            ctx_len: None,
            mmproj: None,
            modified: std::time::SystemTime::UNIX_EPOCH,
            tools: false,
            thinking: false,
            vision: false,
        }
    }

    #[test]
    fn size_estimate_orders_by_size_and_bits() {
        let dense27 = size_estimate(27.0, 27.0, 4.0);
        let dense4 = size_estimate(4.0, 4.0, 4.0);
        let tiny = size_estimate(0.6, 0.6, 4.0);
        assert!(dense27 > dense4 && dense4 > tiny);
        // A 2-bit quant of the same model scores lower; a MoE counts for less than its total.
        assert!(size_estimate(27.0, 27.0, 2.0) < dense27);
        assert!(size_estimate(35.0, 3.0, 4.0) < size_estimate(35.0, 35.0, 4.0));
        assert!((0.0..=100.0).contains(&size_estimate(1000.0, 1000.0, 16.0)));
    }

    #[test]
    fn names_and_quant_labels() {
        assert_eq!(parse_b("0.6B"), Some(0.6));
        assert_eq!(parse_b("125B"), Some(125.0));
        assert_eq!(active_from_name("mlx-community/Qwen3.6-35B-A3B-4bit"), Some(3.0));
        assert_eq!(active_from_name("mlx-community/Qwen3.8-27B-4bit"), None);
        let mut m = model("x/y", "Q4_K_M");
        assert_eq!(bits_per_weight(&m), Some(4.5));
        m.quant = "4bit".into();
        assert_eq!(bits_per_weight(&m), Some(4.0));
        m.quant = "Q2_0".into();
        assert_eq!(bits_per_weight(&m), Some(2.0));
        m.quant = "bf16".into();
        m.name = "x/Qwen-27B-ternary".into();
        assert_eq!(bits_per_weight(&m), Some(1.58));
    }

    #[test]
    fn ranking_uses_goal_weights_and_puts_misfits_last() {
        let r = |name: &str, i: f64, t: f64, fit: &'static str| Row {
            name: name.into(),
            format: Format::Mlx,
            total_b: None,
            active_b: None,
            bits: None,
            intelligence: Some((i, Src::Estimate)),
            tps: Some((t, Src::Measured)),
            watts: Some((30.0, Src::Measured)),
            fit,
            score: 0.0,
        };
        let mut rows = vec![r("small", 20.0, 100.0, "fits"), r("big", 50.0, 10.0, "fits"), r("huge", 60.0, 5.0, "too big")];
        rank(&mut rows, 1); // smartest
        assert_eq!(rows[0].name, "big");
        assert_eq!(rows[2].name, "huge");
        rank(&mut rows, 2); // fastest
        assert_eq!(rows[0].name, "small");
    }
}
