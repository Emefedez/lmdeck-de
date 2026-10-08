//! Anthropic Messages API (`/v1/messages`) on top of the OpenAI chat
//! backends, so Claude Code (and T3 Code's Claude driver) can run on local
//! models: `ANTHROPIC_BASE_URL=http://127.0.0.1:11434`.

use serde_json::{json, Value};

/// Anthropic request -> OpenAI chat request (streaming, usage included).
/// How much of Claude Code's tool list a local model gets.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ToolMode {
    /// Everything as sent.
    All,
    /// Drop MCP tools (mcp__*).
    Core,
    /// Core minus tools a local model rarely needs, with descriptions cut to their first
    /// paragraph and parameter docs to one line: far fewer prompt tokens, same schemas.
    Lean,
    /// Lean plus MCP tools on demand through tool groups (see toolgroups.rs).
    Groups,
}

impl ToolMode {
    pub fn parse(s: &str) -> Self {
        match s {
            "all" => ToolMode::All,
            "core" => ToolMode::Core,
            "groups" => ToolMode::Groups,
            _ => ToolMode::Lean,
        }
    }
}

/// Claude Code tools dropped in lean mode (orchestration, scheduling, notebooks, MCP plumbing).
const LEAN_DROP: &[&str] = &[
    "Agent", "Task", "TaskOutput", "TaskStop", "NotebookEdit", "EnterPlanMode", "ExitPlanMode", "EnterWorktree", "ExitWorktree",
    "Skill", "SlashCommand", "ListMcpResourcesTool", "ReadMcpResourceTool", "CronCreate", "CronDelete", "CronList", "Monitor",
    "RemoteTrigger", "ScheduleWakeup", "PushNotification", "SendMessage", "ListAgents", "ToolSearch", "LSP", "SendFeedback",
    "ReportFindings", "Artifact", "ArtifactComments", "ArtifactData", "DesignSync",
];

fn shorten(text: &str, max: usize) -> String {
    let first = text.split("\n\n").next().unwrap_or(text).trim();
    if first.chars().count() <= max {
        first.to_string()
    } else {
        let cut: String = first.chars().take(max).collect();
        format!("{}…", cut.rsplit_once(' ').map(|(a, _)| a).unwrap_or(&cut))
    }
}

/// Lean JSON schema: parameter descriptions cut to one short line, structure untouched.
fn lean_schema(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(m.iter().map(|(k, v)| {
            let v = if k == "description" { v.as_str().map(|s| json!(shorten(s, 160))).unwrap_or_else(|| v.clone()) } else { lean_schema(v) };
            (k.clone(), v)
        }).collect()),
        Value::Array(a) => Value::Array(a.iter().map(lean_schema).collect()),
        other => other.clone(),
    }
}

/// Tools after applying `mode`, still in Anthropic form.
pub fn filter_tools(tools: &[Value], mode: ToolMode) -> Vec<Value> {
    tools.iter().filter(|t| t.get("input_schema").is_some()).filter_map(|t| {
        let name = t.get("name").and_then(|n| n.as_str()).unwrap_or("");
        if mode != ToolMode::All && name.starts_with("mcp__") {
            return None;
        }
        if matches!(mode, ToolMode::Lean | ToolMode::Groups) {
            if LEAN_DROP.contains(&name) {
                return None;
            }
            let mut t = t.clone();
            if let Some(d) = t.get("description").and_then(|d| d.as_str()) {
                t["description"] = json!(shorten(d, 600));
            }
            t["input_schema"] = lean_schema(&t["input_schema"]);
            return Some(t);
        }
        Some(t.clone())
    }).collect()
}

