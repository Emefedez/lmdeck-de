//! Tool groups: MCP tools for local models without paying for every schema up front.
//!
//! The model sees two small meta tools instead of hundreds of MCP schemas:
//! `tool_group` (show the tools of a group, answered here from a markdown segment) and
//! `call_tool` (rewritten into the real MCP tool_use for the client). Group docs live in
//! ~/.config/lmdeck/tool-groups/<server>.md: one file per MCP server, one `## section` per
//! subgroup. Files are generated from the client's tool list; remove the
//! `lmdeck:auto` marker line to keep your own edits from being regenerated.

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;

const AUTO_MARKER: &str = "<!-- lmdeck:auto";
/// Servers with more tools than this get subgroups by tool-name prefix.
const SPLIT_ABOVE: usize = 12;

pub fn dir() -> PathBuf {
    crate::config::config_path().parent().map(|p| p.join("tool-groups")).unwrap_or_else(|| PathBuf::from("tool-groups"))
}

/// (server, tool name without the mcp__server__ prefix) for an MCP tool name.
fn split_name(full: &str) -> Option<(&str, &str)> {
    full.strip_prefix("mcp__")?.split_once("__")
}

fn short(s: &str, max: usize) -> String {
    let first = s.split("\n\n").next().unwrap_or(s).split(". ").next().unwrap_or(s).trim().replace('\n', " ");
    if first.chars().count() <= max { first } else { format!("{}…", first.chars().take(max).collect::<String>()) }
}

fn params(schema: &Value) -> String {
    let req: Vec<&str> = schema["required"].as_array().map(|r| r.iter().filter_map(|v| v.as_str()).collect()).unwrap_or_default();
    schema["properties"].as_object().map(|p| {
        p.iter().map(|(k, v)| {
            let ty = v["type"].as_str().unwrap_or("any");
            format!("{k}{}: {ty}", if req.contains(&k.as_str()) { "" } else { "?" })
        }).collect::<Vec<_>>().join(", ")
    }).unwrap_or_default()
}

fn subgroup(local: &str) -> &str {
    local.split('_').next().unwrap_or(local)
}

/// Markdown for one server's tools.
fn render(server: &str, tools: &[&Value], sig: &str) -> String {
    let mut by_sub: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for t in tools {
        let local = split_name(t["name"].as_str().unwrap_or("")).map(|(_, l)| l).unwrap_or("");
        let key = if tools.len() > SPLIT_ABOVE { subgroup(local).to_string() } else { "tools".into() };
        by_sub.entry(key).or_default().push(t);
    }
    let mut out = format!("{AUTO_MARKER} {sig} -->\n# {server}\n\nCall with call_tool(name, arguments). Names below are the full tool names.\n");
    for (sub, ts) in by_sub {
        out.push_str(&format!("\n## {sub}\n\n"));
        for t in ts {
            out.push_str(&format!(
                "- {}({}): {}\n",
                t["name"].as_str().unwrap_or(""),
                params(&t["input_schema"]),
                short(t["description"].as_str().unwrap_or(""), 140)
            ));
        }
    }
    out
}

pub struct Groups {
    /// server -> markdown
    docs: BTreeMap<String, String>,
    /// server -> the server's own usage instructions (moved out of the system prompt)
    notes: BTreeMap<String, String>,
}

impl Groups {
    /// Build from the client's MCP tools, (re)writing auto-generated files when the tool set changed.
    pub fn sync(tools: &[Value]) -> Self {
        let mut by_server: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
        for t in tools {
            if let Some((server, _)) = t["name"].as_str().and_then(split_name) {
                by_server.entry(server.to_string()).or_default().push(t);
            }
        }
        let _ = std::fs::create_dir_all(dir());
        let mut docs = BTreeMap::new();
        for (server, ts) in by_server {
            let mut names: Vec<&str> = ts.iter().filter_map(|t| t["name"].as_str()).collect();
            names.sort();
            let sig = format!("{} tools {:016x}", names.len(), fxhash(&names.join(",")));
            let path = dir().join(format!("{server}.md"));
            let existing = std::fs::read_to_string(&path).ok();
            let doc = match existing {
                // Hand-edited (marker removed) or unchanged: keep it.
                Some(e) if !e.starts_with(AUTO_MARKER) || e.lines().next().is_some_and(|l| l.contains(&sig)) => e,
                _ => {
                    let d = render(&server, &ts, &sig);
                    let _ = std::fs::write(&path, &d);
                    d
                }
            };
            docs.insert(server, doc);
        }
        Groups { docs, notes: BTreeMap::new() }
    }

    /// Attach MCP server instructions; they are shown with the server's group instead of
    /// sitting in every prompt.
    pub fn with_notes(mut self, notes: Vec<(String, String)>) -> Self {
        self.notes = notes.into_iter().filter(|(s, _)| self.docs.contains_key(s)).collect();
        self
    }

    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    /// One line per group for the tool_group description: "rea (inspect, trace, …)".
    pub fn index(&self) -> String {
        self.docs.iter().map(|(server, doc)| {
            let subs: Vec<&str> = doc.lines().filter_map(|l| l.strip_prefix("## ")).filter(|s| *s != "tools").collect();
            if subs.is_empty() { format!("- {server}") } else { format!("- {server} (subgroups: {})", subs.iter().map(|s| format!("{server}/{s}")).collect::<Vec<_>>().join(", ")) }
        }).collect::<Vec<_>>().join("\n")
    }

