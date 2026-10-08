//! HTTP front door: Ollama API (`/api/*`), OpenAI API (`/v1/*`, proxied),
//! and lmdeck control endpoints (`/lmdeck/*`).

use crate::backend::{InFlight, Manager};
use crate::config::Config;
use crate::hf::{self, Downloads};
use crate::models::{Format, LocalModel};
use crate::sys::{self, rfc3339};
use crate::usage::{EVENTS, USAGE};
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, delete, get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

pub const OLLAMA_COMPAT_VERSION: &str = "0.12.6";

#[derive(Clone)]
pub struct App {
    pub mgr: Arc<Manager>,
    pub dl: Downloads,
    pub hf: reqwest::Client,
    pub started: Instant,
    /// Fallback model the VS Code extension last reported as applied.
    pub vscode_active: Arc<std::sync::Mutex<Option<String>>>,
}

pub async fn run(cfg: Config) -> anyhow::Result<()> {
    let addr = format!("{}:{}", cfg.server.host, cfg.server.port);
    let app = App {
        mgr: Manager::new(cfg),
        dl: Downloads::default(),
        hf: hf::client(),
        started: Instant::now(),
        vscode_active: Arc::new(std::sync::Mutex::new(
            std::fs::read_to_string(crate::config::state_dir().join("vscode-active")).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
        )),
    };

    // Probe GPU support for quantization modes once per runtime (a few seconds).
    let probe_cfg = app.mgr.cfg();
    tokio::task::spawn_blocking(move || crate::compat::probe(&probe_cfg));

    let flusher = app.mgr.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            let cfg = flusher.cfg();
            let _ = tokio::task::spawn_blocking(move || USAGE.flush(&cfg)).await;
        }
    });

    let reaper = app.mgr.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            reaper.reap().await;
        }
    });

    let router = Router::new()
        .route("/", get(root).head(root))
        .route("/api/version", get(version))
        .route("/api/tags", get(tags))
        .route("/api/ps", get(ps))
        .route("/api/show", post(show))
        .route("/api/chat", post(chat))
        .route("/api/generate", post(generate))
        .route("/api/pull", post(pull))
        .route("/api/delete", delete(delete_model))
        .route("/api/copy", post(copy))
        .route("/api/embed", post(embed))
        .route("/api/embeddings", post(embeddings_legacy))
        .route("/v1/models", get(v1_models))
        .route("/v1/models/{*id}", get(v1_model))
        .route("/v1/messages", post(anthropic_messages))
        .route("/v1/messages/count_tokens", post(anthropic_count))
        .route("/v1/{*rest}", any(v1_proxy))
        .route("/lmdeck/status", get(status))
        .route("/lmdeck/models", get(lm_models))
        .route("/lmdeck/load", post(lm_load))
        .route("/lmdeck/unload", post(lm_unload))
        .route("/lmdeck/pull", post(lm_pull))
        .route("/lmdeck/cancel", post(lm_cancel))
        .route("/lmdeck/downloads", get(lm_downloads).delete(lm_clear_downloads))
        .route("/lmdeck/reload", post(lm_reload))
        .route("/lmdeck/logs", get(lm_logs))
        .route("/lmdeck/watch", get(lm_watch))
        .route("/lmdeck/config", get(lm_config_get).post(lm_config_set))
        .route("/lmdeck/search", get(lm_search))
        .route("/lmdeck/repo", get(lm_repo))
        .route("/lmdeck/restart", post(lm_restart))
        .route("/lmdeck/override", post(lm_override))
        .route("/lmdeck/compat", get(lm_compat))
        .route("/lmdeck/plan", get(lm_plan))
        .route("/lmdeck/vscode", get(lm_vscode))
        .route("/lmdeck/vscode/state", post(lm_vscode_state))
        .layer(axum::middleware::from_fn(log_requests))
        .with_state(app.clone());

    let listener = tokio::net::TcpListener::bind(&addr).await.map_err(|e| anyhow::anyhow!("binding {addr}: {e} (is Ollama or another lmdeck running?)"))?;
    sys::log(&format!("lmdeck {} listening on http://{addr}", env!("CARGO_PKG_VERSION")));
    let mgr = app.mgr.clone();
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = term.recv() => {},
            }
            sys::log("shutting down, unloading models");
            mgr.unload_all().await;
            USAGE.flush(&mgr.cfg());
        })
        .await?;
    Ok(())
}

// ---------- helpers ----------

/// One line per API call in lmdeck.log (control/status polling is skipped).
async fn log_requests(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let path = req.uri().path().to_string();
    let method = req.method().clone();
    let quiet = path.starts_with("/lmdeck/") || path == "/" || path == "/api/version" || path == "/api/ps" || path == "/api/tags";
    let t0 = Instant::now();
    let resp = next.run(req).await;
    if !quiet {
        sys::log(&format!("{method} {path} -> {} ({:.0} ms to headers)", resp.status().as_u16(), t0.elapsed().as_secs_f64() * 1000.0));
    }
    resp
}

/// JSON body extractor that ignores Content-Type (Ollama accepts bare `curl -d`).
pub struct AnyJson(pub Value);

impl<S: Send + Sync> axum::extract::FromRequest<S> for AnyJson {
    type Rejection = Response;
    async fn from_request(req: axum::extract::Request, state: &S) -> Result<Self, Self::Rejection> {
        let b = Bytes::from_request(req, state).await.map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
        if b.is_empty() {
            return Ok(AnyJson(json!({})));
        }
        serde_json::from_slice(&b).map(AnyJson).map_err(|e| err(StatusCode::BAD_REQUEST, format!("invalid JSON: {e}")))
    }
}

type Resp = Response;

fn err(code: StatusCode, msg: impl std::fmt::Display) -> Resp {
    (code, Json(json!({"error": msg.to_string()}))).into_response()
}

fn not_found_or_500(e: anyhow::Error) -> Resp {
    let s = format!("{e:#}");
    if s.contains("not found") {
        err(StatusCode::NOT_FOUND, s)
    } else {
        err(StatusCode::INTERNAL_SERVER_ERROR, s)
    }
}

fn model_field(v: &Value) -> Option<String> {
    v.get("model").or_else(|| v.get("name")).and_then(|m| m.as_str()).map(str::to_string)
}

fn keep_alive_field(v: &Value) -> Option<String> {
    match v.get("keep_alive")? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn details(m: &LocalModel) -> Value {
    json!({
        "parent_model": "",
        "format": if m.format == Format::Gguf { "gguf" } else { "safetensors" },
        "family": m.family(),
        "families": [m.family()],
        "parameter_size": m.params,
        "quantization_level": m.quant,
    })
}

fn tag_entry(m: &LocalModel) -> Value {
    json!({
        "name": m.name,
        "model": m.name,
        "modified_at": rfc3339(m.modified),
        "size": m.size,
        "digest": m.digest(),
        "details": details(m),
    })
}

fn ndjson_stream(rx: mpsc::Receiver<Value>) -> Resp {
    let s = ReceiverStream::new(rx).map(|v| {
        let mut b = serde_json::to_vec(&v).unwrap();
        b.push(b'\n');
        Ok::<_, std::convert::Infallible>(Bytes::from(b))
    });
    let mut r = Response::new(Body::from_stream(s));
    r.headers_mut().insert("content-type", HeaderValue::from_static("application/x-ndjson"));
    r
}

// ---------- basic endpoints ----------

async fn root() -> &'static str {
    "Ollama is running (lmdeck)"
}

