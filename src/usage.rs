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
use std::time::Duration;

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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UsageData {
    pub days: BTreeMap<String, Counts>,
    pub models: BTreeMap<String, Counts>,
    pub last: Option<Last>,
}

pub struct Usage {
    data: Mutex<UsageData>,
    dirty: AtomicBool,
}

pub static USAGE: LazyLock<Usage> = LazyLock::new(|| Usage {
    data: Mutex::new(
        std::fs::read(usage_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default(),
    ),
    dirty: AtomicBool::new(false),
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
        let mut d = self.data.lock().unwrap();
        d.days.entry(crate::sys::local_date(0)).or_default().add(prompt, completion);
        d.models.entry(model.to_string()).or_default().add(prompt, completion);
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
