//! VS Code / Copilot integration.
//!
//! - Registers lmdeck as an "Ollama" language-model provider in VS Code's
//!   `chatLanguageModels.json`, so local models appear in Copilot's model
//!   picker (chat, inline chat, "Fix").
//! - Copilot fallback: VS Code records `chat.quotaResumeState = "blocked"`
//!   when the Copilot quota is used up. While that is the case (or always,
//!   if configured) lmdeck points `inlineChat.defaultModel` and
//!   `chat.defaultModel` at a local model. The lines it writes carry a
//!   `// set by lmdeck` marker and are removed again when quota returns;
//!   values the user set are never touched.

use crate::config::Config;
use crate::models::LocalModel;
use anyhow::Result;
use serde::Serialize;
use serde_json::{json, Value};
use std::path::PathBuf;

const MARK: &str = "// set by lmdeck";
const KEYS: [&str; 2] = ["inlineChat.defaultModel", "chat.defaultModel"];

fn user_dir() -> PathBuf {
    crate::config::config_dir_or_home().join("Code/User")
}

pub fn installed() -> bool {
    user_dir().join("settings.json").exists()
}

/// Copilot quota exhausted according to VS Code's own state. None = unknown.
pub fn quota_blocked() -> Option<bool> {
    let db = user_dir().join("globalStorage/state.vscdb");
    if !db.exists() {
        return None;
    }
    let out = std::process::Command::new("sqlite3")
        .arg("-readonly")
        .arg(&db)
        .arg("select value from ItemTable where key='chat.quotaResumeState';")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Some(v == "blocked")
}

/// Add lmdeck as an Ollama provider in chatLanguageModels.json (idempotent).
pub fn ensure_provider(cfg: &Config) -> Result<bool> {
    let p = user_dir().join("chatLanguageModels.json");
    let raw = std::fs::read_to_string(&p).unwrap_or_else(|_| "[]".into());
    let mut v: Value = serde_json::from_str(&strip_line_comments(&raw)).unwrap_or(json!([]));
    let Some(arr) = v.as_array_mut() else { return Ok(false) };
    let url = cfg.base_url();
    let entry = json!({"vendor": "ollama", "name": "lmdeck", "url": url});
    if let Some(e) = arr.iter_mut().find(|e| e["vendor"] == "ollama" && e["name"] == "lmdeck") {
        if *e == entry {
            return Ok(false);
        }
        *e = entry;
    } else {
        arr.push(entry);
    }
    backup_once(&p);
    std::fs::write(&p, serde_json::to_vec_pretty(&v)?)?;
    Ok(true)
}

/// Keep one copy of the file as it was before lmdeck first touched it.
fn backup_once(p: &std::path::Path) {
    let b = p.with_extension("json.bak.lmdeck");
    if p.exists() && !b.exists() {
        let _ = std::fs::copy(p, b);
    }
}

fn strip_line_comments(s: &str) -> String {
    s.lines().map(|l| if l.trim_start().starts_with("//") { "" } else { l }).collect::<Vec<_>>().join("\n")
}

/// Name Copilot shows for an lmdeck model (its `general.basename`).
pub fn qualified(model: &str) -> String {
    format!("{model} (ollama)")
}

/// Local model to use: configured, else the loaded one if it can call tools,
/// else the first local model with tool calling (inline chat requires it).
pub fn pick_model(cfg: &Config, models: &[LocalModel], loaded: Option<&str>) -> Option<String> {
    if !cfg.vscode.model.is_empty() {
        return Some(cfg.vscode.model.clone());
    }
    let tools = |m: &&LocalModel| m.tools;
    if let Some(l) = loaded {
        if let Some(m) = models.iter().find(|m| m.name == l).filter(tools) {
            return Some(m.name.clone());
        }
    }
    models.iter().filter(tools).filter(|m| crate::compat::gpu_fit(m.size, 8192) != "too big").min_by_key(|m| m.size).map(|m| m.name.clone())
}

/// Current lmdeck-written default (None = not active).
pub fn active_model() -> Option<String> {
    let s = std::fs::read_to_string(user_dir().join("settings.json")).ok()?;
    s.lines().find(|l| l.contains(MARK) && l.contains("inlineChat.defaultModel")).and_then(|l| {
        let v = l.split(':').skip(1).collect::<Vec<_>>().join(":");
        let v = v.split(MARK).next()?.trim().trim_end_matches(',').trim().trim_matches('"').to_string();
        Some(v.trim_end_matches(" (ollama)").to_string())
    })
}

/// Point VS Code's chat/inline defaults at `model`, or remove lmdeck's lines.
pub fn set_defaults(model: Option<&str>) -> Result<bool> {
    let p = user_dir().join("settings.json");
    let raw = std::fs::read_to_string(&p)?;
    // Drop our previous lines.
    let mut lines: Vec<String> = raw.lines().filter(|l| !l.contains(MARK)).map(str::to_string).collect();
    if let Some(m) = model {
        let user_has = |k: &str| lines.iter().any(|l| l.contains(&format!("\"{k}\"")));
        let insert: Vec<String> = KEYS
            .iter()
            .filter(|k| !user_has(k))
            .map(|k| format!("  \"{k}\": {}, {MARK} (Copilot quota exhausted)", json!(qualified(m))))
            .collect();
        if let Some(open) = lines.iter().position(|l| l.trim_start().starts_with('{')) {
            let rest = lines[open].trim_start()[1..].trim().to_string();
            if rest == "}" {
                // "{}" on one line
                lines[open] = "{".into();
                lines.insert(open + 1, "}".into());
            }
            for (i, l) in insert.into_iter().enumerate() {
                lines.insert(open + 1 + i, l);
            }
        }
    }
    let new = lines.join("\n") + if raw.ends_with('\n') { "\n" } else { "" };
    if new == raw {
        return Ok(false);
    }
    backup_once(&p);
    std::fs::write(&p, new)?;
    Ok(true)
}

#[derive(Serialize, Default)]
pub struct Status {
    pub installed: bool,
    pub mode: String,
    pub quota_blocked: Option<bool>,
    pub active_model: Option<String>,
}

pub fn status(cfg: &Config) -> Status {
    Status { installed: installed(), mode: cfg.vscode.fallback.clone(), quota_blocked: quota_blocked(), active_model: active_model() }
}

/// Reconcile VS Code with the policy. Cheap: one sqlite read + file compare.
pub fn sync(cfg: &Config, models: &[LocalModel], loaded: Option<&str>) {
    if !installed() || cfg.vscode.fallback == "off" && active_model().is_none() {
        return;
    }
    if let Err(e) = ensure_provider(cfg) {
        crate::sys::log(&format!("vscode: could not register provider: {e:#}"));
    }
    let want = match cfg.vscode.fallback.as_str() {
        "always" => true,
        "auto" => quota_blocked().unwrap_or(false),
        _ => false,
    };
    let target = if want { pick_model(cfg, models, loaded) } else { None };
    if target.as_deref() == active_model().as_deref() {
        return;
    }
    match set_defaults(target.as_deref()) {
        Ok(true) => crate::sys::log(&match &target {
            Some(m) => format!("vscode: Copilot quota exhausted -> chat and inline Fix now use {m} via lmdeck"),
            None => "vscode: Copilot defaults restored".into(),
        }),
        Ok(false) => {}
        Err(e) => crate::sys::log(&format!("vscode: could not update settings: {e:#}")),
    }
}