pub fn to_openai(b: &Value, model_id: &str, mode: ToolMode) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    match b.get("system") {
        Some(Value::String(s)) if !s.is_empty() => messages.push(json!({"role": "system", "content": s})),
        Some(Value::Array(blocks)) => {
            let text: Vec<&str> = blocks.iter().filter_map(|x| x.get("text").and_then(|t| t.as_str())).collect();
            if !text.is_empty() {
                messages.push(json!({"role": "system", "content": text.join("\n\n")}));
            }
        }
        _ => {}
    }
    for m in b.get("messages").and_then(|m| m.as_array()).cloned().unwrap_or_default() {
        let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("user");
        let blocks: Vec<Value> = match m.get("content") {
            Some(Value::String(s)) => vec![json!({"type": "text", "text": s})],
            Some(Value::Array(a)) => a.clone(),
            _ => vec![],
        };
        if role == "assistant" {
            let mut text = String::new();
            let mut calls = Vec::new();
            for bl in &blocks {
                match bl.get("type").and_then(|t| t.as_str()) {
                    Some("text") => text.push_str(bl.get("text").and_then(|t| t.as_str()).unwrap_or("")),
                    Some("tool_use") => calls.push(json!({
                        "id": bl.get("id").cloned().unwrap_or(json!("call_0")),
                        "type": "function",
                        "function": {
                            "name": bl.get("name").cloned().unwrap_or(json!("")),
                            "arguments": bl.get("input").map(|i| i.to_string()).unwrap_or_else(|| "{}".into()),
                        }
                    })),
                    _ => {} // thinking / redacted_thinking are not replayed to local models
                }
            }
            let mut o = json!({"role": "assistant", "content": if text.is_empty() && !calls.is_empty() { Value::Null } else { json!(text) }});
            if !calls.is_empty() {
                o["tool_calls"] = json!(calls);
            }
            messages.push(o);
            continue;
        }
        // user: tool_result blocks become separate tool messages (before the rest).
        let mut parts: Vec<Value> = Vec::new();
        for bl in &blocks {
            match bl.get("type").and_then(|t| t.as_str()) {
                Some("tool_result") => {
                    let content = match bl.get("content") {
                        Some(Value::String(s)) => s.clone(),
                        Some(Value::Array(a)) => a.iter().filter_map(|x| x.get("text").and_then(|t| t.as_str())).collect::<Vec<_>>().join("\n"),
                        _ => String::new(),
                    };
                    let content = if bl.get("is_error").and_then(|e| e.as_bool()) == Some(true) { format!("Error: {content}") } else { content };
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": bl.get("tool_use_id").cloned().unwrap_or(json!("call_0")),
                        "content": content,
                    }));
                }
                Some("text") => parts.push(json!({"type": "text", "text": bl.get("text").cloned().unwrap_or(json!(""))})),
                Some("image") => {
                    let src = bl.get("source").cloned().unwrap_or_default();
                    let url = match src.get("type").and_then(|t| t.as_str()) {
                        Some("base64") => format!(
                            "data:{};base64,{}",
                            src.get("media_type").and_then(|m| m.as_str()).unwrap_or("image/png"),
                            src.get("data").and_then(|d| d.as_str()).unwrap_or("")
                        ),
                        _ => src.get("url").and_then(|u| u.as_str()).unwrap_or("").to_string(),
                    };
                    parts.push(json!({"type": "image_url", "image_url": {"url": url}}));
                }
                _ => {}
            }
        }
        if !parts.is_empty() {
            let only_text = parts.iter().all(|p| p["type"] == "text");
            let content = if only_text {
                json!(parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n"))
            } else {
                json!(parts)
            };
            messages.push(json!({"role": "user", "content": content}));
        }
    }

    let mut o = json!({
        "model": model_id,
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    if let Some(n) = b.get("max_tokens").and_then(|n| n.as_u64()) {
        o["max_tokens"] = json!(n);
    }
    for (from, to) in [("temperature", "temperature"), ("top_p", "top_p"), ("top_k", "top_k"), ("stop_sequences", "stop")] {
        if let Some(v) = b.get(from).filter(|v| !v.is_null()) {
            o[to] = v.clone();
        }
    }
    if let Some(tools) = b.get("tools").and_then(|t| t.as_array()).filter(|t| !t.is_empty()) {
        let t: Vec<Value> = filter_tools(tools, mode)
            .iter()
            .map(|t| {
                json!({"type": "function", "function": {
                    "name": t.get("name").cloned().unwrap_or(json!("")),
                    "description": t.get("description").cloned().unwrap_or(json!("")),
                    "parameters": t.get("input_schema").cloned().unwrap_or(json!({"type": "object"})),
                }})
            })
            .collect();
        if !t.is_empty() {
            o["tools"] = json!(t);
        }
        match b.get("tool_choice").and_then(|c| c.get("type")).and_then(|t| t.as_str()) {
            Some("any") => o["tool_choice"] = json!("required"),
            Some("none") => o["tool_choice"] = json!("none"),
            Some("tool") => {
                o["tool_choice"] = json!({"type": "function", "function": {"name": b["tool_choice"]["name"].clone()}});
            }
            _ => {}
        }
    }
    match b.get("thinking").and_then(|t| t.get("type")).and_then(|t| t.as_str()) {
        Some("enabled") => o["chat_template_kwargs"] = json!({"enable_thinking": true}),
        Some("disabled") => o["chat_template_kwargs"] = json!({"enable_thinking": false}),
        _ => {}
    }
    o
}

/// Remove a markdown section that starts with `heading` (a line) up to the next top-level
/// "# " heading or the end; returns (text without it, removed section).
fn cut_section(text: &str, heading: &str) -> (String, Option<String>) {
    let Some(start) = text.find(heading) else { return (text.to_string(), None) };
    if start > 0 && !text[..start].ends_with('\n') {
        return (text.to_string(), None);
    }
    let after = start + heading.len();
    let end = text[after..].find("\n# ").map(|i| after + i + 1).or_else(|| text[after..].find("\n<").map(|i| after + i + 1)).unwrap_or(text.len());
    (format!("{}{}", &text[..start], &text[end..]), Some(text[start..end].to_string()))
}

/// MCP server instructions ("## server" subsections) from a removed section.
fn mcp_instructions(section: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for part in section.split("\n## ").skip(1) {
        let (name, body) = part.split_once('\n').unwrap_or((part, ""));
        out.push((name.trim().to_string(), body.trim().to_string()));
    }
    out
}

/// Lean prompt for local models: drop Claude Code text that only applies to Claude itself or to
/// tools the local model doesn't get. Returns the MCP server instructions that were removed.
pub fn lean_prompt(b: &mut Value) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut fix = |t: &str| -> String {
        let (t, mcp) = cut_section(t, "# MCP Server Instructions\n");
        if let Some(m) = mcp {
            found.extend(mcp_instructions(&m));
        }
        let (t, _) = cut_section(&t, "# Environment\n - The most recent Claude models");
        t
    };
    let mut walk = |v: &mut Value| match v {
        Value::String(s) => *s = fix(s),
        Value::Array(blocks) => {
            for blk in blocks.iter_mut() {
                if let Some(Value::String(s)) = blk.get_mut("text") {
                    *s = fix(s);
                }
            }
        }
        _ => {}
    };
    if let Some(sys) = b.get_mut("system") {
        walk(sys);
    }
    if let Some(Value::Array(msgs)) = b.get_mut("messages") {
        for m in msgs.iter_mut() {
            if let Some(c) = m.get_mut("content") {
                walk(c);
            }
        }
    }
    // The memory section names a per-project path, which made the system prompt (and its cached
    // prefill) differ per project. Move it next to the per-conversation environment text.
    let mut moved = Vec::new();
    match b.get_mut("system") {
        Some(Value::String(s)) => {
            let (t, m) = cut_section(s, "# Memory\n");
            *s = t;
            moved.extend(m);
        }
        Some(Value::Array(blocks)) => {
            for blk in blocks.iter_mut() {
                if let Some(Value::String(s)) = blk.get_mut("text") {
                    let (t, m) = cut_section(s, "# Memory\n");
                    *s = t;
                    moved.extend(m);
                }
            }
        }
        _ => {}
    }
    if !moved.is_empty() {
        let text = moved.join("\n");
        if let Some(Value::Array(msgs)) = b.get_mut("messages") {
            let target = msgs.iter().position(|m| m["role"] == "system").or_else(|| msgs.iter().position(|m| m["role"] == "user"));
            if let Some(i) = target {
                match msgs[i].get_mut("content") {
                    Some(Value::String(s)) => s.push_str(&format!("\n{text}")),
                    Some(Value::Array(c)) => c.push(json!({"type": "text", "text": text})),
                    _ => {}
                }
            }
        }
    }
    found
}

