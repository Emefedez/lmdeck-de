//! OpenAI Responses API (`POST /v1/responses`, what Codex speaks) on top of the backends' Chat
//! Completions. The request becomes chat messages (instructions, input items, tools) and the
//! backend's chat stream becomes Responses SSE events. The event and item shapes follow DeepSeek's
//! deepseek-recipe (MIT), which converts the same protocol for its own backends.
//!
//! - Custom (free-form) tools such as Codex's `apply_patch` are offered to the model as function
//!   tools with one string parameter, `input`, and their calls go back as `custom_tool_call` items.
//! - Hosted tools the backend cannot run (web search, file search, …) are dropped.
//! - `server.local_tools` applies as on /v1/messages: anything but "all" drops the MCP tools
//!   (Codex sends each MCP server as a `namespace` tool; 9 servers came to 284 tools, ~83k tokens).
//!   "groups" (MCP on demand) is /v1/messages only, so it drops them here too.
//! - Text and reasoning stream as they arrive; tool calls are emitted as whole items when the
//!   backend has finished them.

use crate::anthropic::sse;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};

pub struct Converted {
    /// Chat Completions body for the backend.
    pub body: Value,
    /// The conversation as chat messages without the leading system message (kept for
    /// `previous_response_id`).
    pub history: Vec<Value>,
    pub custom_tools: HashSet<String>,
    /// Function name -> namespace, for tools declared inside a namespace.
    pub namespaces: HashMap<String, String>,
    pub dropped_tools: Vec<String>,
    /// Request settings the response object reports.
    pub echo: Value,
}

fn text_of_blocks(blocks: &[Value]) -> (String, Vec<Value>) {
    // (plain text, chat content parts when there are images)
    let mut parts = Vec::new();
    let mut has_image = false;
    for bl in blocks {
        match bl.get("type").and_then(|t| t.as_str()) {
            Some("input_text") | Some("output_text") | Some("text") => {
                parts.push(json!({"type": "text", "text": bl.get("text").and_then(|t| t.as_str()).unwrap_or("")}))
            }
            Some("input_image") => match bl.get("image_url").and_then(|u| u.as_str()).filter(|u| !u.is_empty()) {
                Some(url) => {
                    has_image = true;
                    parts.push(json!({"type": "image_url", "image_url": {"url": url}}));
                }
                None => parts.push(json!({"type": "text", "text": "[image by file id: not supported here]"})),
            },
            Some("input_file") => parts.push(json!({"type": "text", "text": "[file attachment: not supported here]"})),
            _ => {}
        }
    }
    let text = parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n");
    (text, if has_image { parts } else { Vec::new() })
}

fn content_value(c: Option<&Value>) -> (String, Vec<Value>) {
    match c {
        Some(Value::String(s)) => (s.clone(), Vec::new()),
        Some(Value::Array(a)) => text_of_blocks(a),
        _ => (String::new(), Vec::new()),
    }
}

