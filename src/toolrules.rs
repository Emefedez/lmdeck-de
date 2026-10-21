//! Per-tool rules on top of `server.local_tools`: `server.tool_deny` removes matching tools from
//! what a local model gets (tool groups included), `server.tool_allow` keeps matching tools the
//! mode would drop (an MCP tool is then sent directly, not through tool groups). Allow wins over
//! deny, so `tool_deny = ["mcp__rea__*"]` + `tool_allow = ["mcp__rea__demangle_swift"]` keeps one.
//!
//! Patterns are globs (`*`) on canonical names: Claude Code's as sent (`Bash`, `mcp__rea__x`),
//! VS Code's `mcp_<server>_<tool>` and Codex's namespaced functions as `mcp__<server>__<tool>`
//! (the raw VS Code name matches too).
//!
//! Each client's last tool list is kept in ~/.local/state/lmdeck/tools-seen.json so `lmdeck tools`
//! (and ctx-trim) can show what every rule does without a request in flight.

use crate::anthropic::ToolMode;
use serde_json::{json, Value};

#[derive(Clone, Debug, Default)]
pub struct Rules {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
}

pub fn glob(pat: &str, name: &str) -> bool {
    let parts: Vec<&str> = pat.split('*').collect();
    if parts.len() == 1 {
        return pat == name;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !name.starts_with(first) || name.len() < first.len() + last.len() || !name.ends_with(last) {
        return false;
    }
    let mut rest = &name[first.len()..name.len() - last.len()];
    for p in &parts[1..parts.len() - 1] {
        match rest.find(p) {
            Some(i) => rest = &rest[i + p.len()..],
            None => return false,
        }
    }
    true
}

impl Rules {
    pub fn from_cfg(c: &crate::config::Config) -> Self {
        Rules { allow: c.server.tool_allow.clone(), deny: c.server.tool_deny.clone() }
    }

    fn any(pats: &[String], names: &[&str]) -> bool {
        pats.iter().any(|p| names.iter().any(|n| glob(p, n)))
    }

    /// Explicitly allowed (overrides the mode and any deny).
    pub fn allows(&self, names: &[&str]) -> bool {
        Self::any(&self.allow, names)
    }

    /// Denied and not allowed.
    pub fn denies(&self, names: &[&str]) -> bool {
        Self::any(&self.deny, names) && !self.allows(names)
    }
}

/// VS Code's `mcp_<server>_<tool>` as `mcp__<server>__<tool>` (same split as the tool groups).
pub fn vscode_canonical(name: &str) -> Option<String> {
    match name.strip_prefix("mcp_").and_then(|r| r.split_once('_')) {
        Some((s, t)) if !s.is_empty() && !t.is_empty() && !name.starts_with("mcp__") => Some(format!("mcp__{s}__{t}")),
        _ => None,
    }
}

/// Codex namespace + function as one canonical name ("mcp__rea__" + "open_binary").
pub fn codex_canonical(ns: &str, name: &str) -> String {
    if ns.ends_with("__") { format!("{ns}{name}") } else { format!("{ns}__{name}") }
}

/// What happens to a tool under the current mode and rules, for `lmdeck tools`.
pub fn status(client: &str, name: &str, mode: ToolMode, rules: &Rules) -> &'static str {
    let canon = if client == "copilot" { vscode_canonical(name) } else { None };
    let names: Vec<&str> = std::iter::once(name).chain(canon.as_deref()).collect();
    let mcp = names.iter().any(|n| n.starts_with("mcp__"));
    if rules.denies(&names) {
        return "denied";
    }
    if rules.allows(&names) {
        return "allowed";
    }
    match (mode, mcp) {
        (ToolMode::All, _) => "sent",
        (ToolMode::Groups, true) if client != "codex" => "group",
        (_, true) => "dropped",
        (ToolMode::Lean | ToolMode::Groups, false) if client == "claude" && crate::anthropic::LEAN_DROP.contains(&name) => "dropped",
        (ToolMode::Lean | ToolMode::Groups, false) => "sent (short)",
        (ToolMode::Core, false) => "sent",
    }
}

fn seen_path() -> std::path::PathBuf {
    crate::config::state_dir().join("tools-seen.json")
}

