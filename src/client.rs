//! Thin HTTP client for the lmdeck daemon (used by the CLI and the TUI).

use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Clone)]
pub struct Daemon {
    pub base: String,
    http: reqwest::Client,
}

impl Daemon {
    pub fn new(base: String) -> Self {
        Self {
            base,
            http: reqwest::Client::builder().connect_timeout(Duration::from_millis(500)).build().unwrap(),
        }
    }

    pub async fn status(&self) -> Option<Value> {
        let r = self.http.get(format!("{}/lmdeck/status", self.base)).timeout(Duration::from_secs(2)).send().await.ok()?;
        r.json().await.ok()
    }

    pub async fn up(&self) -> bool {
        self.status().await.is_some()
    }

    async fn post(&self, path: &str, body: Value, timeout: Duration) -> Result<Value> {
        let r = self.http.post(format!("{}{path}", self.base)).json(&body).timeout(timeout).send().await?;
        let ok = r.status().is_success();
        let v: Value = r.json().await.unwrap_or(json!({}));
        if !ok {
            bail!("{}", v.get("error").and_then(|e| e.as_str()).unwrap_or("request failed"));
        }
        Ok(v)
    }

    pub async fn load(&self, model: &str) -> Result<Value> {
        self.post("/lmdeck/load", json!({"model": model}), Duration::from_secs(3600)).await
    }
    pub async fn unload(&self, model: Option<&str>) -> Result<Value> {
        self.post("/lmdeck/unload", model.map(|m| json!({"model": m})).unwrap_or(json!({})), Duration::from_secs(30)).await
    }
    pub async fn pull(&self, model: &str) -> Result<Value> {
        self.post("/lmdeck/pull", json!({"model": model}), Duration::from_secs(30)).await
    }
    pub async fn cancel(&self, model: &str) -> Result<Value> {
        self.post("/lmdeck/cancel", json!({"model": model}), Duration::from_secs(5)).await
    }
    pub async fn reload(&self) -> Result<Value> {
        self.post("/lmdeck/reload", json!({}), Duration::from_secs(5)).await
    }
    pub async fn restart(&self, model: Option<&str>) -> Result<Value> {
        self.post("/lmdeck/restart", model.map(|m| json!({"model": m})).unwrap_or(json!({})), Duration::from_secs(3600)).await
    }
    pub async fn set_override(&self, model: &str, key: &str, value: &str) -> Result<Value> {
        self.post("/lmdeck/override", json!({"model": model, "key": key, "value": value}), Duration::from_secs(5)).await
    }
    pub async fn clear_downloads(&self) -> Result<()> {
        self.http.delete(format!("{}/lmdeck/downloads", self.base)).send().await?;
        Ok(())
    }
    pub async fn delete(&self, model: &str) -> Result<()> {
        let r = self.http.delete(format!("{}/api/delete", self.base)).json(&json!({"model": model})).send().await?;
        if !r.status().is_success() {
            let v: Value = r.json().await.unwrap_or_default();
            bail!("{}", v.get("error").and_then(|e| e.as_str()).unwrap_or("delete failed"));
        }
        Ok(())
    }
    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }
}

/// Start the daemon: via the user service when installed, else detached.
pub fn start_daemon() -> Result<()> {
    if crate::sys::service_installed() {
        let o = crate::sys::systemctl(&["start", "lmdeck"])?;
        if !o.status.success() {
            bail!("systemctl start failed: {}", String::from_utf8_lossy(&o.stderr));
        }
        return Ok(());
    }
    use std::os::unix::process::CommandExt;
    std::fs::create_dir_all(crate::config::log_dir())?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(crate::config::log_dir().join("daemon.out"))?;
    std::process::Command::new(std::env::current_exe()?)
        .arg("serve")
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .process_group(0)
        .spawn()?;
    Ok(())
}

pub fn stop_daemon(pid: Option<u32>) -> Result<()> {
    if crate::sys::service_active() {
        crate::sys::systemctl(&["stop", "lmdeck"])?;
    } else if let Some(p) = pid {
        unsafe { libc::kill(p as i32, libc::SIGTERM) };
    }
    Ok(())
}

/// SIGKILL the daemon and every backend. Under systemd the whole cgroup goes;
/// otherwise kill the daemon and each backend's process group.
pub fn force_stop(pid: Option<u32>) -> Result<()> {
    if crate::sys::service_active() {
        crate::sys::systemctl(&["kill", "-s", "KILL", "lmdeck"])?;
        // Restart=on-failure would bring it back; stop cancels the pending restart.
        crate::sys::systemctl(&["stop", "lmdeck"])?;
        return Ok(());
    }
    if let Some(p) = pid {
        unsafe { libc::kill(p as i32, libc::SIGKILL) };
    }
    // Backends run in their own process groups; sweep any left behind.
    let _ = std::process::Command::new("pkill").args(["-KILL", "-f", "lmdeck/mlx_fast_server.py|lmdeck/llama.cpp/bin/llama-server"]).status();
    Ok(())
}