/// Prompt size by part (~4 chars per token): (system, messages, tools, mcp tools, mcp tool count).
pub fn prompt_breakdown(b: &Value) -> (u64, u64, u64, u64, usize) {
    let len = |v: Option<&Value>| v.map(|v| v.to_string().len() as u64 / 4).unwrap_or(0);
    let tools = b.get("tools").and_then(|t| t.as_array()).cloned().unwrap_or_default();
    let mcp: Vec<&Value> = tools.iter().filter(|t| t.get("name").and_then(|n| n.as_str()).unwrap_or("").starts_with("mcp__")).collect();
    let mcp_tokens = mcp.iter().map(|t| t.to_string().len() as u64 / 4).sum();
    (len(b.get("system")), len(b.get("messages")), len(b.get("tools")), mcp_tokens, mcp.len())
}

/// Rough token estimate for /v1/messages/count_tokens (~4 chars per token).
pub fn count_tokens(b: &Value) -> u64 {
    let mut chars = b.get("system").map(|s| s.to_string().len()).unwrap_or(0);
    chars += b.get("messages").map(|m| m.to_string().len()).unwrap_or(0);
    chars += b.get("tools").map(|t| t.to_string().len()).unwrap_or(0);
    (chars as u64 / 4).max(1)
}

/// Turns OpenAI streaming deltas into Anthropic SSE events.
pub struct Stream {
    pub id: String,
    pub model: String,
    block: Option<(usize, &'static str)>, // (index, "text" | "thinking" | "tool_use")
    next_index: usize,
    tool_slots: Vec<(String, String)>, // per OpenAI tool index: (id, name) started?
    tool_block: Vec<Option<usize>>,
    pub stop_reason: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub saw_tool: bool,
    pub text: String,
    pub thinking: String,
    pub tools_done: Vec<(String, String, String)>, // id, name, args (for non-stream)
    /// Collect tool calls without emitting them (tool groups decides what reaches the client).
    pub hold_tools: bool,
}

pub fn sse(event: &str, data: &Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

impl Stream {
    pub fn new(model: &str) -> Self {
        let id = format!("msg_lmdeck_{:x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
        Self {
            id,
            model: model.to_string(),
            block: None,
            next_index: 0,
            tool_slots: vec![],
            tool_block: vec![],
            stop_reason: None,
            input_tokens: 0,
            output_tokens: 0,
            saw_tool: false,
            text: String::new(),
            thinking: String::new(),
            tools_done: vec![],
            hold_tools: false,
        }
    }

    /// Start another backend round inside the same client message (tool groups):
    /// block indices keep counting, per-round tool state and finish reason reset.
    pub fn next_round(&mut self) -> String {
        let mut out = String::new();
        self.close(&mut out);
        self.tool_slots.clear();
        self.tool_block.clear();
        self.tools_done.clear();
        self.stop_reason = None;
        out
    }

    /// Emit one complete tool_use block (used for held tool calls).
    pub fn emit_tool(&mut self, id: &str, name: &str, input: &Value) -> String {
        let mut out = String::new();
        let i = self.open("tool_use", json!({"type": "tool_use", "id": id, "name": name, "input": {}}), &mut out);
        out.push_str(&sse("content_block_delta", &json!({"type": "content_block_delta", "index": i, "delta": {"type": "input_json_delta", "partial_json": input.to_string()}})));
        self.close(&mut out);
        self.saw_tool = true;
        out
    }

    pub fn start(&self) -> String {
        sse(
            "message_start",
            &json!({"type": "message_start", "message": {
                "id": self.id, "type": "message", "role": "assistant", "model": self.model,
                "content": [], "stop_reason": null, "stop_sequence": null,
                "usage": {"input_tokens": 0, "output_tokens": 0}
            }}),
        )
    }

    fn close(&mut self, out: &mut String) {
        if let Some((i, _)) = self.block.take() {
            out.push_str(&sse("content_block_stop", &json!({"type": "content_block_stop", "index": i})));
        }
    }

    fn open(&mut self, kind: &'static str, block: Value, out: &mut String) -> usize {
        self.close(out);
        let i = self.next_index;
        self.next_index += 1;
        out.push_str(&sse("content_block_start", &json!({"type": "content_block_start", "index": i, "content_block": block})));
        self.block = Some((i, kind));
        i
    }

    /// Feed one parsed OpenAI chunk; returns SSE text to send.
    pub fn feed(&mut self, v: &Value) -> String {
        let mut out = String::new();
        if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
            self.input_tokens = u.get("prompt_tokens").and_then(|x| x.as_u64()).unwrap_or(self.input_tokens);
            self.output_tokens = u.get("completion_tokens").and_then(|x| x.as_u64()).unwrap_or(self.output_tokens);
        }
        let Some(ch) = v.get("choices").and_then(|c| c.get(0)) else { return out };
        if let Some(f) = ch.get("finish_reason").and_then(|f| f.as_str()) {
            self.stop_reason = Some(f.to_string());
        }
        let Some(d) = ch.get("delta") else { return out };
        let thinking = d.get("reasoning_content").or_else(|| d.get("reasoning")).and_then(|x| x.as_str()).unwrap_or("");
        if !thinking.is_empty() {
            if !matches!(self.block, Some((_, "thinking"))) {
                self.open("thinking", json!({"type": "thinking", "thinking": ""}), &mut out);
            }
            let i = self.block.unwrap().0;
            self.thinking.push_str(thinking);
            out.push_str(&sse("content_block_delta", &json!({"type": "content_block_delta", "index": i, "delta": {"type": "thinking_delta", "thinking": thinking}})));
        }
        let text = d.get("content").and_then(|x| x.as_str()).unwrap_or("");
        if !text.is_empty() {
            if !matches!(self.block, Some((_, "text"))) {
                self.open("text", json!({"type": "text", "text": ""}), &mut out);
            }
            let i = self.block.unwrap().0;
            self.text.push_str(text);
            out.push_str(&sse("content_block_delta", &json!({"type": "content_block_delta", "index": i, "delta": {"type": "text_delta", "text": text}})));
        }
        if let Some(tcs) = d.get("tool_calls").and_then(|x| x.as_array()) {
            for tc in tcs {
                let idx = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(self.tool_slots.len() as u64) as usize;
                while self.tool_slots.len() <= idx {
                    self.tool_slots.push(Default::default());
                    self.tool_block.push(None);
                    self.tools_done.push(Default::default());
                }
                if let Some(id) = tc.get("id").and_then(|x| x.as_str()) {
                    self.tool_slots[idx].0 = id.to_string();
                }
                let f = tc.get("function").cloned().unwrap_or_default();
                if let Some(n) = f.get("name").and_then(|x| x.as_str()) {
                    self.tool_slots[idx].1.push_str(n);
                }
                if self.tool_block[idx].is_none() && !self.tool_slots[idx].1.is_empty() {
                    let id = if self.tool_slots[idx].0.is_empty() { format!("toolu_lmdeck_{idx}") } else { self.tool_slots[idx].0.clone() };
                    self.tool_slots[idx].0 = id.clone();
                    let name = self.tool_slots[idx].1.clone();
                    if self.hold_tools {
                        self.tool_block[idx] = Some(usize::MAX);
                    } else {
                        let i = self.open("tool_use", json!({"type": "tool_use", "id": id, "name": name, "input": {}}), &mut out);
                        self.tool_block[idx] = Some(i);
                        self.saw_tool = true;
                    }
                    self.tools_done[idx].0 = id;
                    self.tools_done[idx].1 = name;
                }
                let args = match f.get("arguments") {
                    Some(Value::String(s)) => s.clone(),
                    Some(Value::Null) | None => String::new(),
                    Some(other) => other.to_string(),
                };
                if !args.is_empty() {
                    if let Some(i) = self.tool_block[idx] {
                        self.tools_done[idx].2.push_str(&args);
                        if i != usize::MAX {
                            out.push_str(&sse("content_block_delta", &json!({"type": "content_block_delta", "index": i, "delta": {"type": "input_json_delta", "partial_json": args}})));
                        }
                    }
                }
            }
        }
        if !text.is_empty() || !thinking.is_empty() {
            self.output_tokens = self.output_tokens.max(1);
        }
        out
    }

    /// True when the backend stream ended without a finish reason or any output
    /// (it crashed or was cut off).
    pub fn failed(&self) -> bool {
        self.stop_reason.is_none() && self.text.is_empty() && self.thinking.is_empty() && !self.saw_tool && self.tools_done.is_empty()
    }

    pub fn anthropic_stop(&self) -> &'static str {
        if self.saw_tool {
            return "tool_use";
        }
        match self.stop_reason.as_deref() {
            Some("length") => "max_tokens",
            Some("tool_calls") => "tool_use",
            _ => "end_turn",
        }
    }

    pub fn finish(&mut self) -> String {
        let mut out = String::new();
        self.close(&mut out);
        out.push_str(&sse(
            "message_delta",
            &json!({"type": "message_delta", "delta": {"stop_reason": self.anthropic_stop(), "stop_sequence": null},
                    "usage": {"input_tokens": self.input_tokens, "output_tokens": self.output_tokens}}),
        ));
        out.push_str(&sse("message_stop", &json!({"type": "message_stop"})));
        out
    }

    /// Complete (non-streaming) Anthropic response.
    pub fn message(&self) -> Value {
        let mut content = Vec::new();
        if !self.thinking.is_empty() {
            content.push(json!({"type": "thinking", "thinking": self.thinking, "signature": ""}));
        }
        if !self.text.is_empty() {
            content.push(json!({"type": "text", "text": self.text}));
        }
        for (id, name, args) in self.tools_done.iter().filter(|t| !t.1.is_empty()) {
            let input: Value = serde_json::from_str(args).unwrap_or_else(|_| json!({}));
            content.push(json!({"type": "tool_use", "id": id, "name": name, "input": input}));
        }
        json!({
            "id": self.id, "type": "message", "role": "assistant", "model": self.model,
            "content": content, "stop_reason": self.anthropic_stop(), "stop_sequence": null,
            "usage": {"input_tokens": self.input_tokens, "output_tokens": self.output_tokens}
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_translation() {
        let b = json!({
            "model": "x", "max_tokens": 100, "system": [{"type": "text", "text": "be brief"}],
            "messages": [
                {"role": "user", "content": "weather?"},
                {"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "get", "input": {"city": "Madrid"}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "sunny"}]}
            ],
            "tools": [{"name": "get", "description": "d", "input_schema": {"type": "object"}}],
            "tool_choice": {"type": "any"}
        });
        let o = to_openai(&b, "default_model", ToolMode::All);
        let m = o["messages"].as_array().unwrap();
        assert_eq!(m[0]["role"], "system");
        assert_eq!(m[2]["tool_calls"][0]["function"]["arguments"], "{\"city\":\"Madrid\"}");
        assert_eq!(m[3]["role"], "tool");
        assert_eq!(m[3]["tool_call_id"], "t1");
        assert_eq!(o["tools"][0]["function"]["name"], "get");
        assert_eq!(o["tool_choice"], "required");
    }

    #[test]
    fn stream_events() {
        let mut s = Stream::new("m");
        let mut out = s.start();
        out += &s.feed(&json!({"choices": [{"delta": {"content": "Hi"}}]}));
        out += &s.feed(&json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c1", "function": {"name": "get", "arguments": "{\"a\":1}"}}]}, "finish_reason": "tool_calls"}]}));
        out += &s.finish();
        assert!(out.contains("text_delta"));
        assert!(out.contains("\"type\":\"tool_use\""));
        assert!(out.contains("input_json_delta"));
        assert!(out.contains("\"stop_reason\":\"tool_use\""));
        assert_eq!(s.message()["content"][1]["input"]["a"], 1);
    }
}