/// Convert a Responses request. `prior` = the stored conversation of `previous_response_id`.
pub fn to_openai(b: &Value, model_id: &str, prior: &[Value], mode: crate::anthropic::ToolMode, rules: &crate::toolrules::Rules) -> Result<Converted, String> {
    let keep_mcp = mode == crate::anthropic::ToolMode::All;
    // Leading system/developer text (instructions first) becomes one system message: several
    // chat templates refuse a system message anywhere but first.
    let mut system: Vec<String> = Vec::new();
    if let Some(s) = b.get("instructions").and_then(|s| s.as_str()).filter(|s| !s.trim().is_empty()) {
        system.push(s.to_string());
    }
    let mut history: Vec<Value> = prior.to_vec();
    let mut pending: Option<Value> = None; // assistant message collecting text + tool calls
    let flush = |pending: &mut Option<Value>, history: &mut Vec<Value>| {
        if let Some(p) = pending.take() {
            history.push(p);
        }
    };

    let items: Vec<Value> = match b.get("input") {
        Some(Value::String(s)) => vec![json!({"role": "user", "content": s})],
        Some(Value::Array(a)) => a.clone(),
        None | Some(Value::Null) => Vec::new(),
        _ => return Err("input should be a string or a list of input items".into()),
    };
    for it in &items {
        let ty = it.get("type").and_then(|t| t.as_str()).unwrap_or(if it.get("role").is_some() { "message" } else { "" });
        match ty {
            "message" => {
                let role = it.get("role").and_then(|r| r.as_str()).unwrap_or("user");
                let (text, parts) = content_value(it.get("content"));
                match role {
                    "system" | "developer" => {
                        if history.is_empty() && pending.is_none() {
                            system.push(text);
                        } else {
                            flush(&mut pending, &mut history);
                            history.push(json!({"role": "user", "content": format!("[system note]\n{text}")}));
                        }
                    }
                    "assistant" => {
                        flush(&mut pending, &mut history);
                        pending = Some(json!({"role": "assistant", "content": text}));
                    }
                    _ => {
                        flush(&mut pending, &mut history);
                        let content = if parts.is_empty() { json!(text) } else { json!(parts) };
                        history.push(json!({"role": "user", "content": content}));
                    }
                }
            }
            "function_call" | "custom_tool_call" => {
                let call_id = it.get("call_id").and_then(|c| c.as_str()).unwrap_or("call_0");
                let name = it.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let arguments = if ty == "custom_tool_call" {
                    json!({"input": it.get("input").and_then(|i| i.as_str()).unwrap_or("")}).to_string()
                } else {
                    it.get("arguments").and_then(|a| a.as_str()).unwrap_or("{}").to_string()
                };
                let p = pending.get_or_insert_with(|| json!({"role": "assistant", "content": Value::Null}));
                if p.get("tool_calls").is_none() {
                    p["tool_calls"] = json!([]);
                }
                p["tool_calls"].as_array_mut().unwrap().push(json!({
                    "id": call_id, "type": "function", "function": {"name": name, "arguments": arguments}
                }));
            }
            "function_call_output" | "custom_tool_call_output" => {
                flush(&mut pending, &mut history);
                let (text, _) = content_value(it.get("output"));
                history.push(json!({
                    "role": "tool",
                    "tool_call_id": it.get("call_id").and_then(|c| c.as_str()).unwrap_or("call_0"),
                    "content": text,
                }));
            }
            // Past reasoning is not replayed to local models (as on /v1/messages); other item
            // types (web search calls, …) have nothing a local backend could use.
            _ => {}
        }
    }
    flush(&mut pending, &mut history);
    // An assistant message with neither text nor calls is not valid chat input.
    history.retain(|m| m["role"] != "assistant" || m.get("tool_calls").is_some() || m["content"].as_str().is_some_and(|s| !s.is_empty()));

    let mut messages = Vec::new();
    if !system.is_empty() {
        messages.push(json!({"role": "system", "content": system.join("\n\n")}));
    }
    messages.extend(history.iter().cloned());

    let mut o = json!({
        "model": model_id,
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    if let Some(n) = b.get("max_output_tokens").and_then(|n| n.as_u64()) {
        o["max_tokens"] = json!(n);
    }
    for k in ["temperature", "top_p", "parallel_tool_calls"] {
        if let Some(v) = b.get(k).filter(|v| !v.is_null()) {
            o[k] = v.clone();
        }
    }

    let mut custom_tools = HashSet::new();
    let mut namespaces = HashMap::new();
    let mut dropped_tools = Vec::new();
    let mut tools = Vec::new();
    let function = |t: &Value| {
        json!({"type": "function", "function": {
            "name": t.get("name").cloned().unwrap_or(json!("")),
            "description": t.get("description").cloned().unwrap_or(json!("")),
            "parameters": t.get("parameters").filter(|p| !p.is_null()).cloned().unwrap_or(json!({"type": "object", "properties": {}})),
        }})
    };
    for mut t in b.get("tools").and_then(|t| t.as_array()).cloned().unwrap_or_default() {
        let tname = t.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
        let is_ns = t.get("type").and_then(|x| x.as_str()) == Some("namespace");
        if !keep_mcp && is_ns {
            // Only the functions in server.tool_allow survive a namespace.
            if let Some(inner) = t.get_mut("tools").and_then(|x| x.as_array_mut()) {
                inner.retain(|i| rules.allows(&[&crate::toolrules::codex_canonical(&tname, i["name"].as_str().unwrap_or(""))]));
            }
            if t["tools"].as_array().is_none_or(|i| i.is_empty()) {
                dropped_tools.push(tname);
                continue;
            }
        } else if !keep_mcp && tname.starts_with("mcp__") && !rules.allows(&[&tname]) {
            dropped_tools.push(tname);
            continue;
        }
        match t.get("type").and_then(|x| x.as_str()) {
            Some("function") => tools.push(function(&t)),
            Some("custom") => {
                let name = t.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                let desc = t.get("description").and_then(|d| d.as_str()).unwrap_or("");
                tools.push(json!({"type": "function", "function": {
                    "name": name,
                    "description": format!("{desc}\n\nPut the complete text for this tool in the `input` argument."),
                    "parameters": {"type": "object", "properties": {"input": {"type": "string", "description": "The complete input text."}}, "required": ["input"]},
                }}));
                custom_tools.insert(name);
            }
            Some("namespace") => {
                let ns = t.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                for inner in t.get("tools").and_then(|x| x.as_array()).cloned().unwrap_or_default() {
                    if inner.get("type").and_then(|x| x.as_str()) == Some("function") {
                        if let Some(n) = inner.get("name").and_then(|n| n.as_str()) {
                            namespaces.insert(n.to_string(), ns.clone());
                        }
                        tools.push(function(&inner));
                    } else {
                        dropped_tools.push(format!("{ns}.{}", inner.get("name").and_then(|n| n.as_str()).unwrap_or("?")));
                    }
                }
            }
            other => dropped_tools.push(t.get("name").and_then(|n| n.as_str()).unwrap_or(other.unwrap_or("?")).to_string()),
        }
    }
    if !tools.is_empty() {
        o["tools"] = json!(tools);
        match b.get("tool_choice") {
            Some(Value::String(s)) if ["none", "auto", "required"].contains(&s.as_str()) => o["tool_choice"] = json!(s),
            Some(Value::Object(c)) => {
                if let Some(n) = c.get("name").and_then(|n| n.as_str()) {
                    o["tool_choice"] = json!({"type": "function", "function": {"name": n}});
                }
            }
            _ => {}
        }
    }
    match b.pointer("/reasoning/effort").and_then(|e| e.as_str()) {
        Some("none") | Some("minimal") => o["chat_template_kwargs"] = json!({"enable_thinking": false}),
        Some(_) => o["chat_template_kwargs"] = json!({"enable_thinking": true}),
        None => {}
    }
    match b.pointer("/text/format/type").and_then(|t| t.as_str()) {
        Some("json_schema") => {
            let f = &b["text"]["format"];
            o["response_format"] = json!({"type": "json_schema", "json_schema": {
                "name": f.get("name").cloned().unwrap_or(json!("output")),
                "schema": f.get("schema").cloned().unwrap_or(json!({})),
                "strict": f.get("strict").cloned().unwrap_or(json!(false)),
            }});
        }
        Some("json_object") => o["response_format"] = json!({"type": "json_object"}),
        _ => {}
    }

    let mut echo = Map::new();
    for k in ["instructions", "max_output_tokens", "temperature", "top_p", "tools", "tool_choice", "parallel_tool_calls", "previous_response_id", "reasoning", "text", "store", "metadata", "user"] {
        if let Some(v) = b.get(k) {
            echo.insert(k.into(), v.clone());
        }
    }
    Ok(Converted { body: o, history, custom_tools, namespaces, dropped_tools, echo: Value::Object(echo) })
}

#[derive(PartialEq, Clone, Copy)]
enum Kind {
    Message,
    Reasoning,
}

struct Open {
    kind: Kind,
    id: String,
    index: usize,
    text: String,
}

/// Turns OpenAI chat streaming chunks into Responses SSE events.
pub struct RStream {
    pub id: String,
    pub model: String,
    created_at: u64,
    seq: usize,
    echo: Value,
    custom: HashSet<String>,
    namespaces: HashMap<String, String>,
    output: Vec<Value>,
    open: Option<Open>,
    tools: Vec<(String, String, String)>, // per backend tool index: call id, name, arguments
    pub stop_reason: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub text: String,
    saw_any: bool,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl RStream {
    pub fn new(model: &str, c: &Converted) -> Self {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        Self {
            id: format!("resp_lmdeck_{nanos:x}"),
            model: model.to_string(),
            created_at: now_secs(),
            seq: 0,
            echo: c.echo.clone(),
            custom: c.custom_tools.clone(),
            namespaces: c.namespaces.clone(),
            output: Vec::new(),
            open: None,
            tools: Vec::new(),
            stop_reason: None,
            input_tokens: 0,
            output_tokens: 0,
            text: String::new(),
            saw_any: false,
        }
    }

    fn ev(&mut self, ty: &str, mut data: Value) -> String {
        data["type"] = json!(ty);
        data["sequence_number"] = json!(self.seq);
        self.seq += 1;
        sse(ty, &data)
    }

    fn incomplete(&self) -> bool {
        self.stop_reason.as_deref() == Some("length")
    }

    /// The response object: `status` "in_progress" while streaming, then completed / incomplete.
    pub fn response(&self, status: &str) -> Value {
        let mut r = json!({
            "id": self.id, "object": "response", "created_at": self.created_at, "status": status,
            "model": self.model, "output": self.output, "error": null,
            "incomplete_details": if status == "incomplete" { json!({"reason": "max_output_tokens"}) } else { Value::Null },
            "usage": if status == "in_progress" { Value::Null } else { json!({
                "input_tokens": self.input_tokens, "input_tokens_details": {"cached_tokens": 0},
                "output_tokens": self.output_tokens, "output_tokens_details": {"reasoning_tokens": 0},
                "total_tokens": self.input_tokens + self.output_tokens,
            })},
            "parallel_tool_calls": true, "tool_choice": "auto", "tools": [], "store": false,
            "temperature": 1.0, "top_p": 1.0, "text": {"format": {"type": "text"}}, "truncation": "disabled",
        });
        if let (Some(r), Some(e)) = (r.as_object_mut(), self.echo.as_object()) {
            for (k, v) in e {
                r.insert(k.clone(), v.clone());
            }
        }
        if status != "in_progress" {
            r["completed_at"] = json!(now_secs());
        }
        r
    }

    pub fn start(&mut self) -> String {
        let r = self.response("in_progress");
        let mut out = self.ev("response.created", json!({"response": r}));
        let r = self.response("in_progress");
        out.push_str(&self.ev("response.in_progress", json!({"response": r})));
        out
    }

    fn close(&mut self, out: &mut String) {
        let Some(o) = self.open.take() else { return };
        let item = match o.kind {
            Kind::Message => {
                let part = json!({"type": "output_text", "text": o.text, "annotations": [], "logprobs": []});
                out.push_str(&self.ev("response.output_text.done", json!({"item_id": o.id, "output_index": o.index, "content_index": 0, "text": o.text, "logprobs": []})));
                out.push_str(&self.ev("response.content_part.done", json!({"item_id": o.id, "output_index": o.index, "content_index": 0, "part": part})));
                let phase = if self.tools.iter().any(|t| !t.1.is_empty()) { "commentary" } else { "final_answer" };
                json!({"type": "message", "id": o.id, "status": "completed", "role": "assistant", "phase": phase, "content": [part]})
            }
            Kind::Reasoning => {
                let part = json!({"type": "reasoning_text", "text": o.text});
                out.push_str(&self.ev("response.reasoning_text.done", json!({"item_id": o.id, "output_index": o.index, "content_index": 0, "text": o.text})));
                out.push_str(&self.ev("response.content_part.done", json!({"item_id": o.id, "output_index": o.index, "content_index": 0, "part": part})));
                json!({"type": "reasoning", "id": o.id, "status": "completed", "summary": [], "content": [part]})
            }
        };
        out.push_str(&self.ev("response.output_item.done", json!({"output_index": o.index, "item": item})));
        self.output.push(item);
    }

    fn ensure(&mut self, kind: Kind, out: &mut String) {
        if self.open.as_ref().is_some_and(|o| o.kind == kind) {
            return;
        }
        self.close(out);
        let index = self.output.len();
        let (id, item, part) = match kind {
            Kind::Message => {
                let id = format!("msg_{}_{index}", &self.id[5..]);
                (id.clone(), json!({"type": "message", "id": id, "status": "in_progress", "role": "assistant", "content": []}),
                 json!({"type": "output_text", "text": "", "annotations": [], "logprobs": []}))
            }
            Kind::Reasoning => {
                let id = format!("rs_{}_{index}", &self.id[5..]);
                (id.clone(), json!({"type": "reasoning", "id": id, "status": "in_progress", "summary": [], "content": []}),
                 json!({"type": "reasoning_text", "text": ""}))
            }
        };
        out.push_str(&self.ev("response.output_item.added", json!({"output_index": index, "item": item})));
        out.push_str(&self.ev("response.content_part.added", json!({"item_id": id, "output_index": index, "content_index": 0, "part": part})));
        self.open = Some(Open { kind, id, index, text: String::new() });
    }

    /// Feed one parsed chat chunk; returns SSE text to send.
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
        for (kind, piece) in [
            (Kind::Reasoning, d.get("reasoning_content").or_else(|| d.get("reasoning")).and_then(|x| x.as_str()).unwrap_or("")),
            (Kind::Message, d.get("content").and_then(|x| x.as_str()).unwrap_or("")),
        ] {
            if piece.is_empty() {
                continue;
            }
            self.saw_any = true;
            self.ensure(kind, &mut out);
            let o = self.open.as_mut().unwrap();
            o.text.push_str(piece);
            if kind == Kind::Message {
                self.text.push_str(piece);
            }
            let (id, index) = (o.id.clone(), o.index);
            let ty = if kind == Kind::Message { "response.output_text.delta" } else { "response.reasoning_text.delta" };
            let mut data = json!({"item_id": id, "output_index": index, "content_index": 0, "delta": piece});
            if kind == Kind::Message {
                data["logprobs"] = json!([]);
            }
            out.push_str(&self.ev(ty, data));
        }
        if let Some(tcs) = d.get("tool_calls").and_then(|x| x.as_array()) {
            for tc in tcs {
                let idx = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(self.tools.len() as u64) as usize;
                while self.tools.len() <= idx {
                    self.tools.push(Default::default());
                }
                let t = &mut self.tools[idx];
                if let Some(id) = tc.get("id").and_then(|x| x.as_str()) {
                    t.0 = id.to_string();
                }
                if let Some(n) = tc.pointer("/function/name").and_then(|x| x.as_str()) {
                    t.1.push_str(n);
                }
                match tc.pointer("/function/arguments") {
                    Some(Value::String(s)) => t.2.push_str(s),
                    Some(Value::Null) | None => {}
                    Some(other) => t.2.push_str(&other.to_string()),
                }
                self.saw_any = true;
            }
        }
        if self.saw_any {
            self.output_tokens = self.output_tokens.max(1);
        }
        out
    }

    /// True when the backend stream ended without a finish reason or any output.
    pub fn failed(&self) -> bool {
        self.stop_reason.is_none() && !self.saw_any
    }

    /// Close the open item, emit the tool calls, then response.completed / response.incomplete.
    pub fn finish(&mut self) -> String {
        let mut out = String::new();
        self.close(&mut out);
        let tools: Vec<_> = self.tools.drain(..).filter(|t| !t.1.is_empty()).collect();
        for (i, (call_id, name, args)) in tools.into_iter().enumerate() {
            let index = self.output.len();
            let call_id = if call_id.is_empty() { format!("call_{}_{i}", &self.id[5..]) } else { call_id };
            if self.custom.contains(&name) {
                let input = serde_json::from_str::<Value>(&args)
                    .ok()
                    .and_then(|v| v.get("input").and_then(|x| x.as_str()).map(str::to_string))
                    .unwrap_or(args);
                let id = format!("ctc_{}_{index}", &self.id[5..]);
                let added = json!({"type": "custom_tool_call", "id": id, "status": "in_progress", "call_id": call_id, "name": name, "input": ""});
                out.push_str(&self.ev("response.output_item.added", json!({"output_index": index, "item": added})));
                out.push_str(&self.ev("response.custom_tool_call_input.delta", json!({"item_id": id, "output_index": index, "delta": input})));
                out.push_str(&self.ev("response.custom_tool_call_input.done", json!({"item_id": id, "output_index": index, "input": input})));
                let item = json!({"type": "custom_tool_call", "id": id, "status": "completed", "call_id": call_id, "name": name, "input": input});
                out.push_str(&self.ev("response.output_item.done", json!({"output_index": index, "item": item})));
                self.output.push(item);
            } else {
                let args = if args.trim().is_empty() { "{}".to_string() } else { args };
                let id = format!("fc_{}_{index}", &self.id[5..]);
                let mut added = json!({"type": "function_call", "id": id, "status": "in_progress", "call_id": call_id, "name": name, "arguments": ""});
                if let Some(ns) = self.namespaces.get(&name) {
                    added["namespace"] = json!(ns);
                }
                out.push_str(&self.ev("response.output_item.added", json!({"output_index": index, "item": added.clone()})));
                out.push_str(&self.ev("response.function_call_arguments.delta", json!({"item_id": id, "output_index": index, "delta": args})));
                out.push_str(&self.ev("response.function_call_arguments.done", json!({"item_id": id, "output_index": index, "arguments": args})));
                let mut item = added;
                item["status"] = json!("completed");
                item["arguments"] = json!(args);
                out.push_str(&self.ev("response.output_item.done", json!({"output_index": index, "item": item.clone()})));
                self.output.push(item);
            }
        }
        let status = if self.incomplete() { "incomplete" } else { "completed" };
        let r = self.response(status);
        out.push_str(&self.ev(if status == "incomplete" { "response.incomplete" } else { "response.completed" }, json!({"response": r})));
        out
    }

    /// End a stream whose backend failed: response.failed carrying `message`.
    pub fn fail(&mut self, message: &str) -> String {
        let mut r = self.response("failed");
        r["error"] = json!({"code": "server_error", "message": message});
        self.ev("response.failed", json!({"response": r}))
    }

    /// The finished response object (non-streaming requests). Call after `finish`.
    pub fn final_response(&self) -> Value {
        self.response(if self.incomplete() { "incomplete" } else { "completed" })
    }

    /// The assistant's turn as chat messages, for `previous_response_id`.
    pub fn as_chat(&self) -> Vec<Value> {
        let text: String = self.output.iter().filter(|i| i["type"] == "message").filter_map(|i| i["content"][0]["text"].as_str()).collect();
        let calls: Vec<Value> = self
            .output
            .iter()
            .filter_map(|i| match i["type"].as_str() {
                Some("function_call") => Some(json!({"id": i["call_id"], "type": "function", "function": {"name": i["name"], "arguments": i["arguments"]}})),
                Some("custom_tool_call") => Some(json!({"id": i["call_id"], "type": "function", "function": {"name": i["name"], "arguments": json!({"input": i["input"]}).to_string()}})),
                _ => None,
            })
            .collect();
        if text.is_empty() && calls.is_empty() {
            return Vec::new();
        }
        let mut m = json!({"role": "assistant", "content": if text.is_empty() { Value::Null } else { json!(text) }});
        if !calls.is_empty() {
            m["tool_calls"] = json!(calls);
        }
        vec![m]
    }
}

/// Conversations kept for `previous_response_id` (in memory, most recent last).
pub struct Store {
    items: std::sync::Mutex<std::collections::VecDeque<(String, Vec<Value>)>>,
}

const STORE_CAP: usize = 32;

impl Store {
    pub const fn new() -> Self {
        Self { items: std::sync::Mutex::new(std::collections::VecDeque::new()) }
    }
    pub fn get(&self, id: &str) -> Option<Vec<Value>> {
        self.items.lock().unwrap().iter().find(|(k, _)| k == id).map(|(_, v)| v.clone())
    }
    pub fn put(&self, id: String, conv: Vec<Value>) {
        let mut q = self.items.lock().unwrap();
        if q.len() >= STORE_CAP {
            q.pop_front();
        }
        q.push_back((id, conv));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(st: &mut RStream, chunks: &[Value]) -> String {
        let mut s = st.start();
        for c in chunks {
            s.push_str(&st.feed(c));
        }
        s.push_str(&st.finish());
        s
    }

    fn events(sse_text: &str) -> Vec<Value> {
        sse_text.split("\n\n").filter_map(|e| e.lines().find_map(|l| l.strip_prefix("data: "))).map(|d| serde_json::from_str(d).unwrap()).collect()
    }

    #[test]
    fn codex_style_request_converts() {
        let b = json!({
            "model": "m", "instructions": "You are Codex.", "store": false, "stream": true,
            "reasoning": {"effort": "medium"},
            "input": [
                {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "sandbox: workspace-write"}]},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "list files"}]},
                {"type": "reasoning", "summary": [], "content": [{"type": "reasoning_text", "text": "think"}]},
                {"type": "function_call", "call_id": "c1", "name": "shell", "arguments": "{\"command\":[\"ls\"]}"},
                {"type": "function_call_output", "call_id": "c1", "output": "a.txt"},
                {"type": "custom_tool_call", "call_id": "c2", "name": "apply_patch", "input": "*** Begin Patch"},
                {"type": "custom_tool_call_output", "call_id": "c2", "output": "ok"},
                {"type": "message", "role": "developer", "content": "late note"}
            ],
            "tools": [
                {"type": "function", "name": "shell", "description": "run", "parameters": {"type": "object"}},
                {"type": "custom", "name": "apply_patch", "description": "patch", "format": {"type": "grammar"}},
                {"type": "web_search"}
            ],
            "tool_choice": "auto"
        });
        let c = to_openai(&b, "backend-id", &[], crate::anthropic::ToolMode::All, &crate::toolrules::Rules::default()).unwrap();
        let m = c.body["messages"].as_array().unwrap();
        assert_eq!(m[0], json!({"role": "system", "content": "You are Codex.\n\nsandbox: workspace-write"}));
        assert_eq!(m[1]["role"], "user");
        assert_eq!(m[2]["tool_calls"][0]["function"]["name"], "shell");
        assert_eq!(m[2]["tool_calls"].as_array().unwrap().len(), 1);
        assert_eq!(m[3], json!({"role": "tool", "tool_call_id": "c1", "content": "a.txt"}));
        assert_eq!(m[4]["tool_calls"][0]["function"]["arguments"], "{\"input\":\"*** Begin Patch\"}");
        assert_eq!(m[5]["role"], "tool");
        assert_eq!(m[6]["role"], "user");
        assert!(m[6]["content"].as_str().unwrap().contains("late note"));
        assert_eq!(c.body["tools"].as_array().unwrap().len(), 2);
        assert_eq!(c.body["tools"][1]["function"]["parameters"]["required"], json!(["input"]));
        assert!(c.custom_tools.contains("apply_patch"));
        assert_eq!(c.dropped_tools, vec!["web_search"]);
        assert_eq!(c.body["chat_template_kwargs"]["enable_thinking"], true);
        assert_eq!(c.body["model"], "backend-id");
    }

    #[test]
    fn lean_drops_mcp_namespaces_and_functions() {
        let b = json!({"model": "m", "input": "hi", "tools": [
            {"type": "function", "name": "exec_command"},
            {"type": "function", "name": "mcp__codex_apps__search_service"},
            {"type": "namespace", "name": "mcp__rea__", "tools": [{"type": "function", "name": "open_binary"}]},
        ]});
        let c = to_openai(&b, "x", &[], crate::anthropic::ToolMode::Lean, &crate::toolrules::Rules::default()).unwrap();
        let names: Vec<&str> = c.body["tools"].as_array().unwrap().iter().map(|t| t["function"]["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["exec_command"]);
        assert_eq!(c.dropped_tools.len(), 2);
        let c = to_openai(&b, "x", &[], crate::anthropic::ToolMode::All, &crate::toolrules::Rules::default()).unwrap();
        assert_eq!(c.body["tools"].as_array().unwrap().len(), 3);
        assert_eq!(c.namespaces.get("open_binary").map(String::as_str), Some("mcp__rea__"));
    }

    #[test]
    fn stream_events_text_reasoning_and_tools() {
        let b = json!({"model": "m", "input": "hi", "tools": [{"type": "custom", "name": "apply_patch"}, {"type": "function", "name": "shell"}]});
        let c = to_openai(&b, "x", &[], crate::anthropic::ToolMode::All, &crate::toolrules::Rules::default()).unwrap();
        let mut st = RStream::new("m", &c);
        let s = feed_all(&mut st, &[
            json!({"choices": [{"delta": {"reasoning_content": "hmm"}}]}),
            json!({"choices": [{"delta": {"content": "Doing it."}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "t0", "function": {"name": "apply_patch", "arguments": "{\"input\": \"*** Begin"}}]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": " Patch\"}"}}]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 1, "id": "t1", "function": {"name": "shell", "arguments": "{\"command\":[\"ls\"]}"}}]}, "finish_reason": "tool_calls"}]}),
            json!({"choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 5}}),
        ]);
        let ev = events(&s);
        let types: Vec<&str> = ev.iter().map(|e| e["type"].as_str().unwrap()).collect();
        assert_eq!(types.first(), Some(&"response.created"));
        assert_eq!(types.last(), Some(&"response.completed"));
        let seqs: Vec<u64> = ev.iter().map(|e| e["sequence_number"].as_u64().unwrap()).collect();
        assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1));
        let done = ev.last().unwrap();
        let out = done["response"]["output"].as_array().unwrap();
        assert_eq!(out.len(), 4);
        assert_eq!(out[0]["type"], "reasoning");
        assert_eq!(out[1]["type"], "message");
        assert_eq!(out[1]["content"][0]["text"], "Doing it.");
        assert_eq!(out[2], json!({"type": "custom_tool_call", "id": out[2]["id"], "status": "completed", "call_id": "t0", "name": "apply_patch", "input": "*** Begin Patch"}));
        assert_eq!(out[3]["type"], "function_call");
        assert_eq!(out[3]["arguments"], "{\"command\":[\"ls\"]}");
        assert_eq!(done["response"]["usage"]["total_tokens"], 15);
        let chat = st.as_chat();
        assert_eq!(chat[0]["tool_calls"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn length_finish_is_incomplete() {
        let c = to_openai(&json!({"model": "m", "input": "hi", "max_output_tokens": 3}), "x", &[], crate::anthropic::ToolMode::All, &crate::toolrules::Rules::default()).unwrap();
        assert_eq!(c.body["max_tokens"], 3);
        let mut st = RStream::new("m", &c);
        let s = feed_all(&mut st, &[json!({"choices": [{"delta": {"content": "abc"}, "finish_reason": "length"}]})]);
        let last = events(&s).pop().unwrap();
        assert_eq!(last["type"], "response.incomplete");
        assert_eq!(last["response"]["incomplete_details"]["reason"], "max_output_tokens");
        assert_eq!(last["response"]["max_output_tokens"], 3);
    }

    #[test]
    fn previous_response_history_comes_first() {
        let prior = vec![json!({"role": "user", "content": "first"}), json!({"role": "assistant", "content": "ok"})];
        let c = to_openai(&json!({"model": "m", "instructions": "sys", "input": "second"}), "x", &prior, crate::anthropic::ToolMode::All, &crate::toolrules::Rules::default()).unwrap();
        let m = c.body["messages"].as_array().unwrap();
        assert_eq!(m.len(), 4);
        assert_eq!(m[0]["role"], "system");
        assert_eq!(m[1]["content"], "first");
        assert_eq!(m[3]["content"], "second");
        assert_eq!(c.history.len(), 3);
    }
}
