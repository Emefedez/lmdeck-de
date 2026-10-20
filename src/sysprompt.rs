//! System prompt injected per model and per backend (`overrides."<model>".system_prompt` beats
//! `mlx.system_prompt` / `llama.system_prompt`). Applied to every chat request on its way to the
//! backend: Ollama /api/chat and /api/generate, Anthropic /v1/messages and OpenAI /v1/chat/completions.

use crate::config::Config;
use crate::models::{Format, LocalModel};
use serde_json::{json, Value};

pub const MODES: &[&str] = &["prepend", "replace", "fallback"];

/// The prompt and mode that apply to `m`, after template expansion. `None`: nothing to inject.
pub fn for_model(cfg: &Config, name: &str, format: Format) -> Option<(String, String)> {
    let (backend_text, backend_mode, backend) = match format {
        Format::Mlx => (&cfg.mlx.system_prompt, &cfg.mlx.system_prompt_mode, "mlx"),
        Format::Gguf => (&cfg.llama.system_prompt, &cfg.llama.system_prompt_mode, "llama.cpp"),
    };
    let ov = cfg.overrides.get(name);
    let text = ov.and_then(|o| o.system_prompt.as_ref()).unwrap_or(backend_text);
    if text.trim().is_empty() {
        return None;
    }
    let mode = ov.and_then(|o| o.system_prompt_mode.clone()).unwrap_or_else(|| backend_mode.clone());
    Some((expand(text, name, backend), mode))
}

/// `{model}`, `{backend}` and `{date}` (YYYY-MM-DD, UTC).
fn expand(text: &str, model: &str, backend: &str) -> String {
    let mut s = text.replace("{model}", model).replace("{backend}", backend);
    if s.contains("{date}") {
        s = s.replace("{date}", &crate::sys::now_rfc3339()[..10]);
    }
    s
}

/// Apply to an OpenAI chat body's `messages`:
///   prepend  - before the client's system prompt (or as the system prompt if there is none)
///   replace  - instead of the client's system prompts
///   fallback - only when the client sent none
pub fn apply(body: &mut Value, text: &str, mode: &str) {
    let Some(msgs) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else { return };
    let first_sys = msgs.iter().position(|m| m["role"] == "system");
    match (mode, first_sys) {
        ("fallback", Some(_)) => {}
        ("replace", _) => {
            msgs.retain(|m| m["role"] != "system");
            msgs.insert(0, json!({"role": "system", "content": text}));
        }
        (_, Some(i)) if i == 0 => {
            let m = &mut msgs[0];
            match m.get_mut("content") {
                Some(Value::String(s)) => *s = if s.is_empty() { text.to_string() } else { format!("{text}\n\n{s}") },
                Some(Value::Array(parts)) => parts.insert(0, json!({"type": "text", "text": text})),
                _ => m["content"] = json!(text),
            }
        }
        _ => msgs.insert(0, json!({"role": "system", "content": text})),
    }
}

/// Apply whatever prompt `cfg` sets for `m` (no-op when none is set).
pub fn apply_for(cfg: &Config, m: &LocalModel, body: &mut Value) {
    if let Some((text, mode)) = for_model(cfg, &m.name, m.format) {
        apply(body, &text, &mode);
    }
}

/// TUI single-line editing: newlines shown and typed as a literal `\n`.
pub fn to_line(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\n', "\\n")
}
pub fn from_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('\\') => out.push('\\'),
            Some(o) => {
                out.push('\\');
                out.push(o);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(msgs: Value) -> Value {
        json!({"model": "x", "messages": msgs})
    }

    #[test]
    fn prepend_merges_into_the_client_system_prompt() {
        let mut b = body(json!([{"role": "system", "content": "client"}, {"role": "user", "content": "hi"}]));
        apply(&mut b, "ours", "prepend");
        assert_eq!(b["messages"][0]["content"], "ours\n\nclient");
        assert_eq!(b["messages"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn prepend_adds_one_when_the_client_sent_none() {
        let mut b = body(json!([{"role": "user", "content": "hi"}]));
        apply(&mut b, "ours", "prepend");
        assert_eq!(b["messages"][0], json!({"role": "system", "content": "ours"}));
    }

    #[test]
    fn prepend_into_content_parts() {
        let mut b = body(json!([{"role": "system", "content": [{"type": "text", "text": "client"}]}, {"role": "user", "content": "hi"}]));
        apply(&mut b, "ours", "prepend");
        assert_eq!(b["messages"][0]["content"][0]["text"], "ours");
        assert_eq!(b["messages"][0]["content"][1]["text"], "client");
    }

    #[test]
    fn replace_drops_every_client_system_message() {
        let mut b = body(json!([{"role": "system", "content": "a"}, {"role": "user", "content": "hi"}, {"role": "system", "content": "b"}]));
        apply(&mut b, "ours", "replace");
        let m = b["messages"].as_array().unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m[0]["content"], "ours");
        assert_eq!(m[1]["role"], "user");
    }

    #[test]
    fn fallback_keeps_the_client_prompt() {
        let mut b = body(json!([{"role": "system", "content": "client"}, {"role": "user", "content": "hi"}]));
        apply(&mut b, "ours", "fallback");
        assert_eq!(b["messages"][0]["content"], "client");
        let mut b = body(json!([{"role": "user", "content": "hi"}]));
        apply(&mut b, "ours", "fallback");
        assert_eq!(b["messages"][0]["content"], "ours");
    }

    #[test]
    fn model_override_beats_backend_and_empty_override_disables() {
        let mut cfg = Config::default();
        cfg.mlx.system_prompt = "backend {backend} {model}".into();
        assert_eq!(for_model(&cfg, "org/m", Format::Mlx).unwrap().0, "backend mlx org/m");
        cfg.overrides.entry("org/m".into()).or_default().system_prompt = Some("model".into());
        assert_eq!(for_model(&cfg, "org/m", Format::Mlx).unwrap(), ("model".to_string(), "prepend".to_string()));
        cfg.overrides.get_mut("org/m").unwrap().system_prompt = Some(String::new());
        assert!(for_model(&cfg, "org/m", Format::Mlx).is_none());
        assert!(for_model(&cfg, "org/g:Q4", Format::Gguf).is_none());
    }

    #[test]
    fn line_escapes_round_trip() {
        let s = "a\nb \\n c\\";
        assert_eq!(from_line(&to_line(s)), s);
        assert_eq!(from_line("one\\ntwo"), "one\ntwo");
    }
}