async fn version() -> Json<Value> {
    Json(json!({"version": OLLAMA_COMPAT_VERSION, "lmdeck": env!("CARGO_PKG_VERSION")}))
}

async fn tags(State(a): State<App>) -> Json<Value> {
    a.mgr.invalidate();
    Json(json!({"models": a.mgr.models().iter().map(tag_entry).collect::<Vec<_>>()}))
}

async fn ps(State(a): State<App>) -> Json<Value> {
    let models: Vec<Value> = a
        .mgr
        .running()
        .iter()
        .map(|r| {
            let exp = r.expires_in().map(|d| std::time::SystemTime::now() + d);
            let mut e = tag_entry(&r.model);
            e["expires_at"] = json!(exp.map(rfc3339).unwrap_or_else(|| "2318-01-01T00:00:00Z".into()));
            e["size_vram"] = json!(r.model.size); // unified memory: all of it is "GPU"
            e["context_length"] = json!(r.model.ctx_len);
            e
        })
        .collect();
    Json(json!({"models": models}))
}

async fn show(State(a): State<App>, AnyJson(b): AnyJson) -> Resp {
    let Some(name) = model_field(&b) else { return err(StatusCode::BAD_REQUEST, "model is required") };
    let m = match a.mgr.resolve(&name) {
        Ok(m) => m,
        Err(e) => return not_found_or_500(e),
    };
    let arch = m.family();
    let mut info = json!({
        "general.architecture": arch,
        // Copilot's Ollama provider names models after general.basename;
        // the full name keeps quants of one repo apart and matches lmdeck's id.
        "general.basename": m.name.clone(),
        "general.size_label": m.params,
    });
    let cfg = a.mgr.cfg();
    let ctx = match m.format {
        Format::Mlx => m.ctx_len.map(|c| c.min(cfg.mlx.max_context as u64)).or(Some(cfg.mlx.max_context as u64)),
        Format::Gguf => Some(cfg.overrides.get(&m.name).and_then(|o| o.ctx_size).unwrap_or(cfg.llama.ctx_size) as u64),
    };
    if let Some(c) = ctx {
        info[format!("{arch}.context_length")] = json!(c);
    }
    Json(json!({
        "license": "",
        "modelfile": format!("# served by lmdeck\nFROM {}\n", m.path.display()),
        "parameters": "",
        "template": "{{ .Prompt }}",
        "details": details(&m),
        "model_info": info,
        "capabilities": m.capabilities(),
        "modified_at": rfc3339(m.modified),
        "lmdeck": {"format": m.format.as_str(), "path": m.path, "size": m.size},
    }))
    .into_response()
}

async fn copy(State(a): State<App>, AnyJson(b): AnyJson) -> Resp {
    let (Some(src), Some(dst)) = (b.get("source").and_then(|v| v.as_str()), b.get("destination").and_then(|v| v.as_str())) else {
        return err(StatusCode::BAD_REQUEST, "source and destination are required");
    };
    let m = match a.mgr.resolve(src) {
        Ok(m) => m,
        Err(e) => return not_found_or_500(e),
    };
    let mut cfg = a.mgr.cfg();
    cfg.aliases.insert(crate::models::normalize(dst), m.name);
    if let Err(e) = cfg.save() {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e);
    }
    *a.mgr.cfg.write().unwrap() = cfg;
    StatusCode::OK.into_response()
}

