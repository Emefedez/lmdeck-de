//! Token usage accounting and a cheap change-notification counter.
//!
//! Usage lives in memory and is flushed at most every 30 s (only when it
//! changed) to `~/.local/state/lmdeck/usage.json` and, for the Omarchy bar,
//! to an agents usage record (`~/.local/state/omarchy/agents/usage/lmdeck.json`).

use crate::config::state_dir;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, SystemTime};

/// Bumped on every state change worth redrawing (load, unload, request
/// start/end, download start/finish, config change). Long-poll waiters wake.
pub struct Events {
    version: AtomicU64,
    notify: tokio::sync::Notify,
}

impl Events {
    pub fn bump(&self) {
        self.version.fetch_add(1, Ordering::Relaxed);
        self.notify.notify_waiters();
    }
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Relaxed)
    }
    /// Wait until the version differs from `since` or the timeout passes.
    pub async fn wait(&self, since: u64, timeout: Duration) {
        let notified = self.notify.notified();
        if self.version() != since {
            return;
        }
        let _ = tokio::time::timeout(timeout, notified).await;
    }
}

pub static EVENTS: LazyLock<Events> = LazyLock::new(|| Events { version: AtomicU64::new(1), notify: tokio::sync::Notify::new() });

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Counts {
    pub requests: u64,
    pub prompt: u64,
    pub completion: u64,
}