#[cfg(test)]
mod lean_prompt_tests {
    use serde_json::json;

    #[test]
    fn drops_mcp_instructions_and_claude_model_notes() {
        let sys = "# Harness\nkeep\n# Environment\n - The most recent Claude models are X\n - fast mode\n# Context management\nkeep too\n";
        let env = "# Environment\nYou have been invoked: cwd /x\n# MCP Server Instructions\n\nintro\n## rea\nREA notes\n## hypruse\nH notes\n<total_tokens>1</total_tokens>\n";
        let mut b = json!({"system": [{"type": "text", "text": sys}], "messages": [{"role": "user", "content": "hi"}, {"role": "system", "content": [{"type": "text", "text": env}]}]});
        let found = super::lean_prompt(&mut b);
        let s = b["system"][0]["text"].as_str().unwrap();
        assert!(s.contains("keep") && s.contains("keep too") && !s.contains("Claude models"));
        let e = b["messages"][1]["content"][0]["text"].as_str().unwrap();
        assert!(e.contains("cwd /x") && e.contains("<total_tokens>") && !e.contains("REA notes"));
        assert_eq!(found, vec![("rea".to_string(), "REA notes".to_string()), ("hypruse".to_string(), "H notes".to_string())]);
    }

    #[test]
    fn memory_section_moves_out_of_the_system_prompt() {
        let sys = "# Harness\nkeep\n# Memory\nmemory at /home/u/.claude/projects/-tmp-x/memory\n# Context management\nctx\n";
        let mut b = json!({"system": [{"type": "text", "text": sys}], "messages": [{"role": "user", "content": "hi"}, {"role": "system", "content": [{"type": "text", "text": "# Environment\ncwd"}]}]});
        super::lean_prompt(&mut b);
        let s = b["system"][0]["text"].as_str().unwrap();
        assert!(!s.contains("-tmp-x") && s.contains("ctx"));
        assert!(b["messages"][1]["content"][1]["text"].as_str().unwrap().contains("-tmp-x/memory"));
    }
}

