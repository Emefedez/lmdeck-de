//! VS Code / Copilot integration.
//!
//! - Registers lmdeck as an "Ollama" language-model provider in VS Code's
//!   `chatLanguageModels.json`, so local models appear in Copilot's model
//!   picker (chat, inline chat, "Fix").
//! - Copilot fallback: VS Code records `chat.quotaResumeState = "blocked"`
//!   when the Copilot quota is used up. The lmdeck VS Code extension asks
//!   for this (only at startup / window focus / on request; lmdeck never
//!   polls) and switches `inlineChat.defaultModel` / `chat.defaultModel`
//!   through VS Code's settings API, restoring the previous values later.
//! - `reset_settings_lines` removes lines older lmdeck builds wrote directly.

use crate::config::Config;
use crate::models::LocalModel;
use anyhow::Result;
use serde_json::{json, Value};
use std::path::PathBuf;

const MARK: &str = "// set by lmdeck";

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

/// Model for VS Code inline chat / Fix: configured `vscode.inline_model`,
/// else `pick_model` (which checks `vscode.model`, then loaded, then smallest).
pub fn pick_inline_model(cfg: &Config, models: &[LocalModel], loaded: Option<&str>) -> Option<String> {
    if !cfg.vscode.inline_model.is_empty() {
        return Some(cfg.vscode.inline_model.clone());
    }
    pick_model(cfg, models, loaded)
}

/// Model for VS Code chat: configured `vscode.chat_model`,
/// else `pick_model` (which checks `vscode.model`, then loaded, then smallest).
pub fn pick_chat_model(cfg: &Config, models: &[LocalModel], loaded: Option<&str>) -> Option<String> {
    if !cfg.vscode.chat_model.is_empty() {
        return Some(cfg.vscode.chat_model.clone());
    }
    pick_model(cfg, models, loaded)
}

/// Remove `// set by lmdeck` lines earlier builds wrote into settings.json
/// (the extension now manages those settings through VS Code's API).
pub fn reset_settings_lines() -> Result<bool> {
    let p = user_dir().join("settings.json");
    let raw = std::fs::read_to_string(&p)?;
    if !raw.contains(MARK) {
        return Ok(false);
    }
    let new = raw.lines().filter(|l| !l.contains(MARK)).collect::<Vec<_>>().join("\n") + if raw.ends_with('\n') { "\n" } else { "" };
    std::fs::write(&p, new)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Format;

    fn test_model(name: &str, tools: bool, size: u64) -> LocalModel {
        LocalModel {
            name: name.into(),
            format: Format::Mlx,
            tools,
            size,
            repo: None,
            path: Default::default(),
            files: vec![],
            quant: String::new(),
            qmode: None,
            arch: String::new(),
            unsupported: None,
            needs_prism: false,
            params: String::new(),
            ctx_len: None,
            mmproj: None,
            modified: std::time::SystemTime::UNIX_EPOCH,
            thinking: false,
            vision: false,
        }
    }

    #[test]
    fn pick_models_honors_chat_and_inline_overrides() {
        let mut cfg = Config::default();
        let models = vec![
            test_model("fast-inline-model", true, 2 << 30),
            test_model("strata/q2_0", true, 13 << 30),
            test_model("small-no-tools", false, 1 << 30),
        ];

        // Default: falls back to smallest tool model
        assert_eq!(pick_model(&cfg, &models, None), Some("fast-inline-model".into()));
        assert_eq!(pick_inline_model(&cfg, &models, None), Some("fast-inline-model".into()));
        assert_eq!(pick_chat_model(&cfg, &models, None), Some("fast-inline-model".into()));

        // vscode.model overrides all if inline/chat are empty
        cfg.vscode.model = "default-override".into();
        assert_eq!(pick_model(&cfg, &models, None), Some("default-override".into()));
        assert_eq!(pick_inline_model(&cfg, &models, None), Some("default-override".into()));
        assert_eq!(pick_chat_model(&cfg, &models, None), Some("default-override".into()));

        // inline_model and chat_model override individually
        cfg.vscode.inline_model = "fast-inline-model".into();
        cfg.vscode.chat_model = "strata/q2_0".into();
        assert_eq!(pick_inline_model(&cfg, &models, None), Some("fast-inline-model".into()));
        assert_eq!(pick_chat_model(&cfg, &models, None), Some("strata/q2_0".into()));
        assert_eq!(pick_model(&cfg, &models, None), Some("default-override".into()));
    }
}