pub fn seen() -> Value {
    std::fs::read(seen_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_else(|| json!({}))
}

/// Remember a client's tool list ([(canonical name, ~tokens)]); written only when it changed.
pub fn record(client: &str, tools: Vec<(String, u64)>) {
    if tools.is_empty() || cfg!(test) {
        return;
    }
    let mut all = seen();
    let list: Vec<Value> = tools.iter().map(|(n, t)| json!({"name": n, "tokens": t})).collect();
    if all[client]["tools"] == json!(list) {
        return;
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    all[client] = json!({"updated": now, "tools": list});
    let p = seen_path();
    let _ = std::fs::create_dir_all(p.parent().unwrap());
    let tmp = p.with_extension("json.tmp");
    if std::fs::write(&tmp, serde_json::to_vec(&all).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
}

fn tok(v: &Value) -> u64 {
    v.to_string().len() as u64 / 4
}

/// Anthropic tools (Claude Code): record, then drop denied ones.
pub fn anthropic(b: &mut Value, rules: &Rules) {
    let Some(tools) = b.get_mut("tools").and_then(|t| t.as_array_mut()) else { return };
    record("claude", tools.iter().map(|t| (t["name"].as_str().unwrap_or("").to_string(), tok(t))).collect());
    if !rules.deny.is_empty() {
        tools.retain(|t| !rules.denies(&[t["name"].as_str().unwrap_or("")]));
    }
}

/// OpenAI chat tools (VS Code Copilot): record, then drop denied ones.
pub fn openai(b: &mut Value, rules: &Rules) {
    let Some(tools) = b.get_mut("tools").and_then(|t| t.as_array_mut()) else { return };
    record("copilot", tools.iter().map(|t| (t["function"]["name"].as_str().unwrap_or("").to_string(), tok(t))).collect());
    if !rules.deny.is_empty() {
        tools.retain(|t| {
            let n = t["function"]["name"].as_str().unwrap_or("");
            let c = vscode_canonical(n);
            !rules.denies(&std::iter::once(n).chain(c.as_deref()).collect::<Vec<_>>())
        });
    }
}

/// Responses tools (Codex): record (namespaces flattened), then drop denied ones.
pub fn responses(b: &mut Value, rules: &Rules) {
    let Some(tools) = b.get_mut("tools").and_then(|t| t.as_array_mut()) else { return };
    let mut list = Vec::new();
    for t in tools.iter() {
        let name = t["name"].as_str().unwrap_or("");
        if t["type"] == "namespace" {
            for inner in t["tools"].as_array().into_iter().flatten() {
                list.push((codex_canonical(name, inner["name"].as_str().unwrap_or("")), tok(inner)));
            }
        } else {
            list.push((name.to_string(), tok(t)));
        }
    }
    record("codex", list);
    if rules.deny.is_empty() {
        return;
    }
    for t in tools.iter_mut() {
        if t["type"] == "namespace" {
            let ns = t["name"].as_str().unwrap_or("").to_string();
            if let Some(inner) = t.get_mut("tools").and_then(|x| x.as_array_mut()) {
                inner.retain(|i| !rules.denies(&[&codex_canonical(&ns, i["name"].as_str().unwrap_or(""))]));
            }
        }
    }
    tools.retain(|t| {
        if t["type"] == "namespace" {
            t["tools"].as_array().is_some_and(|i| !i.is_empty())
        } else {
            !rules.denies(&[t["name"].as_str().unwrap_or("")])
        }
    });
}

/// Edit the allow/deny lists: `allow` / `deny` move `pattern` to that list, `default` removes it.
pub fn set(c: &mut crate::config::Config, pattern: &str, action: &str) -> Result<(), String> {
    let s = &mut c.server;
    s.tool_allow.retain(|p| p != pattern);
    s.tool_deny.retain(|p| p != pattern);
    match action {
        "allow" => s.tool_allow.push(pattern.into()),
        "deny" => s.tool_deny.push(pattern.into()),
        "default" => {}
        _ => return Err("use allow, deny or default".into()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(allow: &[&str], deny: &[&str]) -> Rules {
        Rules { allow: allow.iter().map(|s| s.to_string()).collect(), deny: deny.iter().map(|s| s.to_string()).collect() }
    }

    #[test]
    fn globs() {
        assert!(glob("mcp__rea__*", "mcp__rea__open_binary"));
        assert!(!glob("mcp__rea__*", "mcp__hypruse__ui"));
        assert!(glob("*binary*", "mcp__rea__open_binary"));
        assert!(glob("Bash", "Bash") && !glob("Bash", "BashOutput"));
    }

    #[test]
    fn allow_wins_and_statuses() {
        let r = rules(&["mcp__rea__demangle_swift", "NotebookEdit"], &["mcp__rea__*", "WebSearch"]);
        assert_eq!(status("claude", "mcp__rea__open_binary", ToolMode::Groups, &r), "denied");
        assert_eq!(status("claude", "mcp__rea__demangle_swift", ToolMode::Groups, &r), "allowed");
        assert_eq!(status("claude", "NotebookEdit", ToolMode::Lean, &r), "allowed");
        assert_eq!(status("claude", "Agent", ToolMode::Lean, &r), "dropped");
        assert_eq!(status("claude", "mcp__hypruse__ui", ToolMode::Groups, &r), "group");
        assert_eq!(status("codex", "mcp__hypruse__ui", ToolMode::Groups, &r), "dropped");
        assert_eq!(status("copilot", "mcp_rea_open_binary", ToolMode::All, &r), "denied");
        assert_eq!(status("claude", "WebSearch", ToolMode::All, &r), "denied");
    }

    #[test]
    fn deny_removes_from_each_format() {
        let r = rules(&[], &["mcp__rea__*", "Bash"]);
        let mut a = json!({"tools": [{"name": "Bash"}, {"name": "Read"}, {"name": "mcp__rea__x"}]});
        anthropic(&mut a, &r);
        assert_eq!(a["tools"].as_array().unwrap().len(), 1);
        let mut o = json!({"tools": [{"type": "function", "function": {"name": "mcp_rea_x"}}, {"type": "function", "function": {"name": "read"}}]});
        openai(&mut o, &r);
        assert_eq!(o["tools"][0]["function"]["name"], "read");
        let mut c = json!({"tools": [
            {"type": "namespace", "name": "mcp__rea__", "tools": [{"type": "function", "name": "open_binary"}]},
            {"type": "namespace", "name": "mcp__hypruse__", "tools": [{"type": "function", "name": "ui"}]},
            {"type": "function", "name": "shell"}]});
        responses(&mut c, &r);
        assert_eq!(c["tools"].as_array().unwrap().len(), 2);
        assert_eq!(c["tools"][0]["name"], "mcp__hypruse__");
    }
}