#[cfg(test)]
mod held_tool_tests {
    use serde_json::json;

    #[test]
    fn held_tools_stream_text_but_not_tool_calls() {
        let mut st = super::Stream::new("m");
        st.hold_tools = true;
        let mut out = st.feed(&json!({"choices": [{"delta": {"content": "hi"}}]}));
        out += &st.feed(&json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c1", "function": {"name": "tool_group", "arguments": "{\"group\":\"rea\"}"}}]}, "finish_reason": "tool_calls"}]}));
        assert!(out.contains("text_delta"));
        assert!(!out.contains("tool_use"));
        assert_eq!(st.tools_done[0].1, "tool_group");
        let next = st.next_round();
        assert!(next.contains("content_block_stop"));
        assert!(st.tools_done.is_empty());
        let t = st.emit_tool("c2", "mcp__x__y", &json!({"a": 1}));
        assert!(t.contains("\"index\":1") && t.contains("input_json_delta"));
        assert_eq!(st.anthropic_stop(), "tool_use");
    }
}

#[cfg(test)]
mod mcp_tool_tests {
    #[test]
    fn core_mode_drops_mcp_tools() {
        let b = serde_json::json!({
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [
                {"name": "Bash", "description": "d", "input_schema": {"type": "object"}},
                {"name": "mcp__rea__open_binary", "description": "d", "input_schema": {"type": "object"}}
            ]
        });
        let core = super::to_openai(&b, "m", super::ToolMode::Core);
        assert_eq!(core["tools"].as_array().unwrap().len(), 1);
        assert_eq!(core["tools"][0]["function"]["name"], "Bash");
        assert_eq!(super::to_openai(&b, "m", super::ToolMode::All)["tools"].as_array().unwrap().len(), 2);
        assert_eq!(super::prompt_breakdown(&b).4, 1);
        let lean = super::to_openai(&b, "m", super::ToolMode::Lean);
        assert_eq!(lean["tools"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn lean_shortens_descriptions_but_keeps_schema() {
        let long = format!("Runs a command.\n\n{}", "detail ".repeat(500));
        let tools = vec![
            serde_json::json!({"name": "Bash", "description": long, "input_schema": {"type": "object", "properties": {"command": {"type": "string", "description": "x ".repeat(300)}}, "required": ["command"]}}),
            serde_json::json!({"name": "Agent", "description": "d", "input_schema": {"type": "object"}}),
        ];
        let out = super::filter_tools(&tools, super::ToolMode::Lean);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["description"], "Runs a command.");
        assert_eq!(out[0]["input_schema"]["required"][0], "command");
        assert!(out[0]["input_schema"]["properties"]["command"]["description"].as_str().unwrap().chars().count() <= 161);
    }
}