impl Counts {
    fn add(&mut self, p: u64, c: u64) {
        self.requests += 1;
        self.prompt += p;
        self.completion += c;
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Last {
    pub model: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub prompt_tps: f64,
    pub gen_tps: f64,
    pub ttft_secs: f64,
    pub at: String,
}

/// Measured speed and power of one model on this machine (the model grader reads it).
/// Moving averages over requests that generated at least `PERF_MIN_TOKENS`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Perf {
    pub runs: u64,
    pub gen_tps: f64,
    pub prompt_tps: f64,
    /// Total system power while generating (SMC sensor), 0 = not measured.
    pub watts: f64,
    pub at: String,
}

const PERF_MIN_TOKENS: u64 = 32;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UsageData {
    pub days: BTreeMap<String, Counts>,
    pub models: BTreeMap<String, Counts>,
    pub last: Option<Last>,
    pub perf: BTreeMap<String, Perf>,
}

/// The perf records as last flushed to usage.json (for the TUI, which is another process).
pub fn read_perf() -> BTreeMap<String, Perf> {
    let mut d: UsageData = std::fs::read(usage_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    seed_perf(&mut d);
    d.perf
}

/// usage.json from before the perf records still has the last request's numbers: use them.
fn seed_perf(d: &mut UsageData) {
    if !d.perf.is_empty() {
        return;
    }
    if let Some(l) = d.last.as_ref().filter(|l| l.completion_tokens >= PERF_MIN_TOKENS && l.gen_tps > 0.0) {
        d.perf.insert(l.model.clone(), Perf { runs: 1, gen_tps: l.gen_tps, prompt_tps: l.prompt_tps, watts: 0.0, at: l.at.clone() });
    }
}

/// `usage.perf`: record speed and power per model. Off by default; the daemon sets it from the
/// config on every housekeeping tick.
pub static PERF: AtomicBool = AtomicBool::new(false);

/// Power samples taken while a request is being served (the daemon's 2 s housekeeping tick,
/// only when busy and `usage.perf` is on): (time, watts). Nothing is read while lmdeck is idle.
static POWER: Mutex<std::collections::VecDeque<(std::time::Instant, f64)>> = Mutex::new(std::collections::VecDeque::new());

/// The SMC's "Total System Power" sensor (µW), found once.
fn power_sensor() -> Option<&'static PathBuf> {
    static P: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    P.get_or_init(|| {
        let dirs = std::fs::read_dir("/sys/class/hwmon").ok()?;
        for d in dirs.flatten() {
            for e in std::fs::read_dir(d.path()).ok()?.flatten() {
                let n = e.file_name().to_string_lossy().to_string();
                if n.starts_with("power") && n.ends_with("_label")
                    && std::fs::read_to_string(e.path()).is_ok_and(|l| l.trim() == "Total System Power")
                {
                    return Some(d.path().join(n.replace("_label", "_input")));
                }
            }
        }
        None
    })
    .as_ref()
}

/// `usage.perf` turned off: drop the samples.
pub fn clear_power() {
    let mut q = POWER.lock().unwrap();
    q.clear();
    q.shrink_to_fit();
}

pub fn sample_power() {
    let Some(p) = power_sensor() else { return };
    let Some(uw) = std::fs::read_to_string(p).ok().and_then(|s| s.trim().parse::<f64>().ok()) else { return };
    let mut q = POWER.lock().unwrap();
    q.push_back((std::time::Instant::now(), uw / 1e6));
    while q.len() > 600 {
        q.pop_front();
    }
}

/// Mean power over the last `secs` seconds, if it was sampled.
fn mean_power(secs: f64) -> Option<f64> {
    let q = POWER.lock().unwrap();
    let since = std::time::Instant::now().checked_sub(Duration::from_secs_f64(secs.max(0.0)))?;
    let w: Vec<f64> = q.iter().filter(|(t, _)| *t >= since).map(|s| s.1).collect();
    (!w.is_empty()).then(|| w.iter().sum::<f64>() / w.len() as f64)
}

pub struct Usage {
    data: Mutex<UsageData>,
    dirty: AtomicBool,
    /// Per model, how full the context was: the last request and the largest one (memory only).
    ctx: Mutex<BTreeMap<String, CtxFill>>,
}

/// One request's share of a model's context window: prompt + reply tokens.
#[derive(Debug, Clone, Copy)]
pub struct CtxUse {
    pub prompt: u64,
    pub completion: u64,
    pub at: SystemTime,
}

impl CtxUse {
    pub fn total(&self) -> u64 {
        self.prompt + self.completion
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CtxFill {
    pub last: CtxUse,
    pub peak: CtxUse,
}

pub static USAGE: LazyLock<Usage> = LazyLock::new(|| Usage {
    data: Mutex::new(std::fs::read(usage_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()),
    dirty: AtomicBool::new(false),
    ctx: Mutex::new(BTreeMap::new()),
});

fn usage_path() -> PathBuf {
    state_dir().join("usage.json")
}

fn record_path() -> PathBuf {
    let base = std::env::var("XDG_STATE_HOME").map(PathBuf::from).unwrap_or_else(|_| crate::config::home().join(".local/state"));
    base.join("omarchy/agents/usage/lmdeck.json")
}

impl Usage {
    pub fn record(&self, model: &str, prompt: u64, completion: u64, prompt_secs: f64, gen_secs: f64, ttft_secs: f64) {
        if prompt == 0 && completion == 0 {
            return;
        }
        // A backend that reported no prompt count says nothing about the context.
        if prompt > 0 {
            let now = CtxUse { prompt, completion, at: SystemTime::now() };
            let mut c = self.ctx.lock().unwrap();
            let e = c.entry(model.to_string()).or_insert(CtxFill { last: now, peak: now });
            e.last = now;
            if now.total() >= e.peak.total() {
                e.peak = now;
            }
        }
        let mut d = self.data.lock().unwrap();
        d.days.entry(crate::sys::local_date(0)).or_default().add(prompt, completion);
        d.models.entry(model.to_string()).or_default().add(prompt, completion);
        if PERF.load(Ordering::Relaxed) && completion >= PERF_MIN_TOKENS && gen_secs > 0.0 {
            let p = d.perf.entry(model.to_string()).or_default();
            let a = if p.runs == 0 { 1.0 } else { 0.3 };
            let ema = |old: f64, new: f64| if old > 0.0 { old + a * (new - old) } else { new };
            p.gen_tps = ema(p.gen_tps, completion as f64 / gen_secs);
            if prompt_secs > 0.0 && prompt >= PERF_MIN_TOKENS {
                p.prompt_tps = ema(p.prompt_tps, prompt as f64 / prompt_secs);
            }
            if let Some(w) = mean_power(gen_secs) {
                p.watts = ema(p.watts, w);
            }
            p.runs += 1;
            p.at = crate::sys::now_rfc3339();
        }
        d.last = Some(Last {
            model: model.to_string(),
            prompt_tokens: prompt,
            completion_tokens: completion,
            prompt_tps: if prompt_secs > 0.0 { prompt as f64 / prompt_secs } else { 0.0 },
            gen_tps: if gen_secs > 0.0 { completion as f64 / gen_secs } else { 0.0 },
            ttft_secs,
            at: crate::sys::now_rfc3339(),
        });
        // Keep a year of days at most.
        while d.days.len() > 366 {
            let k = d.days.keys().next().cloned().unwrap();
            d.days.remove(&k);
        }
        self.dirty.store(true, Ordering::Relaxed);
        drop(d);
        EVENTS.bump();
    }

    /// How full `model`'s context was on its last request, and at most, since it loaded.
    pub fn context(&self, model: &str) -> Option<CtxFill> {
        self.ctx.lock().unwrap().get(model).copied()
    }

    /// A model (re)loaded: its context starts empty.
    pub fn reset_context(&self, model: &str) {
        self.ctx.lock().unwrap().remove(model);
    }

    /// Compact summary for /lmdeck/status.
    pub fn summary(&self) -> Value {
        let d = self.data.lock().unwrap();
        let today = d.days.get(&crate::sys::local_date(0)).cloned().unwrap_or_default();
        let mut top: Vec<(&String, &Counts)> = d.models.iter().collect();
        top.sort_by(|a, b| (b.1.prompt + b.1.completion).cmp(&(a.1.prompt + a.1.completion)));
        let week: Vec<Value> = (0..7)
            .rev()
            .map(|i| {
                let k = crate::sys::local_date(i);
                let c = d.days.get(&k).cloned().unwrap_or_default();
                json!({"date": k, "tokens": c.prompt + c.completion, "requests": c.requests})
            })
            .collect();
        json!({
            "today": today,
            "week": week,
            "models": top.iter().take(6).map(|(m, c)| json!({"model": m, "requests": c.requests, "prompt": c.prompt, "completion": c.completion})).collect::<Vec<_>>(),
            "last": d.last,
        })
    }

    /// Write usage.json and the Omarchy agents record if anything changed.
    pub fn flush(&self, cfg: &crate::config::Config) {
        if !self.dirty.swap(false, Ordering::Relaxed) {
            return;
        }
        let d = self.data.lock().unwrap().clone();
        let _ = std::fs::create_dir_all(state_dir());
        if let Ok(b) = serde_json::to_vec(&d) {
            let tmp = usage_path().with_extension("json.tmp");
            if std::fs::write(&tmp, b).is_ok() {
                let _ = std::fs::rename(tmp, usage_path());
            }
        }
        if cfg.usage.omarchy_record {
            write_agents_record(&d, cfg);
        }
    }
}

/// The Omarchy agents panel picks up any record in its usage directory.
fn write_agents_record(d: &UsageData, cfg: &crate::config::Config) {
    let today_key = crate::sys::local_date(0);
    let today = d.days.get(&today_key).cloned().unwrap_or_default();
    let recent: Vec<Value> = (0..7)
        .rev()
        .map(|i| {
            let k = crate::sys::local_date(i);
            let c = d.days.get(&k).cloned().unwrap_or_default();
            json!({"date": k, "messageCount": c.prompt + c.completion})
        })
        .collect();
    let mut models = serde_json::Map::new();
    for (m, c) in &d.models {
        models.insert(
            m.clone(),
            json!({"inputTokens": c.prompt, "outputTokens": c.completion, "cacheReadInputTokens": 0, "cacheCreationInputTokens": 0}),
        );
    }
    let active: Vec<&String> = d.days.iter().filter(|(_, c)| c.requests > 0).map(|(k, _)| k).collect();
    let rec = json!({
        "schemaVersion": 1,
        "id": "lmdeck",
        "name": "lmdeck",
        "ready": true,
        "tierLabel": format!("Local · {}:{}", cfg.server.host, cfg.server.port),
        "usageStatusText": "",
        "authHelpText": "",
        "limits": [],
        "todayPrompts": today.requests,
        "todaySessions": 0,
        "todayTotalTokens": today.prompt + today.completion,
        "todayTokensByModel": {},
        "recentDays": recent,
        "totalPrompts": d.models.values().map(|c| c.requests).sum::<u64>(),
        "totalSessions": 0,
        "activeDays": active.len(),
        "activeDates": active,
        "modelUsage": models,
        "hasLocalStats": true,
        "hasPromptStats": true,
        "updatedAt": crate::sys::now_rfc3339(),
    });
    let p = record_path();
    if let Some(dir) = p.parent() {
        if !dir.exists() {
            return; // Omarchy agents not present on this machine.
        }
    }
    let tmp = p.with_extension("json.tmp");
    if std::fs::write(&tmp, serde_json::to_vec_pretty(&rec).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(tmp, p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perf_is_recorded_only_when_enabled() {
        let u = Usage { data: Mutex::new(UsageData::default()), dirty: AtomicBool::new(false), ctx: Mutex::new(BTreeMap::new()) };
        PERF.store(false, Ordering::Relaxed);
        u.record("org/m", 100, 200, 1.0, 4.0, 1.0);
        assert!(u.data.lock().unwrap().perf.is_empty(), "off: no perf record");
        PERF.store(true, Ordering::Relaxed);
        u.record("org/m", 100, 200, 1.0, 4.0, 1.0);
        assert_eq!(u.data.lock().unwrap().perf["org/m"].gen_tps, 50.0);
        PERF.store(false, Ordering::Relaxed);
    }
}
