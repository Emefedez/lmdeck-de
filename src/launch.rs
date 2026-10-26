//! `lmdeck launch <app>`: run coding agents against local models, like
//! `ollama launch`. Everything is per process (environment / CLI flags);
//! the agents' own config files are never rewritten. T3 Code gets a
//! dedicated provider instance in its settings instead.

use crate::client::Daemon;
use crate::config::{data_dir, home, which, Config};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::os::unix::process::CommandExt;

pub const APPS: &[&str] = &["claude", "codex", "opencode", "t3"];

/// Model to use: explicit, else the loaded one, else the first local chat model.
async fn pick_model(d: &Daemon, cfg: &Config, model: Option<String>) -> Result<String> {
    if let Some(m) = model {
        let ms = crate::models::scan(cfg);
        return Ok(crate::models::resolve(&m, &ms, cfg).map(|m| m.name.clone()).unwrap_or(m));
    }
    if let Some(s) = d.status().await {
        if let Some(n) = s["running"].as_array().and_then(|r| r.first()).and_then(|r| r["name"].as_str()) {
            return Ok(n.to_string());
        }
    }
    let ms = crate::models::scan(cfg);
    ms.iter()
        .find(|m| !m.name.to_lowercase().contains("embed"))
        .map(|m| m.name.clone())
        .context("no local models; pull one first (lmdeck pull <org/repo>)")
}

/// Environment that points Claude Code at lmdeck's Anthropic endpoint.
pub fn claude_env(cfg: &Config, model: &str) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = vec![
        ("ANTHROPIC_BASE_URL".into(), cfg.base_url()),
        ("ANTHROPIC_AUTH_TOKEN".into(), "lmdeck".into()),
        ("ANTHROPIC_DEFAULT_OPUS_MODEL".into(), model.into()),
        ("ANTHROPIC_DEFAULT_SONNET_MODEL".into(), model.into()),
        ("ANTHROPIC_DEFAULT_HAIKU_MODEL".into(), model.into()),
        ("ANTHROPIC_SMALL_FAST_MODEL".into(), model.into()),
        ("CLAUDE_CODE_SUBAGENT_MODEL".into(), model.into()),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".into(), "1".into()),
        // Local prefill of a big agent prompt can take minutes.
        ("API_TIMEOUT_MS".into(), "3000000".into()),
    ];
    v.push(("ANTHROPIC_MODEL".into(), model.into()));
    v
}

pub async fn run(cfg: &Config, d: &Daemon, app: &str, model: Option<String>, args: Vec<String>) -> Result<()> {
    if !d.up().await {
        crate::client::start_daemon()?;
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if d.up().await {
                break;
            }
        }
    }
    let model = pick_model(d, cfg, model).await?;
    if app != "t3" {
        eprintln!("lmdeck: loading {model}…");
        let v = d.load(&model).await.context("loading model")?;
        eprintln!("lmdeck: {model} ready ({:.1}s) — launching {app}", v["load_secs"].as_f64().unwrap_or(0.0));
    }
    let base = cfg.base_url();
    let err = match app {
        "claude" => {
            let bin = which("claude").context("claude not found on PATH")?;
            std::process::Command::new(bin)
                .env_remove("ANTHROPIC_API_KEY")
                .envs(claude_env(cfg, &model))
                .args(&args)
                .exec()
        }
        "codex" => {
            let bin = which("codex").context("codex not found on PATH")?;
            // Codex's OSS mode targets an Ollama server; lmdeck is one.
            std::process::Command::new(bin)
                .env("CODEX_OSS_BASE_URL", format!("{base}/v1"))
                .env("OLLAMA_HOST", &base)
                .args(["--oss", "--local-provider", "ollama", "-m", &model])
                .args(&args)
                .exec()
        }
        "opencode" => {
            let bin = which("opencode").context("opencode not found on PATH")?;
            let cfg_json = opencode_config(cfg, &model);
            std::process::Command::new(bin)
                .env("OPENCODE_CONFIG_CONTENT", cfg_json.to_string())
                .args(["-m", &format!("lmdeck/{model}")])
                .args(&args)
                .exec()
        }
        "t3" => {
            let p = register_t3(cfg, &model)?;
            eprintln!("lmdeck: T3 Code provider \"lmdeck (local)\" written to {}", p.display());
            if args.iter().any(|a| a == "--no-open") {
                // Don't interrupt running agent sessions; T3 reads it on its next start.
                eprintln!("lmdeck: restart T3 (or `systemctl --user restart t3code-backend`) to see it");
                return Ok(());
            }
            restart_t3_backend();
            let ui = which("t1code").or_else(|| which("t3code")).context("neither t1code nor t3code on PATH")?;
            std::process::Command::new(ui).args(args.iter().filter(|a| *a != "--no-open")).exec()
        }
        other => bail!("unknown app '{other}' (use one of: {})", APPS.join(", ")),
    };
    Err(err).context(format!("launching {app}"))
}