    /// The markdown for "server" (whole file) or "server/sub" (that section).
    pub fn segment(&self, group: &str) -> String {
        let (server, sub) = group.split_once('/').unwrap_or((group, ""));
        let Some(doc) = self.docs.get(server.trim()) else {
            return format!("No group '{group}'. Groups:\n{}", self.index());
        };
        let notes = self.notes.get(server.trim());
        if sub.is_empty() {
            let body = doc.lines().filter(|l| !l.starts_with(AUTO_MARKER)).collect::<Vec<_>>().join("\n");
            return match notes {
                Some(n) => format!("Usage notes from the {server} server:\n{n}\n\n{body}"),
                None => body,
            };
        }
        let mut out = Vec::new();
        let mut on = false;
        for l in doc.lines() {
            if let Some(h) = l.strip_prefix("## ") {
                on = h.trim() == sub.trim();
            }
            if on {
                out.push(l);
            }
        }
        if out.is_empty() {
            format!("No subgroup '{sub}' in {server}. Groups:\n{}", self.index())
        } else if notes.is_some() {
            format!("{}\n\n(Usage notes for {server}: tool_group(\"{server}\").)", out.join("\n"))
        } else {
            out.join("\n")
        }
    }

    /// The two meta tools, in Anthropic form.
    pub fn meta_tools(&self) -> Vec<Value> {
        vec![
            json!({"name": "tool_group", "description": format!("Show the tools in a group before using them (one call per group you need). Groups:\n{}", self.index()),
                   "input_schema": {"type": "object", "properties": {"group": {"type": "string", "description": "e.g. rea or rea/inspect"}}, "required": ["group"]}}),
            json!({"name": "call_tool", "description": "Call a tool listed by tool_group.",
                   "input_schema": {"type": "object", "properties": {"name": {"type": "string", "description": "Full tool name from tool_group"}, "arguments": {"type": "object"}}, "required": ["name", "arguments"]}}),
        ]
    }
}

/// VS Code's tools (OpenAI form, MCP ones named `mcp_<server>_<tool>`): the MCP tools as groups,
/// a map from the groups' names (`mcp__<server>__<tool>`) back to VS Code's, and the other tools.
pub fn from_openai(tools: &[Value]) -> (Groups, std::collections::HashMap<String, String>, Vec<Value>) {
    let mut mcp = Vec::new();
    let mut names = std::collections::HashMap::new();
    let mut rest = Vec::new();
    for t in tools {
        let name = t["function"]["name"].as_str().unwrap_or("");
        match name.strip_prefix("mcp_").and_then(|r| r.split_once('_')) {
            Some((server, tool)) if !server.is_empty() && !tool.is_empty() => {
                let g = format!("mcp__{server}__{tool}");
                mcp.push(json!({"name": g, "description": t["function"]["description"], "input_schema": t["function"]["parameters"]}));
                names.insert(g, name.to_string());
            }
            _ => rest.push(t.clone()),
        }
    }
    (Groups::sync(&mcp), names, rest)
}

/// The meta tools in OpenAI form.
pub fn meta_tools_openai(g: &Groups) -> Vec<Value> {
    g.meta_tools()
        .into_iter()
        .map(|t| json!({"type": "function", "function": {"name": t["name"], "description": t["description"], "parameters": t["input_schema"]}}))
        .collect()
}

fn fxhash(s: &str) -> u64 {
    s.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

/// call_tool(name, arguments) -> the real tool_use the client knows.
pub fn rewrite_calls(msg: &mut Value) {
    if let Some(c) = msg["content"].as_array_mut() {
        for b in c.iter_mut().filter(|b| b["type"] == "tool_use" && b["name"] == "call_tool") {
            let name = b["input"]["name"].clone();
            let args = b["input"].get("arguments").cloned().unwrap_or(json!({}));
            b["name"] = name;
            b["input"] = if args.is_object() { args } else { json!({}) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str) -> Value {
        json!({"name": name, "description": "Does a thing. More text.", "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}})
    }

    #[test]
    fn groups_split_and_segment() {
        let mut tools: Vec<Value> = (0..14).map(|i| tool(&format!("mcp__srv__inspect_x{i}"))).collect();
        tools.push(tool("mcp__srv__trace_y"));
        let docs: BTreeMap<String, String> = [("srv".to_string(), render("srv", &tools.iter().collect::<Vec<_>>(), "sig"))].into();
        let g = Groups { docs, notes: BTreeMap::new() };
        assert!(g.index().contains("srv/inspect"));
        let seg = g.segment("srv/trace");
        assert!(seg.contains("mcp__srv__trace_y(path: string): Does a thing"));
        assert!(!seg.contains("inspect_x0"));
        assert!(g.segment("nope").starts_with("No group"));
    }

    #[test]
    fn vscode_mcp_tools_become_groups() {
        let t = |n: &str| json!({"type": "function", "function": {"name": n, "description": "Does a thing.", "parameters": {"type": "object", "properties": {}}}});
        let (_, names, rest) = from_openai(&[t("read_file"), t("mcp_zz-test-srv_open_binary")]);
        assert_eq!(rest.len(), 1);
        assert_eq!(names["mcp__zz-test-srv__open_binary"], "mcp_zz-test-srv_open_binary");
        let _ = std::fs::remove_file(dir().join("zz-test-srv.md"));
    }

    #[test]
    fn call_tool_rewrites_to_real_tool() {
        let mut m = json!({"content": [{"type": "tool_use", "id": "t", "name": "call_tool", "input": {"name": "mcp__srv__trace_y", "arguments": {"path": "/x"}}}]});
        rewrite_calls(&mut m);
        assert_eq!(m["content"][0]["name"], "mcp__srv__trace_y");
        assert_eq!(m["content"][0]["input"]["path"], "/x");
    }
}