async fn delete_model(State(a): State<App>, AnyJson(b): AnyJson) -> Resp {
    let Some(name) = model_field(&b) else { return err(StatusCode::BAD_REQUEST, "model is required") };
    let m = match a.mgr.resolve(&name) {
        Ok(m) => m,
        Err(e) => return not_found_or_500(e),
    };
    let _ = a.mgr.unload(&m.name).await;
    sys::log(&format!("deleting {} ({} bytes)", m.name, m.size));
    let cfg = a.mgr.cfg();
    let res = tokio::task::spawn_blocking(move || crate::models::delete(&m, &cfg)).await;
    a.mgr.invalidate();
    match res {
        Ok(Ok(())) => StatusCode::OK.into_response(),
        Ok(Err(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

// ---------- pull ----------

async fn pull(State(a): State<App>, AnyJson(b): AnyJson) -> Resp {
    let Some(name) = model_field(&b) else { return err(StatusCode::BAD_REQUEST, "model is required") };
    let stream = b.get("stream").and_then(|v| v.as_bool()).unwrap_or(true);
    sys::log(&format!("pull requested: {name}"));
    let d = a.dl.start(a.hf.clone(), a.mgr.cfg(), &name);
    let mgr = a.mgr.clone();
    if !stream {
        while !d.finished() {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        mgr.invalidate();
        let v = d.view();
        return match v.status.get("state").and_then(|s| s.as_str()) {
            Some("done") => Json(json!({"status": "success"})).into_response(),
            _ => err(StatusCode::INTERNAL_SERVER_ERROR, v.status.get("detail").cloned().unwrap_or(json!("pull failed"))),
        };
    }
    let (tx, rx) = mpsc::channel(16);
    tokio::spawn(async move {
        let _ = tx.send(json!({"status": "pulling manifest"})).await;
        loop {
            let v = d.view();
            let state = v.status.get("state").and_then(|s| s.as_str()).unwrap_or("").to_string();
            match state.as_str() {
                "done" => {
                    mgr.invalidate();
                    let _ = tx.send(json!({"status": "verifying sha256 digest"})).await;
                    let _ = tx.send(json!({"status": "writing manifest"})).await;
                    let _ = tx.send(json!({"status": "success"})).await;
                    break;
                }
                "failed" | "cancelled" => {
                    let _ = tx.send(json!({"error": v.status.get("detail").cloned().unwrap_or(json!(state))})).await;
                    break;
                }
                "resolving" => {}
                _ => {
                    let msg = json!({
                        "status": format!("pulling {}", v.file),
                        "digest": format!("sha256:{}", v.name),
                        "total": v.total,
                        "completed": v.completed,
                    });
                    if tx.send(msg).await.is_err() {
                        break; // client went away; the download continues in the background
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
    });
    ndjson_stream(rx)
}

// ---------- chat / generate ----------

fn image_url(b64: &str) -> String {
    let mime = if b64.starts_with("/9j/") {
        "image/jpeg"
    } else if b64.starts_with("R0lG") {
        "image/gif"
    } else if b64.starts_with("UklG") {
        "image/webp"
    } else {
        "image/png"
    };
    format!("data:{mime};base64,{b64}")
}

fn convert_messages(msgs: &[Value]) -> Vec<Value> {
    let mut out = Vec::with_capacity(msgs.len());
    let mut pending_ids: std::collections::VecDeque<String> = Default::default();
    let mut call_n = 0usize;
    for m in msgs {
        let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("user");
        let content = m.get("content").cloned().unwrap_or(json!(""));
        let mut o = json!({"role": role});
        let images: Vec<&str> = m.get("images").and_then(|i| i.as_array()).map(|a| a.iter().filter_map(|x| x.as_str()).collect()).unwrap_or_default();
        if images.is_empty() {
            o["content"] = content;
        } else {
            let mut parts = vec![json!({"type": "text", "text": content.as_str().unwrap_or("")})];
            parts.extend(images.iter().map(|b| json!({"type": "image_url", "image_url": {"url": image_url(b)}})));
            o["content"] = json!(parts);
        }
        if let Some(tcs) = m.get("tool_calls").and_then(|t| t.as_array()) {
            let calls: Vec<Value> = tcs
                .iter()
                .map(|tc| {
                    let f = tc.get("function").cloned().unwrap_or_default();
                    let id = tc.get("id").and_then(|i| i.as_str()).map(str::to_string).unwrap_or_else(|| {
                        call_n += 1;
                        format!("call_{call_n}")
                    });
                    pending_ids.push_back(id.clone());
                    let args = match f.get("arguments") {
                        Some(Value::String(s)) => s.clone(),
                        Some(v) => v.to_string(),
                        None => "{}".into(),
                    };
                    json!({"id": id, "type": "function", "function": {"name": f.get("name").cloned().unwrap_or(json!("")), "arguments": args}})
                })
                .collect();
            o["tool_calls"] = json!(calls);
            if o["content"].as_str() == Some("") {
                o["content"] = Value::Null;
            }
        }
        if role == "tool" {
            let id = m
                .get("tool_call_id")
                .and_then(|i| i.as_str())
                .map(str::to_string)
                .or_else(|| pending_ids.pop_front())
                .unwrap_or_else(|| "call_0".into());
            o["tool_call_id"] = json!(id);
            if let Some(n) = m.get("tool_name").or_else(|| m.get("name")) {
                o["name"] = n.clone();
            }
        }
        out.push(o);
    }
    out
}

/// Build an OpenAI chat request from an Ollama chat/generate body.
fn openai_body(b: &Value, messages: Vec<Value>, model_id: &str) -> Value {
    let mut o = json!({
        "model": model_id,
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    let opts = b.get("options").cloned().unwrap_or_default();
    let copy = |o: &mut Value, from: &str, to: &str| {
        if let Some(v) = opts.get(from) {
            if !v.is_null() {
                o[to] = v.clone();
            }
        }
    };
    copy(&mut o, "temperature", "temperature");
    copy(&mut o, "top_p", "top_p");
    copy(&mut o, "top_k", "top_k");
    copy(&mut o, "min_p", "min_p");
    copy(&mut o, "typical_p", "typical_p");
    copy(&mut o, "seed", "seed");
    copy(&mut o, "stop", "stop");
    copy(&mut o, "presence_penalty", "presence_penalty");
    copy(&mut o, "frequency_penalty", "frequency_penalty");
    copy(&mut o, "repeat_penalty", "repeat_penalty");
    copy(&mut o, "repeat_penalty", "repetition_penalty");
    copy(&mut o, "repeat_last_n", "repeat_last_n");
    if let Some(n) = opts.get("num_predict").and_then(|v| v.as_i64()) {
        if n > 0 {
            o["max_tokens"] = json!(n);
        }
    }
    match b.get("format") {
        Some(Value::String(s)) if s == "json" => o["response_format"] = json!({"type": "json_object"}),
        Some(v @ Value::Object(_)) => o["response_format"] = json!({"type": "json_schema", "json_schema": {"name": "response", "schema": v, "strict": true}}),
        _ => {}
    }
    if let Some(t) = b.get("tools").filter(|t| t.as_array().map(|a| !a.is_empty()).unwrap_or(false)) {
        o["tools"] = t.clone();
    }
    match b.get("think") {
        Some(Value::Bool(false)) => o["chat_template_kwargs"] = json!({"enable_thinking": false}),
        Some(Value::Bool(true)) => o["chat_template_kwargs"] = json!({"enable_thinking": true}),
        Some(Value::String(level)) => o["reasoning_effort"] = json!(level),
        _ => {}
    }
    o
}

#[derive(Default)]
struct Acc {
    content: String,
    thinking: String,
    tools: Vec<(String, String, String)>, // id, name, args
    finish: Option<String>,
    prompt_tokens: u64,
    completion_tokens: u64,
    chunks: u64,
    t_first: Option<Instant>,
    timings: Option<Value>,
}

/// Consume the backend SSE stream; call `emit(content, thinking)` per delta.
async fn pump_sse<F>(resp: reqwest::Response, acc: &mut Acc, tx: Option<&mpsc::Sender<Value>>, mk: F) -> anyhow::Result<()>
where
    F: Fn(&str, &str) -> Value,
{
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut pending: Vec<Value> = Vec::new();
    'outer: while let Some(chunk) = stream.next().await {
        buf.extend_from_slice(&chunk?);
        while let Some(pos) = buf.iter().position(|&c| c == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let line = std::str::from_utf8(&line).unwrap_or("").trim();
            let Some(data) = line.strip_prefix("data:") else { continue };
            let data = data.trim();
            if data == "[DONE]" {
                break 'outer;
            }
            let Ok(v) = serde_json::from_str::<Value>(data) else { continue };
            if let Some(e) = v.get("error") {
                anyhow::bail!("backend error: {e}");
            }
            if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
                acc.prompt_tokens = u.get("prompt_tokens").and_then(|x| x.as_u64()).unwrap_or(acc.prompt_tokens);
                acc.completion_tokens = u.get("completion_tokens").and_then(|x| x.as_u64()).unwrap_or(acc.completion_tokens);
            }
            if let Some(t) = v.get("timings") {
                acc.timings = Some(t.clone());
            }
            let Some(ch) = v.get("choices").and_then(|c| c.get(0)) else { continue };
            if let Some(f) = ch.get("finish_reason").and_then(|f| f.as_str()) {
                acc.finish = Some(f.to_string());
            }
            let Some(d) = ch.get("delta") else { continue };
            let c = d.get("content").and_then(|x| x.as_str()).unwrap_or("");
            let t = d.get("reasoning_content").or_else(|| d.get("reasoning")).and_then(|x| x.as_str()).unwrap_or("");
            if let Some(tcs) = d.get("tool_calls").and_then(|x| x.as_array()) {
                for tc in tcs {
                    let idx = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(acc.tools.len() as u64) as usize;
                    while acc.tools.len() <= idx {
                        acc.tools.push(Default::default());
                    }
                    let e = &mut acc.tools[idx];
                    if let Some(id) = tc.get("id").and_then(|x| x.as_str()) {
                        e.0 = id.to_string();
                    }
                    if let Some(f) = tc.get("function") {
                        if let Some(n) = f.get("name").and_then(|x| x.as_str()) {
                            e.1.push_str(n);
                        }
                        if let Some(a) = f.get("arguments") {
                            match a {
                                Value::String(s) => e.2.push_str(s),
                                other => e.2.push_str(&other.to_string()),
                            }
                        }
                    }
                }
            }
            if !c.is_empty() || !t.is_empty() {
                acc.t_first.get_or_insert_with(Instant::now);
                acc.chunks += 1;
                acc.content.push_str(c);
                acc.thinking.push_str(t);
                if tx.is_some() {
                    pending.push(mk(c, t));
                }
            }
        }
        if let Some(tx) = tx {
            for v in pending.drain(..) {
                if tx.send(v).await.is_err() {
                    anyhow::bail!("client disconnected");
                }
            }
        }
    }
    if let Some(tx) = tx {
        for v in pending.drain(..) {
            if tx.send(v).await.is_err() {
                anyhow::bail!("client disconnected");
            }
        }
    }
    Ok(())
}

fn tool_calls_json(acc: &Acc) -> Vec<Value> {
    acc.tools
        .iter()
        .filter(|t| !t.1.is_empty())
        .map(|(id, name, args)| {
            let parsed: Value = serde_json::from_str(args).unwrap_or_else(|_| json!({}));
            json!({"id": id, "function": {"name": name, "arguments": parsed}})
        })
        .collect()
}

fn final_stats(acc: &Acc, t0: Instant, load: f64, t_sent: Instant) -> Value {
    let ns = |d: Duration| d.as_nanos() as u64;
    let end = Instant::now();
    let first = acc.t_first.unwrap_or(end);
    let (mut pe_n, mut pe_d, mut ev_n, mut ev_d) =
        (acc.prompt_tokens, ns(first.saturating_duration_since(t_sent)), acc.completion_tokens.max(acc.chunks), ns(end.saturating_duration_since(first)));
    if let Some(t) = &acc.timings {
        // llama-server reports exact numbers.
        let f = |k: &str| t.get(k).and_then(|x| x.as_f64());
        if let (Some(n), Some(ms)) = (f("prompt_n"), f("prompt_ms")) {
            pe_n = n as u64;
            pe_d = (ms * 1e6) as u64;
        }
        if let (Some(n), Some(ms)) = (f("predicted_n"), f("predicted_ms")) {
            ev_n = n as u64;
            ev_d = (ms * 1e6) as u64;
        }
    }
    json!({
        "done": true,
        "done_reason": match acc.finish.as_deref() { Some("length") => "length", _ => "stop" },
        "total_duration": ns(end - t0),
        "load_duration": (load * 1e9) as u64,
        "prompt_eval_count": pe_n,
        "prompt_eval_duration": pe_d,
        "eval_count": ev_n,
        "eval_duration": ev_d,
    })
}

async fn chat(State(a): State<App>, AnyJson(b): AnyJson) -> Resp {
    let msgs = b.get("messages").and_then(|m| m.as_array()).cloned().unwrap_or_default();
    ollama_generate(a, b, convert_messages(&msgs), true).await
}

async fn generate(State(a): State<App>, AnyJson(b): AnyJson) -> Resp {
    let prompt = b.get("prompt").and_then(|p| p.as_str()).unwrap_or("");
    let mut msgs = Vec::new();
    if let Some(s) = b.get("system").and_then(|s| s.as_str()) {
        msgs.push(json!({"role": "system", "content": s}));
    }
    if !prompt.is_empty() || b.get("images").is_some() {
        let mut u = json!({"role": "user", "content": prompt});
        if let Some(i) = b.get("images") {
            u["images"] = i.clone();
        }
        msgs.push(u);
    }
    ollama_generate(a, b, convert_messages(&msgs), false).await
}

async fn ollama_generate(a: App, b: Value, messages: Vec<Value>, is_chat: bool) -> Resp {
    let t0 = Instant::now();
    let Some(name) = model_field(&b) else { return err(StatusCode::BAD_REQUEST, "model is required") };
    let ka = keep_alive_field(&b);
    let stream = b.get("stream").and_then(|v| v.as_bool()).unwrap_or(true);
    let has_user = messages.iter().any(|m| m["role"] != "system");

    // Ollama semantics: empty request = load (or unload with keep_alive 0).
    if !has_user {
        if ka.as_deref().and_then(crate::config::parse_duration) == Some(Duration::ZERO) {
            let _ = a.mgr.set_keep_alive(&name, "0").await;
            let m = a.mgr.resolve(&name).map(|m| m.name).unwrap_or(name);
            let mut v = json!({"model": m, "created_at": sys::now_rfc3339(), "done": true, "done_reason": "unload"});
            v[if is_chat { "message" } else { "response" }] = if is_chat { json!({"role": "assistant", "content": ""}) } else { json!("") };
            return Json(v).into_response();
        }
        return match a.mgr.acquire(&name, ka.as_deref()).await {
            Ok((g, load)) => {
                let mut v = json!({"model": g.0.name, "created_at": sys::now_rfc3339(), "done": true, "done_reason": "load", "load_duration": (load * 1e9) as u64});
                v[if is_chat { "message" } else { "response" }] = if is_chat { json!({"role": "assistant", "content": ""}) } else { json!("") };
                Json(v).into_response()
            }
            Err(e) => not_found_or_500(e),
        };
    }

    let (guard, load) = match a.mgr.acquire(&name, ka.as_deref()).await {
        Ok(x) => x,
        Err(e) => return not_found_or_500(e),
    };
    let body = openai_body(&b, messages, guard.0.backend_model_id());
    let t_sent = Instant::now();
    let resp = match a.mgr.http.post(format!("{}/v1/chat/completions", guard.0.base())).json(&body).send().await {
        Ok(r) => r,
        Err(e) => return err(StatusCode::BAD_GATEWAY, format!("backend dropped the request: {e}\n{}", sys::tail(&guard.0.log, 4))),
    };
    if !resp.status().is_success() {
        let code = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let text = resp.text().await.unwrap_or_default();
        return err(code, format!("backend: {text}"));
    }
    let model_name = guard.0.name.clone();
    let key = if is_chat { "message" } else { "response" };

    let mk = move |c: &str, t: &str, model: &str| -> Value {
        let mut v = json!({"model": model, "created_at": sys::now_rfc3339(), "done": false});
        if is_chat {
            let mut m = json!({"role": "assistant", "content": c});
            if !t.is_empty() {
                m["thinking"] = json!(t);
            }
            v["message"] = m;
        } else {
            v["response"] = json!(c);
            if !t.is_empty() {
                v["thinking"] = json!(t);
            }
        }
        v
    };

    if !stream {
        let mut acc = Acc::default();
        if let Err(e) = pump_sse(resp, &mut acc, None, |_, _| Value::Null).await {
            return err(StatusCode::BAD_GATEWAY, e);
        }
        drop(guard);
        let mut v = mk(&acc.content, &acc.thinking, &model_name);
        let tc = tool_calls_json(&acc);
        if !tc.is_empty() && is_chat {
            v["message"]["tool_calls"] = json!(tc);
        }
        if !is_chat {
            v["context"] = json!([]);
        }
        let stats = final_stats(&acc, t0, load, t_sent);
        record_usage(&model_name, &stats);
        for (k, val) in stats.as_object().unwrap() {
            v[k] = val.clone();
        }
        let _ = key;
        return Json(v).into_response();
    }

    let (tx, rx) = mpsc::channel::<Value>(64);
    tokio::spawn(async move {
        let _g: InFlight = guard;
        let mut acc = Acc::default();
        let mn = model_name.clone();
        let r = pump_sse(resp, &mut acc, Some(&tx), |c, t| mk(c, t, &mn)).await;
        if let Err(e) = r {
            let _ = tx.send(json!({"error": e.to_string()})).await;
            return;
        }
        if acc.finish.is_none() && acc.chunks == 0 && acc.tools.is_empty() {
            let _ = tx.send(json!({"error": format!("local model backend failed: {}", sys::error_summary(&_g.0.log))})).await;
            return;
        }
        let tc = tool_calls_json(&acc);
        if !tc.is_empty() {
            let mut v = mk("", "", &model_name);
            if is_chat {
                v["message"]["tool_calls"] = json!(tc);
            }
            let _ = tx.send(v).await;
        }
        let mut v = mk("", "", &model_name);
        if !is_chat {
            v["context"] = json!([]);
        }
        let stats = final_stats(&acc, t0, load, t_sent);
        record_usage(&model_name, &stats);
        for (k, val) in stats.as_object().unwrap() {
            v[k] = val.clone();
        }
        let _ = tx.send(v).await;
    });
    ndjson_stream(rx)
}

// ---------- embeddings ----------

async fn embed_inner(a: &App, model: &str, input: Value, ka: Option<String>) -> Result<(String, Vec<Value>, u64), Resp> {
    let (g, _) = a.mgr.acquire(model, ka.as_deref()).await.map_err(not_found_or_500)?;
    let body = json!({"model": g.0.backend_model_id(), "input": input});
    let r = a
        .mgr
        .http
        .post(format!("{}/v1/embeddings", g.0.base()))
        .json(&body)
        .send()
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, e))?;
    if !r.status().is_success() {
        let t = r.text().await.unwrap_or_default();
        return Err(err(StatusCode::BAD_GATEWAY, format!("backend has no embeddings support for this model: {t}")));
    }
    let v: Value = r.json().await.map_err(|e| err(StatusCode::BAD_GATEWAY, e))?;
    let embs = v.get("data").and_then(|d| d.as_array()).map(|d| d.iter().map(|e| e["embedding"].clone()).collect()).unwrap_or_default();
    let n = v.pointer("/usage/prompt_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
    Ok((g.0.name.clone(), embs, n))
}

async fn embed(State(a): State<App>, AnyJson(b): AnyJson) -> Resp {
    let t0 = Instant::now();
    let Some(model) = model_field(&b) else { return err(StatusCode::BAD_REQUEST, "model is required") };
    let input = b.get("input").cloned().unwrap_or(json!(""));
    match embed_inner(&a, &model, input, keep_alive_field(&b)).await {
        Ok((name, e, n)) => Json(json!({"model": name, "embeddings": e, "total_duration": t0.elapsed().as_nanos() as u64, "prompt_eval_count": n})).into_response(),
        Err(r) => r,
    }
}

async fn embeddings_legacy(State(a): State<App>, AnyJson(b): AnyJson) -> Resp {
    let Some(model) = model_field(&b) else { return err(StatusCode::BAD_REQUEST, "model is required") };
    let input = b.get("prompt").cloned().unwrap_or(json!(""));
    match embed_inner(&a, &model, input, keep_alive_field(&b)).await {
        Ok((_, e, _)) => Json(json!({"embedding": e.into_iter().next().unwrap_or(json!([]))})).into_response(),
        Err(r) => r,
    }
}

// ---------- Anthropic Messages (Claude Code, T3 Code) ----------

fn anth_err(code: StatusCode, kind: &str, msg: impl std::fmt::Display) -> Resp {
    (code, Json(json!({"type": "error", "error": {"type": kind, "message": msg.to_string()}}))).into_response()
}

async fn anthropic_count(AnyJson(b): AnyJson) -> Json<Value> {
    Json(json!({"input_tokens": crate::anthropic::count_tokens(&b)}))
}

async fn anthropic_messages(State(a): State<App>, AnyJson(b): AnyJson) -> Resp {
    let t0 = Instant::now();
    let Some(mut name) = model_field(&b) else { return anth_err(StatusCode::BAD_REQUEST, "invalid_request_error", "model is required") };
    // Claude Code makes background calls with claude-* names; serve them with
    // whatever local model is loaded rather than failing the session.
    if name.starts_with("claude") && a.mgr.resolve(&name).is_err() {
        if let Some(r) = a.mgr.running().first() {
            name = r.name.clone();
        }
    }
    let stream = b.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
    let (guard, _) = match a.mgr.acquire(&name, None).await {
        Ok(x) => x,
        Err(e) => {
            let s = format!("{e:#}");
            let code = if s.contains("not found") { StatusCode::NOT_FOUND } else { StatusCode::INTERNAL_SERVER_ERROR };
            return anth_err(code, if code == StatusCode::NOT_FOUND { "not_found_error" } else { "api_error" }, s);
        }
    };
    let body = crate::anthropic::to_openai(&b, guard.0.backend_model_id());
    let resp = match a.mgr.http.post(format!("{}/v1/chat/completions", guard.0.base())).json(&body).send().await {
        Ok(r) => r,
        Err(e) => return anth_err(StatusCode::BAD_GATEWAY, "api_error", format!("backend unreachable: {e}")),
    };
    if !resp.status().is_success() {
        let t = resp.text().await.unwrap_or_default();
        return anth_err(StatusCode::BAD_GATEWAY, "api_error", format!("backend: {t}"));
    }
    let model_name = guard.0.name.clone();

    // Shared SSE reader: OpenAI chunks -> Anthropic Stream state.
    async fn pump(resp: reqwest::Response, st: &mut crate::anthropic::Stream, tx: Option<&mpsc::Sender<Result<Bytes, std::io::Error>>>) -> Option<Instant> {
        let mut s = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut first = None;
        while let Some(chunk) = s.next().await {
            let Ok(chunk) = chunk else { break };
            buf.extend_from_slice(&chunk);
            let mut out = String::new();
            while let Some(pos) = buf.iter().position(|&c| c == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line);
                let Some(data) = line.trim().strip_prefix("data:") else { continue };
                let data = data.trim();
                if data == "[DONE]" {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<Value>(data) {
                    let piece = st.feed(&v);
                    if !piece.is_empty() {
                        first.get_or_insert_with(Instant::now);
                    }
                    out.push_str(&piece);
                }
            }
            if let Some(tx) = tx {
                if !out.is_empty() && tx.send(Ok(Bytes::from(out))).await.is_err() {
                    return first; // client went away
                }
            }
        }
        first
    }

    let mut st = crate::anthropic::Stream::new(&model_name);
    if !stream {
        let first = pump(resp, &mut st, None).await;
        if st.failed() {
            let msg = format!("local model backend failed: {}", sys::error_summary(&guard.0.log));
            return anth_err(StatusCode::INTERNAL_SERVER_ERROR, "api_error", msg);
        }
        drop(guard);
        let gen_secs = first.map(|f| f.elapsed().as_secs_f64()).unwrap_or(0.0);
        let ttft = first.map(|f| f.duration_since(t0).as_secs_f64()).unwrap_or(0.0);
        USAGE.record(&model_name, st.input_tokens, st.output_tokens, ttft, gen_secs, ttft);
        return Json(st.message()).into_response();
    }
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(64);
    tokio::spawn(async move {
        let _g = guard;
        let _ = tx.send(Ok(Bytes::from(st.start()))).await;
        let first = pump(resp, &mut st, Some(&tx)).await;
        if st.failed() {
            let msg = format!("local model backend failed: {}", sys::error_summary(&_g.0.log));
            sys::log(&format!("/v1/messages {model_name}: {msg}"));
            let ev = crate::anthropic::sse("error", &json!({"type": "error", "error": {"type": "api_error", "message": msg}}));
            let _ = tx.send(Ok(Bytes::from(ev))).await;
            return;
        }
        let _ = tx.send(Ok(Bytes::from(st.finish()))).await;
        sys::log(&format!(
            "/v1/messages {model_name}: {} in / {} out, stop={}, {:.1}s",
            st.input_tokens,
            st.output_tokens,
            st.anthropic_stop(),
            t0.elapsed().as_secs_f64()
        ));
        let gen_secs = first.map(|f| f.elapsed().as_secs_f64()).unwrap_or(0.0);
        let ttft = first.map(|f| f.duration_since(t0).as_secs_f64()).unwrap_or(0.0);
        USAGE.record(&model_name, st.input_tokens, st.output_tokens, ttft, gen_secs, ttft);
    });
    let mut r = Response::new(Body::from_stream(ReceiverStream::new(rx)));
    r.headers_mut().insert("content-type", HeaderValue::from_static("text/event-stream"));
    r.headers_mut().insert("cache-control", HeaderValue::from_static("no-cache"));
    r
}

// ---------- OpenAI ----------

async fn v1_models(State(a): State<App>) -> Json<Value> {
    let data: Vec<Value> = a
        .mgr
        .models()
        .iter()
        .map(|m| {
            let created = m.modified.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            json!({"id": m.name, "object": "model", "created": created, "owned_by": m.format.as_str()})
        })
        .collect();
    Json(json!({"object": "list", "data": data}))
}

async fn v1_model(State(a): State<App>, Path(id): Path<String>) -> Resp {
    match a.mgr.resolve(&id) {
        Ok(m) => Json(json!({"id": m.name, "object": "model", "created": 0, "owned_by": m.format.as_str()})).into_response(),
        Err(e) => not_found_or_500(e),
    }
}

/// Generic OpenAI-compatible proxy: load the requested model, rewrite `model`,
/// stream the backend response back untouched (except the MLX model id).
async fn v1_proxy(State(a): State<App>, method: Method, uri: Uri, headers: HeaderMap, body: Bytes) -> Resp {
    if method != Method::POST {
        return err(StatusCode::NOT_FOUND, "not found");
    }
    let mut v: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, format!("invalid JSON: {e}")),
    };
    let Some(name) = model_field(&v) else { return err(StatusCode::BAD_REQUEST, "model is required") };
    let ka = keep_alive_field(&v);
    let (guard, _) = match a.mgr.acquire(&name, ka.as_deref()).await {
        Ok(x) => x,
        Err(e) => return not_found_or_500(e),
    };
    if let Some(o) = v.as_object_mut() {
        o.remove("keep_alive");
        o.insert("model".into(), json!(guard.0.backend_model_id()));
    }
    let url = format!("{}{}", guard.0.base(), uri.path_and_query().map(|p| p.as_str()).unwrap_or(uri.path()));
    let mut rb = a.mgr.http.post(url).json(&v);
    if let Some(acc) = headers.get("accept") {
        rb = rb.header("accept", acc);
    }
    let resp = match rb.send().await {
        Ok(r) => r,
        Err(e) => return err(StatusCode::BAD_GATEWAY, format!("backend dropped the request: {e}\n{}", sys::tail(&guard.0.log, 4))),
    };
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let ctype = resp.headers().get("content-type").cloned();
    let rewrite = (guard.0.model.format == Format::Mlx).then(|| guard.0.name.clone());

    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(64);
    let ok = status.is_success();
    tokio::spawn(async move {
        let _g = guard;
        let model_name = _g.0.name.clone();
        let t0 = Instant::now();
        let mut t_first: Option<Instant> = None;
        let mut deltas = 0u64;
        let mut tail: Vec<u8> = Vec::with_capacity(16 << 10);
        let mut s = resp.bytes_stream();
        let mut line_buf: Vec<u8> = Vec::new();
        while let Some(chunk) = s.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(Err(std::io::Error::other(e))).await;
                    return;
                }
            };
            if ok {
                t_first.get_or_insert_with(Instant::now);
                deltas += chunk.windows(7).filter(|w| *w == b"\"delta\"").count() as u64;
                tail.extend_from_slice(&chunk);
                if tail.len() > 16 << 10 {
                    let cut = tail.len() - (16 << 10);
                    tail.drain(..cut);
                }
            }
            let out = match &rewrite {
                None => chunk,
                Some(name) => {
                    // Line-buffered rewrite of mlx_lm's "default_model" id.
                    line_buf.extend_from_slice(&chunk);
                    let Some(last_nl) = line_buf.iter().rposition(|&c| c == b'\n') else { continue };
                    let rest = line_buf.split_off(last_nl + 1);
                    let text = String::from_utf8_lossy(&line_buf)
                        .replace("\"model\": \"default_model\"", &format!("\"model\": {}", json!(name)))
                        .replace("\"model\":\"default_model\"", &format!("\"model\":{}", json!(name)));
                    line_buf = rest;
                    Bytes::from(text)
                }
            };
            if tx.send(Ok(out)).await.is_err() {
                return; // client disconnected; dropping the backend stream cancels generation
            }
        }
        if !line_buf.is_empty() {
            let text = match &rewrite {
                Some(name) => String::from_utf8_lossy(&line_buf)
                    .replace("\"model\": \"default_model\"", &format!("\"model\": {}", json!(name)))
                    .replace("\"model\":\"default_model\"", &format!("\"model\":{}", json!(name))),
                None => String::from_utf8_lossy(&line_buf).into_owned(),
            };
            let _ = tx.send(Ok(Bytes::from(text))).await;
        }
        if ok {
            let (p, c) = extract_usage(&tail).unwrap_or((0, deltas));
            let first = t_first.unwrap_or(t0);
            let ttft = first.duration_since(t0).as_secs_f64();
            USAGE.record(&model_name, p, c, ttft, Instant::now().duration_since(first).as_secs_f64(), ttft);
        }
    });
    let mut r = Response::new(Body::from_stream(ReceiverStream::new(rx)));
    *r.status_mut() = status;
    if let Some(ct) = ctype {
        r.headers_mut().insert("content-type", ct);
    }
    r
}

// ---------- lmdeck control ----------

fn status_value(a: &App) -> Value {
    let (total, avail) = sys::mem();
    let cfg = a.mgr.cfg();
    let quick: serde_json::Map<String, Value> = [
        "server.keep_alive",
        "server.max_loaded",
        "server.host",
        "llama.ctx_size",
        "llama.cache_type_k",
        "llama.cache_type_v",
        "llama.cache_reuse",
        "mlx.prompt_cache_size",
        "mlx.kv_bits",
        "mlx.max_kv_size",
        "mlx.fast_path",
        "server.mem_check",
        "server.gpu_heap",
        "vscode.fallback",
    ]
    .iter()
    .map(|k| (k.to_string(), json!(crate::config::get_setting(&cfg, k))))
    .collect();
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "event": EVENTS.version(),
        "uptime_secs": a.started.elapsed().as_secs(),
        "pid": std::process::id(),
        "rss": sys::self_rss(),
        "listen": format!("{}:{}", cfg.server.host, cfg.server.port),
        "host": cfg.server.host,
        "port": cfg.server.port,
        "ips": sys::lan_ips().into_iter().map(|(i, ip)| json!({"iface": i, "ip": ip})).collect::<Vec<_>>(),
        "running": a.mgr.views(),
        "loading": a.mgr.loading(),
        "load": a.mgr.load_progress(),
        "downloads": a.dl.list(),
        "mem": {"total": total, "available": avail, "gpu_budget": crate::compat::gpu_budget(), "muvm": sys::muvm_rss()},
        "usage": USAGE.summary(),
        "settings": quick,
        "vscode_active": a.vscode_active.lock().unwrap().clone(),
    })
}

async fn status(State(a): State<App>) -> Json<Value> {
    Json(status_value(&a))
}

#[derive(serde::Deserialize)]
struct WatchQ {
    since: Option<u64>,
    timeout: Option<u64>,
}

/// Long-poll: returns as soon as anything changes (or after `timeout` s).
/// Lets the bar widget follow lmdeck with ~one idle request a minute.
async fn lm_watch(State(a): State<App>, Query(q): Query<WatchQ>) -> Json<Value> {
    if let Some(since) = q.since {
        EVENTS.wait(since, Duration::from_secs(q.timeout.unwrap_or(55).min(300))).await;
    }
    Json(status_value(&a))
}

async fn lm_config_get(State(a): State<App>) -> Json<Value> {
    let cfg = a.mgr.cfg();
    Json(json!(crate::config::SETTINGS
        .iter()
        .map(|(sec, k, help)| json!({"section": sec, "key": k, "value": crate::config::get_setting(&cfg, k), "help": help, "reload": crate::config::needs_reload(k)}))
        .collect::<Vec<_>>()))
}

async fn lm_config_set(State(a): State<App>, AnyJson(b): AnyJson) -> Resp {
    let (Some(key), Some(value)) = (b.get("key").and_then(|k| k.as_str()), b.get("value")) else {
        return err(StatusCode::BAD_REQUEST, "key and value are required");
    };
    let value = match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let mut cfg = a.mgr.cfg();
    if let Err(e) = crate::config::set_setting(&mut cfg, key, &value) {
        return err(StatusCode::BAD_REQUEST, format!("{key}: {e}"));
    }
    if let Err(e) = cfg.save() {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e);
    }
    *a.mgr.cfg.write().unwrap() = cfg;
    a.mgr.invalidate();
    EVENTS.bump();
    let restart = key == "server.host" || key == "server.port";
    let reload = crate::config::needs_reload(key) && !a.mgr.running().is_empty();
    Json(json!({"ok": true, "reload": reload, "restart_server": restart})).into_response()
}