pub fn opencode_config(cfg: &Config, model: &str) -> Value {
    let models: serde_json::Map<String, Value> = crate::models::scan(cfg)
        .into_iter()
        .map(|m| (m.name.clone(), json!({"name": format!("{} (lmdeck)", m.name)})))
        .collect();
    let mut models = models;
    models.entry(model.to_string()).or_insert(json!({"name": model}));
    json!({
        "$schema": "https://opencode.ai/config.json",
        "provider": {
            "lmdeck": {
                "npm": "@ai-sdk/openai-compatible",
                "name": "lmdeck (local)",
                "options": {"baseURL": format!("{}/v1", cfg.base_url()), "apiKey": "lmdeck"},
                "models": models,
            }
        }
    })
}

fn t3_settings_path() -> std::path::PathBuf {
    home().join(".t3/userdata/settings.json")
}

/// Add/refresh a T3 Code provider instance ("lmdeck") that runs Claude Code
/// against lmdeck: the stock claudeAgent driver plus environment overrides,
/// with the local models as custom models. Other instances are untouched.
pub fn register_t3(cfg: &Config, default_model: &str) -> Result<std::path::PathBuf> {
    let p = t3_settings_path();
    let raw = std::fs::read_to_string(&p).with_context(|| format!("reading {} (is T3 Code installed and run once?)", p.display()))?;
    let mut v: Value = serde_json::from_str(&raw).context("parsing T3 settings.json")?;
    let models: Vec<String> = crate::models::scan(cfg).into_iter().map(|m| m.name).filter(|n| !n.to_lowercase().contains("embed")).collect();
    let env: Vec<Value> = claude_env(cfg, default_model)
        .into_iter()
        .filter(|(k, _)| k != "ANTHROPIC_MODEL") // T3 passes the selected model itself
        .map(|(k, val)| json!({"name": k, "value": val, "sensitive": false}))
        .collect();
    let instance = json!({
        "driver": "claudeAgent",
        "displayName": "lmdeck (local)",
        "enabled": true,
        "environment": env,
        "config": {
            "binaryPath": which("claude").map(|b| b.display().to_string()).unwrap_or_else(|| "claude".into()),
            "customModels": models,
        }
    });
    if !v["providerInstances"].is_object() {
        v["providerInstances"] = json!({});
    }
    if v["providerInstances"]["lmdeck"] == instance {
        return Ok(p);
    }
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    std::fs::copy(&p, p.with_extension(format!("json.bak.lmdeck-{ts}")))?;
    v["providerInstances"]["lmdeck"] = instance;
    let tmp = p.with_extension("json.lmdeck-tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&v)?)?;
    std::fs::rename(&tmp, &p)?;
    let _ = std::fs::create_dir_all(data_dir());
    Ok(p)
}

/// The T3 backend reads settings at start; restart it if it is running.
fn restart_t3_backend() {
    let active = std::process::Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", "t3code-backend.service"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if active {
        let _ = std::process::Command::new("systemctl").args(["--user", "restart", "t3code-backend.service"]).status();
        eprintln!("lmdeck: restarted t3code-backend.service to load the provider");
    }
}
