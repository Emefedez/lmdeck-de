//! System prompts, kept as plain files so any editor can show and change them:
//!
//!   ~/.config/lmdeck/prompts/mlx.md, llama.md          per backend
//!   ~/.config/lmdeck/prompts/models/<model>.md          per model ('/' in the name becomes "--")
//!
//! A model file beats its backend's; an empty model file means no system prompt for that model.
//! How the prompt combines with the client's is `mlx.system_prompt_mode` / `llama.system_prompt_mode`
//! or `overrides."<model>".system_prompt_mode`. Applied to every chat request on its way to the
//! backend (Ollama /api/chat and /api/generate, Anthropic /v1/messages, OpenAI /v1/chat/completions),
//! read fresh each time, so an edit applies to the next request without a reload.

use crate::config::Config;
use crate::models::{Format, LocalModel};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub const MODES: &[&str] = &["prepend", "replace", "fallback"];

pub fn dir() -> PathBuf {
    crate::config::config_dir_or_home().join("lmdeck/prompts")
}

fn backend_name(f: Format) -> &'static str {
    match f {
        Format::Mlx => "mlx",
        Format::Gguf => "llama",
    }
}

pub fn backend_file_in(dir: &Path, f: Format) -> PathBuf {
    dir.join(format!("{}.md", backend_name(f)))
}
pub fn backend_file(f: Format) -> PathBuf {
    backend_file_in(&dir(), f)
}

pub fn model_file_in(dir: &Path, name: &str) -> PathBuf {
    dir.join("models").join(format!("{}.md", name.replace('/', "--")))
}
pub fn model_file(name: &str) -> PathBuf {
    model_file_in(&dir(), name)
}

/// Where a model's prompt comes from (the TUI's Prompt column).
#[derive(Debug, PartialEq, Eq)]
pub enum Source {
    Model,
    Off,
    Backend,
    None,
}

fn read(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok()
}

pub fn source_in(dir: &Path, name: &str, f: Format) -> Source {
    match read(&model_file_in(dir, name)) {
        Some(t) if t.trim().is_empty() => Source::Off,
        Some(_) => Source::Model,
        None if read(&backend_file_in(dir, f)).is_some_and(|t| !t.trim().is_empty()) => Source::Backend,
        None => Source::None,
    }
}
pub fn source(name: &str, f: Format) -> Source {
    source_in(&dir(), name, f)
}

/// The backend's prompt text ("" when none), for settings rows and for seeding a new model file.
pub fn backend_text(f: Format) -> String {
    read(&backend_file(f)).unwrap_or_default()
}

pub fn set_backend_text(f: Format, text: &str) -> std::io::Result<()> {
    let p = backend_file(f);
    std::fs::create_dir_all(p.parent().unwrap())?;
    std::fs::write(p, text)
}

/// The prompt and mode that apply to a model, after template expansion. `None`: nothing to inject.
pub fn for_model_in(cfg: &Config, dir: &Path, name: &str, f: Format) -> Option<(String, String)> {
    let text = read(&model_file_in(dir, name)).or_else(|| read(&backend_file_in(dir, f)))?;
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let backend_mode = match f {
        Format::Mlx => &cfg.mlx.system_prompt_mode,
        Format::Gguf => &cfg.llama.system_prompt_mode,
    };
    let mode = cfg.overrides.get(name).and_then(|o| o.system_prompt_mode.clone()).unwrap_or_else(|| backend_mode.clone());
    Some((expand(text, name, if f == Format::Mlx { "mlx" } else { "llama.cpp" }), mode))
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
        (_, Some(0)) => {
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

/// Apply whatever prompt applies to `m` (no-op when none does).
pub fn apply_for(cfg: &Config, m: &LocalModel, body: &mut Value) {
    if let Some((text, mode)) = for_model_in(cfg, &dir(), &m.name, m.format) {
        apply(body, &text, &mode);
    }
}

/// The command that edits a file: `ui.editor`, else $VISUAL, $EDITOR, nvim. GUI editors that
/// return at once (VS Code and its forks) get --wait so the TUI resumes after the tab is closed.
pub fn editor_cmd(cfg: &Config, file: &Path) -> Vec<String> {
    let line = [cfg.ui.editor.clone(), std::env::var("VISUAL").unwrap_or_default(), std::env::var("EDITOR").unwrap_or_default()]
        .into_iter()
        .find(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "nvim".into());
    let mut argv: Vec<String> = line.split_whitespace().map(str::to_string).collect();
    let exe = Path::new(&argv[0]).file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
    if ["code", "code-insiders", "codium", "vscodium", "cursor"].contains(&exe.as_str())
        && !argv.iter().any(|a| a == "--wait" || a == "-w")
    {
        argv.push("--wait".into());
    }
    argv.push(file.display().to_string());
    argv
}

/// One-line form for settings rows and the bar panel: newlines shown and typed as a literal `\n`.
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
    fn model_file_beats_backend_file_and_an_empty_one_disables() {
        let d = std::env::temp_dir().join(format!("lmdeck-sysprompt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("models")).unwrap();
        let mut cfg = Config::default();
        assert!(for_model_in(&cfg, &d, "org/m", Format::Mlx).is_none());
        assert_eq!(source_in(&d, "org/m", Format::Mlx), Source::None);
        std::fs::write(backend_file_in(&d, Format::Mlx), "backend {backend} {model}\n").unwrap();
        assert_eq!(for_model_in(&cfg, &d, "org/m", Format::Mlx).unwrap().0, "backend mlx org/m");
        assert_eq!(source_in(&d, "org/m", Format::Mlx), Source::Backend);
        assert!(for_model_in(&cfg, &d, "org/g:Q4", Format::Gguf).is_none());
        let mf = model_file_in(&d, "org/m");
        assert!(mf.ends_with("models/org--m.md"));
        std::fs::write(&mf, "model\n").unwrap();
        cfg.overrides.entry("org/m".into()).or_default().system_prompt_mode = Some("replace".into());
        assert_eq!(for_model_in(&cfg, &d, "org/m", Format::Mlx).unwrap(), ("model".to_string(), "replace".to_string()));
        assert_eq!(source_in(&d, "org/m", Format::Mlx), Source::Model);
        std::fs::write(&mf, "  \n").unwrap();
        assert!(for_model_in(&cfg, &d, "org/m", Format::Mlx).is_none());
        assert_eq!(source_in(&d, "org/m", Format::Mlx), Source::Off);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn vscode_waits_and_editor_falls_back() {
        let mut cfg = Config::default();
        cfg.ui.editor = "code".into();
        assert_eq!(editor_cmd(&cfg, Path::new("/x.md")), vec!["code", "--wait", "/x.md"]);
        cfg.ui.editor = "/usr/bin/code -n -w".into();
        assert_eq!(editor_cmd(&cfg, Path::new("/x.md")), vec!["/usr/bin/code", "-n", "-w", "/x.md"]);
        cfg.ui.editor = "nvim +startinsert".into();
        assert_eq!(editor_cmd(&cfg, Path::new("/x.md")), vec!["nvim", "+startinsert", "/x.md"]);
    }

    #[test]
    fn line_escapes_round_trip() {
        let s = "a\nb \\n c\\";
        assert_eq!(from_line(&to_line(s)), s);
        assert_eq!(from_line("one\\ntwo"), "one\ntwo");
    }
}