#[derive(serde::Deserialize)]
struct SearchQ {
    q: Option<String>,
    kind: Option<String>,
    limit: Option<usize>,
}

async fn lm_search(State(a): State<App>, Query(q): Query<SearchQ>) -> Resp {
    let kind = q.kind.unwrap_or_else(|| "mlx".into());
    match hf::search(&a.hf, q.q.as_deref().unwrap_or(""), &kind, q.limit.unwrap_or(30).min(100)).await {
        Ok(h) => {
            let cfg = a.mgr.cfg();
            let out: Vec<Value> = h
                .iter()
                .map(|hit| {
                    let mut v = json!(hit);
                    if kind != "gguf" {
                        v["compat"] = json!(crate::compat::assess_mlx(&cfg, &hit.id, None));
                    }
                    v
                })
                .collect();
            Json(json!(out)).into_response()
        }
        Err(e) => err(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

#[derive(serde::Deserialize)]
struct RepoQ {
    id: String,
}

async fn lm_repo(State(a): State<App>, Query(q): Query<RepoQ>) -> Resp {
    let res = async {
        let sha = hf::repo_sha(&a.hf, &q.id, "main").await?;
        let files = hf::repo_files(&a.hf, &q.id, &sha).await?;
        // The MLX quantization mode lives in config.json; small, so just read it.
        let mut qmode = None;
        if files.iter().any(|f| f.path == "config.json") && files.iter().any(|f| f.path.ends_with(".safetensors")) {
            if let Ok(r) = a.hf.get(format!("{}/{}/resolve/{sha}/config.json", hf::HF, q.id)).send().await {
                if let Ok(v) = r.json::<Value>().await {
                    let qc = v.get("quantization").or_else(|| v.get("quantization_config"));
                    qmode = qc.and_then(|x| x.get("mode")).and_then(|m| m.as_str()).map(str::to_string).or_else(|| qc.map(|_| "affine".into()));
                }
            }
        }
        Ok::<_, anyhow::Error>((files, qmode))
    }
    .await;
    match res {
        Ok((files, qmode)) => {
            let (total, avail) = sys::mem();
            let cfg = a.mgr.cfg();
            let choices: Vec<Value> = hf::choices(&q.id, &files, &cfg.download.gguf_default_quant)
                .into_iter()
                .map(|c| {
                    // GPU budget is the real limit on Asahi (about half of RAM).
                    let fit = crate::compat::gpu_fit(c.size, cfg.llama.ctx_size as u64);
                    let _ = (total, avail);
                    let mut v = json!(c);
                    v["fit"] = json!(fit);
                    v
                })
                .collect();
            let has_mlx = choices.iter().any(|c| c["format"] == "mlx");
            let mut compat = has_mlx.then(|| crate::compat::assess_mlx(&cfg, &q.id, qmode.as_deref()));
            // Only recommend a repo that actually exists.
            if let Some(v) = compat.as_mut() {
                if let Some(rec) = v.recommend.clone() {
                    let ok = a.hf.get(format!("{}/api/models/{rec}", hf::HF)).send().await.map(|r| r.status().is_success()).unwrap_or(false);
                    if !ok {
                        v.recommend = None;
                    }
                }
            }
            Json(json!({"choices": choices, "compat": compat, "qmode": qmode})).into_response()
        }
        Err(e) => err(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

async fn lm_restart(State(a): State<App>, AnyJson(b): AnyJson) -> Resp {
    let names: Vec<String> = match model_field(&b) {
        Some(n) => vec![n],
        None => a.mgr.running().iter().map(|r| r.name.clone()).collect(),
    };
    for n in names {
        if let Err(e) = a.mgr.restart(&n).await {
            return not_found_or_500(e);
        }
    }
    Json(json!({"ok": true})).into_response()
}

fn record_usage(model: &str, stats: &Value) {
    let f = |k: &str| stats[k].as_u64().unwrap_or(0);
    let pd = f("prompt_eval_duration") as f64 / 1e9;
    let ed = f("eval_duration") as f64 / 1e9;
    USAGE.record(model, f("prompt_eval_count"), f("eval_count"), pd, ed, pd);
}

/// Token counts from the tail of an OpenAI/Anthropic/llama.cpp response.
fn extract_usage(tail: &[u8]) -> Option<(u64, u64)> {
    let s = String::from_utf8_lossy(tail);
    for (i, _) in s.rmatch_indices("\"usage\"") {
        let rest = &s[i + 7..];
        let Some(colon) = rest.find(':') else { continue };
        let mut it = serde_json::Deserializer::from_str(&rest[colon + 1..]).into_iter::<Value>();
        if let Some(Ok(v)) = it.next() {
            let g = |a: &str, b: &str| v.get(a).or_else(|| v.get(b)).and_then(|x| x.as_u64());
            if let (Some(p), Some(c)) = (g("prompt_tokens", "input_tokens"), g("completion_tokens", "output_tokens")) {
                return Some((p, c));
            }
        }
    }
    for (i, _) in s.rmatch_indices("\"timings\"") {
        let rest = &s[i + 9..];
        let Some(colon) = rest.find(':') else { continue };
        let mut it = serde_json::Deserializer::from_str(&rest[colon + 1..]).into_iter::<Value>();
        if let Some(Ok(v)) = it.next() {
            if let (Some(p), Some(c)) = (v["prompt_n"].as_u64(), v["predicted_n"].as_u64()) {
                return Some((p, c));
            }
        }
    }
    None
}

async fn lm_models(State(a): State<App>) -> Json<Value> {
    a.mgr.invalidate();
    let cfg = a.mgr.cfg();
    let out: Vec<Value> = a
        .mgr
        .models()
        .iter()
        .map(|m| {
            let mut v = json!(m);
            v["device"] = json!(cfg.overrides.get(&m.name).and_then(|o| o.device.clone()).unwrap_or_else(|| "gpu".into()));
            v["gpu_fit"] = json!(crate::compat::gpu_fit(m.size, cfg.llama.ctx_size as u64));
            if m.format == Format::Mlx {
                v["compat"] = json!(crate::compat::assess_mlx(&cfg, m.repo.as_deref().unwrap_or(&m.name), m.qmode.as_deref()));
            }
            v
        })
        .collect();
    Json(json!(out))
}

/// Per-model override: {model, key: device|ctx_size|keep_alive, value ("" clears)}.
async fn lm_override(State(a): State<App>, AnyJson(b): AnyJson) -> Resp {
    let (Some(name), Some(key)) = (model_field(&b), b.get("key").and_then(|k| k.as_str())) else {
        return err(StatusCode::BAD_REQUEST, "model and key are required");
    };
    let value = match b.get("value") {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Null) | None => String::new(),
        Some(v) => v.to_string(),
    };
    let m = match a.mgr.resolve(&name) {
        Ok(m) => m,
        Err(e) => return not_found_or_500(e),
    };
    let mut cfg = a.mgr.cfg();
    let o = cfg.overrides.entry(m.name.clone()).or_default();
    match key {
        "device" => {
            if !["", "gpu", "cpu"].contains(&value.as_str()) {
                return err(StatusCode::BAD_REQUEST, "device must be gpu or cpu");
            }
            o.device = (value == "cpu").then(|| value.clone());
        }
        "ctx_size" => o.ctx_size = value.parse().ok(),
        "keep_alive" => o.keep_alive = (!value.is_empty()).then(|| value.clone()),
        _ => return err(StatusCode::BAD_REQUEST, "unknown override key"),
    }
    if let Err(e) = cfg.save() {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e);
    }
    *a.mgr.cfg.write().unwrap() = cfg;
    EVENTS.bump();
    sys::log(&format!("override {}: {key} = {value:?}", m.name));
    Json(json!({"ok": true})).into_response()
}

#[derive(serde::Deserialize)]
struct PlanQ {
    model: String,
}

async fn lm_plan(State(a): State<App>, Query(q): Query<PlanQ>) -> Json<Value> {
    Json(a.mgr.plan_preview(&q.model))
}

/// Called by the lmdeck VS Code extension when it decides to check (startup,
/// window focus, or on request): registers the provider, reads the Copilot
/// quota state once, and returns what the extension should apply.
async fn lm_vscode(State(a): State<App>) -> Json<Value> {
    let cfg = a.mgr.cfg();
    let models = a.mgr.models();
    let loaded = a.mgr.running().first().map(|r| r.name.clone());
    let active = a.vscode_active.lock().unwrap().clone();
    let v = tokio::task::spawn_blocking(move || {
        let _ = crate::vscode::ensure_provider(&cfg);
        json!({
            "installed": crate::vscode::installed(),
            "mode": cfg.vscode.fallback,
            "quota_blocked": crate::vscode::quota_blocked(),
            "suggested_model": crate::vscode::pick_model(&cfg, &models, loaded.as_deref()),
            "active_model": active,
        })
    })
    .await
    .unwrap_or_default();
    Json(v)
}

/// The extension reports what it applied, so the TUI and panel can show it.
async fn lm_vscode_state(State(a): State<App>, AnyJson(b): AnyJson) -> Json<Value> {
    let active = b.get("active_model").and_then(|m| m.as_str()).map(str::to_string);
    let _ = std::fs::write(crate::config::state_dir().join("vscode-active"), active.clone().unwrap_or_default());
    *a.vscode_active.lock().unwrap() = active;
    EVENTS.bump();
    Json(json!({"ok": true}))
}

async fn lm_compat(State(a): State<App>) -> Json<Value> {
    let cfg = a.mgr.cfg();
    let p = tokio::task::spawn_blocking(move || crate::compat::probe(&cfg)).await.ok().flatten();
    Json(json!({"probe": p, "catalog": crate::compat::catalog()}))
}

async fn lm_load(State(a): State<App>, AnyJson(b): AnyJson) -> Resp {
    let Some(name) = model_field(&b) else { return err(StatusCode::BAD_REQUEST, "model is required") };
    match a.mgr.acquire(&name, keep_alive_field(&b).as_deref()).await {
        Ok((g, secs)) => Json(json!({"model": g.0.name, "load_secs": secs, "port": g.0.port})).into_response(),
        Err(e) => {
            let text = format!("{e:#}");
            if !crate::compat::is_gpu_gap(&text) {
                return not_found_or_500(e);
            }
            // Structured so the TUI/panel can offer "run on CPU" or the recommended download.
            let model = a.mgr.resolve(&name).ok();
            let recommend = model.as_ref().filter(|m| m.format == Format::Mlx).and_then(|m| {
                crate::compat::assess_mlx(&a.mgr.cfg(), m.repo.as_deref().unwrap_or(&m.name), m.qmode.as_deref()).recommend
            });
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": text, "gpu_gap": true, "model": model.map(|m| m.name).unwrap_or(name), "recommend": recommend})),
            )
                .into_response()
        }
    }
}

async fn lm_unload(State(a): State<App>, AnyJson(b): AnyJson) -> Resp {
    match model_field(&b) {
        Some(n) => match a.mgr.unload(&n).await {
            Ok(()) => Json(json!({"ok": true})).into_response(),
            Err(e) => err(StatusCode::BAD_REQUEST, e),
        },
        None => {
            a.mgr.unload_all().await;
            Json(json!({"ok": true})).into_response()
        }
    }
}

async fn lm_pull(State(a): State<App>, AnyJson(b): AnyJson) -> Resp {
    let Some(name) = model_field(&b) else { return err(StatusCode::BAD_REQUEST, "model is required") };
    sys::log(&format!("pull requested: {name}"));
    let d = a.dl.start(a.hf.clone(), a.mgr.cfg(), &name);
    Json(json!(d.view())).into_response()
}

async fn lm_cancel(State(a): State<App>, AnyJson(b): AnyJson) -> Resp {
    let Some(name) = model_field(&b) else { return err(StatusCode::BAD_REQUEST, "model is required") };
    Json(json!({"ok": a.dl.cancel(&name)})).into_response()
}

async fn lm_downloads(State(a): State<App>) -> Json<Value> {
    Json(json!(a.dl.list()))
}

async fn lm_clear_downloads(State(a): State<App>) -> StatusCode {
    a.dl.clear_finished();
    a.mgr.invalidate();
    StatusCode::OK
}

async fn lm_reload(State(a): State<App>) -> Resp {
    match Config::load() {
        Ok(c) => {
            *a.mgr.cfg.write().unwrap() = c;
            a.mgr.invalidate();
            Json(json!({"ok": true, "note": "host/port changes need a daemon restart"})).into_response()
        }
        Err(e) => err(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

#[derive(serde::Deserialize)]
struct LogQ {
    model: Option<String>,
    lines: Option<usize>,
}

async fn lm_logs(State(a): State<App>, Query(q): Query<LogQ>) -> Resp {
    let n = q.lines.unwrap_or(200);
    let path = match q.model {
        Some(m) => {
            let name = a.mgr.resolve(&m).map(|m| m.name).unwrap_or(m);
            crate::config::log_dir().join(format!("{}.log", name.replace(['/', ':'], "_")))
        }
        None => crate::config::log_dir().join("lmdeck.log"),
    };
    sys::tail(&path, n).into_response()
}
