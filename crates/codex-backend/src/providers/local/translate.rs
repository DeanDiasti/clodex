//! Translates between Claude Code's Anthropic Messages requests and the
//! OpenAI Chat Completions surface that local servers such as ollama,
//! mlx_lm and llama.cpp speak, in both directions.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use crate::anthropic::schema::MessagesRequest;
use crate::anthropic::sse::encode_sse_event;
use crate::providers::translate_shared::{ContentBlock, flatten_system_text, normalize_content};

/// Builds the chat-completions request body for `model` from an Anthropic
/// Messages request. Server tools without an `input_schema` are skipped, as
/// are thinking blocks; images become a placeholder because the local servers
/// this targets are text models.
pub fn to_chat_request(req: &MessagesRequest, model: &str, stream: bool) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    if let Some(system) = flatten_system_text(req.extra.get("system")) {
        messages.push(json!({"role": "system", "content": system}));
    }

    for message in &req.messages {
        let blocks = normalize_content(&message.content, json!({}));
        if message.role == "assistant" {
            let mut text = String::new();
            let mut tool_calls: Vec<Value> = Vec::new();
            for block in blocks {
                match block {
                    ContentBlock::Text { text: part } => text.push_str(&part),
                    ContentBlock::ToolUse { id, name, input } => tool_calls.push(json!({
                        "id": id,
                        "type": "function",
                        "function": {"name": name, "arguments": input.to_string()}
                    })),
                    ContentBlock::Thinking { .. }
                    | ContentBlock::Image { .. }
                    | ContentBlock::ToolResult { .. } => {}
                }
            }
            let mut out = Map::new();
            out.insert("role".into(), json!("assistant"));
            out.insert("content".into(), json!(text));
            if !tool_calls.is_empty() {
                out.insert("tool_calls".into(), Value::Array(tool_calls));
            }
            messages.push(Value::Object(out));
        } else {
            let mut text = String::new();
            for block in blocks {
                match block {
                    ContentBlock::Text { text: part } => text.push_str(&part),
                    ContentBlock::Image { .. } => text.push_str("[image omitted]"),
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } => {
                        let mut body = tool_result_text(&content);
                        if is_error == Some(true) && !body.starts_with("Error") {
                            body = format!("Error: {body}");
                        }
                        messages.push(json!({
                            "role": "tool",
                            "tool_call_id": tool_use_id,
                            "content": body
                        }));
                    }
                    ContentBlock::ToolUse { .. } | ContentBlock::Thinking { .. } => {}
                }
            }
            if !text.is_empty() {
                messages.push(json!({"role": "user", "content": text}));
            }
        }
    }

    let mut out = Map::new();
    out.insert("model".into(), json!(model));
    out.insert("messages".into(), Value::Array(messages));
    out.insert("stream".into(), json!(stream));
    if stream {
        out.insert("stream_options".into(), json!({"include_usage": true}));
    }
    if let Some(max_tokens) = req.max_tokens {
        out.insert("max_tokens".into(), json!(max_tokens));
    }
    for key in ["temperature", "top_p"] {
        if let Some(value) = req.extra.get(key) {
            out.insert(key.into(), value.clone());
        }
    }
    if let Some(stop) = req.extra.get("stop_sequences").filter(|v| v.is_array()) {
        out.insert("stop".into(), stop.clone());
    }

    let tools: Vec<Value> = req
        .extra
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| tools.iter().filter_map(function_tool).collect())
        .unwrap_or_default();
    if !tools.is_empty() {
        out.insert("tools".into(), Value::Array(tools));
        if let Some(choice) = tool_choice(req.extra.get("tool_choice")) {
            out.insert("tool_choice".into(), choice);
        }
    }
    Value::Object(out)
}

fn function_tool(tool: &Value) -> Option<Value> {
    let name = tool.get("name")?.as_str()?;
    let parameters = tool.get("input_schema")?;
    let mut function = Map::new();
    function.insert("name".into(), json!(name));
    if let Some(description) = tool.get("description").and_then(Value::as_str) {
        function.insert("description".into(), json!(description));
    }
    function.insert("parameters".into(), without_patterns(parameters));
    Some(json!({"type": "function", "function": function}))
}

/// The schema without its `pattern` keywords. llama.cpp compiles tool schemas
/// into a sampling grammar and its regex converter rejects some patterns: one
/// of Claude Code's own (`^[^\0]*$`) fails the whole request with "failed to
/// parse grammar". A pattern is a validation hint, not structure, so local
/// servers get schemas without them. Only a string-valued `pattern` is a
/// keyword; a property named `pattern` (Grep's) is a schema object and stays.
fn without_patterns(schema: &Value) -> Value {
    match schema {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, value)| !(key.as_str() == "pattern" && value.is_string()))
                .map(|(key, value)| (key.clone(), without_patterns(value)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(without_patterns).collect()),
        other => other.clone(),
    }
}

fn tool_choice(choice: Option<&Value>) -> Option<Value> {
    let choice = choice?;
    match choice.get("type").and_then(Value::as_str)? {
        "auto" => Some(json!("auto")),
        "any" => Some(json!("required")),
        "none" => Some(json!("none")),
        "tool" => choice
            .get("name")
            .and_then(Value::as_str)
            .map(|name| json!({"type": "function", "function": {"name": name}})),
        _ => None,
    }
}

fn tool_result_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| match block.get("type").and_then(Value::as_str) {
                Some("text") => block
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                Some("image") => Some("[image omitted]".to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Rough token estimate for count_tokens: a byte count over four.
pub fn estimate_tokens(chat_request: &Value) -> u64 {
    (chat_request.to_string().len() as u64).div_ceil(4)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Open {
    Text,
    Thinking,
    Tool { slot: i64 },
}

/// Turns a stream of chat-completions chunks into Anthropic SSE events.
#[derive(Debug)]
pub struct StreamTranslator {
    message_id: String,
    model: String,
    estimated_input_tokens: u64,
    started: bool,
    finished: bool,
    next_index: usize,
    open: Option<(usize, Open)>,
    tool_args_len: usize,
    tool_ids: BTreeMap<i64, String>,
    saw_tool: bool,
    finish_reason: Option<String>,
    input_tokens: Option<u64>,
    output_tokens: u64,
    counted_output_tokens: Option<u64>,
}

impl StreamTranslator {
    pub fn new(message_id: String, model: String, estimated_input_tokens: u64) -> Self {
        Self {
            message_id,
            model,
            estimated_input_tokens,
            started: false,
            finished: false,
            next_index: 0,
            open: None,
            tool_args_len: 0,
            tool_ids: BTreeMap::new(),
            saw_tool: false,
            finish_reason: None,
            input_tokens: None,
            output_tokens: 0,
            counted_output_tokens: None,
        }
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// An Anthropic `ping` for a quiet upstream (a long prompt still being
    /// prefilled), preceded by `message_start` if nothing has been sent yet, so
    /// the client sees the stream is alive. Nothing once the message finished.
    pub fn ping(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        self.ensure_started(&mut out);
        emit(&mut out, "ping", &json!({"type": "ping"}));
        out
    }

    /// Feeds one parsed SSE `data:` payload. Returns the Anthropic SSE bytes
    /// it produced, if any.
    pub fn on_chunk(&mut self, chunk: &Value) -> Vec<u8> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        if let Some(error) = chunk.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| error.to_string());
            self.emit_error(&mut out, &message);
            return out;
        }
        if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
            if let Some(prompt) = usage.get("prompt_tokens").and_then(Value::as_u64) {
                self.input_tokens = Some(prompt);
            }
            if let Some(completion) = usage.get("completion_tokens").and_then(Value::as_u64) {
                self.counted_output_tokens = Some(completion);
            }
        }
        let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            return out;
        };
        if let Some(delta) = choice.get("delta") {
            self.on_delta(&mut out, delta);
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(reason.to_string());
        }
        out
    }

    fn on_delta(&mut self, out: &mut Vec<u8>, delta: &Value) {
        for key in ["reasoning_content", "reasoning"] {
            if let Some(text) = delta.get(key).and_then(Value::as_str)
                && !text.is_empty()
            {
                self.ensure_open(out, Open::Thinking);
                let index = self.open_index();
                self.output_tokens += 1;
                emit(
                    out,
                    "content_block_delta",
                    &json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": {"type": "thinking_delta", "thinking": text}
                    }),
                );
            }
        }
        if let Some(text) = delta.get("content").and_then(Value::as_str)
            && !text.is_empty()
        {
            self.ensure_open(out, Open::Text);
            let index = self.open_index();
            self.output_tokens += 1;
            emit(
                out,
                "content_block_delta",
                &json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {"type": "text_delta", "text": text}
                }),
            );
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for (position, call) in calls.iter().enumerate() {
                let slot = call
                    .get("index")
                    .and_then(Value::as_i64)
                    .unwrap_or(position as i64);
                let function = call.get("function");
                let name = function
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty());
                let id = call
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty());
                let already_open =
                    matches!(self.open, Some((_, Open::Tool { slot: open })) if open == slot);
                if !already_open
                    || name.is_some() && id.is_some() && !self.tool_ids.contains_key(&slot)
                {
                    let Some(name) = name else {
                        // An argument fragment for a call that was never opened.
                        continue;
                    };
                    let id = id
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("call_{}_{}", self.message_id, self.next_index));
                    self.close_open(out);
                    self.tool_ids.insert(slot, id.clone());
                    self.open_block(
                        out,
                        Open::Tool { slot },
                        json!({"type": "tool_use", "id": id, "name": name, "input": {}}),
                    );
                    self.saw_tool = true;
                }
                if let Some(arguments) = function
                    .and_then(|f| f.get("arguments"))
                    .and_then(Value::as_str)
                    .filter(|arguments| !arguments.is_empty())
                {
                    let index = self.open_index();
                    self.tool_args_len += arguments.len();
                    self.output_tokens += 1;
                    emit(
                        out,
                        "content_block_delta",
                        &json!({
                            "type": "content_block_delta",
                            "index": index,
                            "delta": {"type": "input_json_delta", "partial_json": arguments}
                        }),
                    );
                }
            }
        }
    }

    /// Closes the stream: the open block, `message_delta` and `message_stop`.
    pub fn finish(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        self.ensure_started(&mut out);
        self.close_open(&mut out);
        let stop_reason = match self.finish_reason.as_deref() {
            Some("length") => "max_tokens",
            Some("tool_calls") => "tool_use",
            _ if self.saw_tool => "tool_use",
            _ => "end_turn",
        };
        let output_tokens = self.counted_output_tokens.unwrap_or(self.output_tokens);
        let mut usage = json!({"output_tokens": output_tokens});
        if let Some(input) = self.input_tokens {
            usage["input_tokens"] = json!(input);
        }
        emit(
            &mut out,
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                "usage": usage
            }),
        );
        emit(&mut out, "message_stop", &json!({"type": "message_stop"}));
        self.finished = true;
        out
    }

    /// Reports an upstream failure. Before any content it is a plain error
    /// event; after content it still closes the message so the client does
    /// not hang.
    pub fn fail(&mut self, message: &str) -> Vec<u8> {
        let mut out = Vec::new();
        self.emit_error(&mut out, message);
        out
    }

    fn emit_error(&mut self, out: &mut Vec<u8>, message: &str) {
        if self.finished {
            return;
        }
        emit(
            out,
            "error",
            &json!({
                "type": "error",
                "error": {"type": "api_error", "message": message}
            }),
        );
        self.finished = true;
    }

    fn ensure_started(&mut self, out: &mut Vec<u8>) {
        if self.started {
            return;
        }
        self.started = true;
        emit(
            out,
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": self.message_id,
                    "type": "message",
                    "role": "assistant",
                    "model": self.model,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {
                        "input_tokens": self.input_tokens.unwrap_or(self.estimated_input_tokens),
                        "output_tokens": 0
                    }
                }
            }),
        );
    }

    fn ensure_open(&mut self, out: &mut Vec<u8>, kind: Open) {
        if matches!(self.open, Some((_, open)) if open == kind) {
            return;
        }
        self.close_open(out);
        let block = match kind {
            Open::Text => json!({"type": "text", "text": ""}),
            Open::Thinking => json!({"type": "thinking", "thinking": "", "signature": ""}),
            Open::Tool { .. } => unreachable!("tool blocks open through open_block"),
        };
        self.open_block(out, kind, block);
    }

    fn open_block(&mut self, out: &mut Vec<u8>, kind: Open, block: Value) {
        self.ensure_started(out);
        let index = self.next_index;
        self.next_index += 1;
        self.open = Some((index, kind));
        self.tool_args_len = 0;
        emit(
            out,
            "content_block_start",
            &json!({"type": "content_block_start", "index": index, "content_block": block}),
        );
    }

    fn open_index(&self) -> usize {
        self.open.map(|(index, _)| index).unwrap_or(0)
    }

    fn close_open(&mut self, out: &mut Vec<u8>) {
        let Some((index, kind)) = self.open.take() else {
            return;
        };
        if matches!(kind, Open::Tool { .. }) && self.tool_args_len == 0 {
            emit(
                out,
                "content_block_delta",
                &json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {"type": "input_json_delta", "partial_json": "{}"}
                }),
            );
        }
        if kind == Open::Thinking {
            emit(
                out,
                "content_block_delta",
                &json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {"type": "signature_delta", "signature": ""}
                }),
            );
        }
        emit(
            out,
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": index}),
        );
    }
}

fn emit(out: &mut Vec<u8>, event: &str, data: &Value) {
    out.extend_from_slice(&encode_sse_event(Some(event), &data.to_string()));
}

/// Splits buffered SSE bytes into complete `data:` payloads, leaving a
/// trailing partial event in `buffer`. `[DONE]` ends the stream.
pub fn drain_sse_payloads(buffer: &mut Vec<u8>) -> (Vec<Value>, bool) {
    let mut payloads = Vec::new();
    let mut done = false;
    while let Some((event_len, sep_len)) = find_event_end(buffer) {
        let event: Vec<u8> = buffer.drain(..event_len + sep_len).collect();
        let text = String::from_utf8_lossy(&event[..event_len]);
        let data: Vec<&str> = text
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim_start)
            .collect();
        if data.is_empty() {
            continue;
        }
        let joined = data.join("\n");
        if joined.trim() == "[DONE]" {
            done = true;
            break;
        }
        match serde_json::from_str::<Value>(&joined) {
            Ok(value) => payloads.push(value),
            Err(error) => payloads.push(json!({"error": {"message": format!(
                "unparseable chunk from the local server: {error}"
            )}})),
        }
    }
    (payloads, done)
}

fn find_event_end(buffer: &[u8]) -> Option<(usize, usize)> {
    let lf = buffer.windows(2).position(|w| w == b"\n\n").map(|p| (p, 2));
    let crlf = buffer
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| (p, 4));
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// Builds the non-streaming Anthropic message from a finished chat response.
pub fn to_anthropic_message(response: &Value, message_id: &str, model: &str) -> Value {
    let message = response
        .pointer("/choices/0/message")
        .cloned()
        .unwrap_or(json!({}));
    let mut content: Vec<Value> = Vec::new();
    for key in ["reasoning_content", "reasoning"] {
        if let Some(text) = message.get(key).and_then(Value::as_str)
            && !text.is_empty()
        {
            content.push(json!({"type": "thinking", "thinking": text, "signature": ""}));
        }
    }
    if let Some(text) = message.get("content").and_then(Value::as_str)
        && !text.is_empty()
    {
        content.push(json!({"type": "text", "text": text}));
    }
    let mut saw_tool = false;
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for (position, call) in calls.iter().enumerate() {
            let Some(name) = call.pointer("/function/name").and_then(Value::as_str) else {
                continue;
            };
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("call_{message_id}_{position}"));
            let input = call
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .and_then(|arguments| serde_json::from_str::<Value>(arguments).ok())
                .filter(Value::is_object)
                .unwrap_or_else(|| json!({}));
            content.push(json!({"type": "tool_use", "id": id, "name": name, "input": input}));
            saw_tool = true;
        }
    }
    let stop_reason = match response
        .pointer("/choices/0/finish_reason")
        .and_then(Value::as_str)
    {
        Some("length") => "max_tokens",
        Some("tool_calls") => "tool_use",
        _ if saw_tool => "tool_use",
        _ => "end_turn",
    };
    let usage = response.get("usage").cloned().unwrap_or(json!({}));
    json!({
        "id": message_id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {
            "input_tokens": usage.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0),
            "output_tokens": usage.get("completion_tokens").and_then(Value::as_u64).unwrap_or(0)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::sse::parse_sse_events;

    fn request(value: Value) -> MessagesRequest {
        serde_json::from_value(value).unwrap()
    }

    fn events(bytes: &[u8]) -> Vec<(String, Value)> {
        parse_sse_events(bytes)
            .into_iter()
            .map(|event| {
                (
                    event.event.unwrap_or_default(),
                    serde_json::from_str(&event.data).unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn system_tools_and_tool_results_translate_to_chat_shape() {
        let req = request(json!({
            "model": "wollf",
            "max_tokens": 64,
            "temperature": 0.2,
            "system": [{"type": "text", "text": "Be brief."}],
            "messages": [
                {"role": "user", "content": "ls please"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "hmm", "signature": ""},
                    {"type": "text", "text": "Listing."},
                    {"type": "tool_use", "id": "call_1", "name": "Bash", "input": {"command": "ls"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_1", "content": [{"type": "text", "text": "a\nb"}]},
                    {"type": "text", "text": "thanks"}
                ]}
            ],
            "tools": [
                {"name": "Bash", "description": "Run", "input_schema": {"type": "object", "properties": {"command": {"type": "string"}}}},
                {"type": "web_search_20250305", "name": "web_search"}
            ],
            "tool_choice": {"type": "auto"}
        }));
        let chat = to_chat_request(&req, "wollf", true);
        assert_eq!(chat["model"], "wollf");
        assert_eq!(chat["stream"], true);
        assert_eq!(chat["stream_options"]["include_usage"], true);
        assert_eq!(chat["max_tokens"], 64);
        assert_eq!(chat["temperature"], 0.2);
        let messages = chat["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "Be brief.");
        assert_eq!(messages[1], json!({"role": "user", "content": "ls please"}));
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[2]["content"], "Listing.");
        assert_eq!(messages[2]["tool_calls"][0]["id"], "call_1");
        assert_eq!(messages[2]["tool_calls"][0]["function"]["name"], "Bash");
        assert_eq!(
            messages[2]["tool_calls"][0]["function"]["arguments"],
            r#"{"command":"ls"}"#
        );
        assert_eq!(
            messages[3],
            json!({"role": "tool", "tool_call_id": "call_1", "content": "a\nb"})
        );
        assert_eq!(messages[4], json!({"role": "user", "content": "thanks"}));
        let tools = chat["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1, "server tools without a schema are skipped");
        assert_eq!(tools[0]["function"]["name"], "Bash");
        assert_eq!(tools[0]["function"]["parameters"]["type"], "object");
        assert_eq!(chat["tool_choice"], "auto");
    }

    #[test]
    fn tool_choice_variants_and_no_tools_omit_the_choice() {
        let base = json!({"model": "m", "messages": [], "tools": [
            {"name": "t", "input_schema": {"type": "object"}}
        ]});
        for (choice, expected) in [
            (json!({"type": "any"}), json!("required")),
            (json!({"type": "none"}), json!("none")),
            (
                json!({"type": "tool", "name": "t"}),
                json!({"type": "function", "function": {"name": "t"}}),
            ),
        ] {
            let mut value = base.clone();
            value["tool_choice"] = choice;
            assert_eq!(
                to_chat_request(&request(value), "m", false)["tool_choice"],
                expected
            );
        }
        let none = to_chat_request(
            &request(json!({"model": "m", "messages": [], "tool_choice": {"type": "any"}})),
            "m",
            false,
        );
        assert!(none.get("tools").is_none());
        assert!(none.get("tool_choice").is_none());
        assert!(none.get("stream_options").is_none());
    }

    #[test]
    fn tool_schemas_lose_string_patterns_but_keep_a_property_named_pattern() {
        let chat = to_chat_request(
            &request(json!({
                "model": "wollf",
                "max_tokens": 8,
                "messages": [{"role": "user", "content": "hi"}],
                "tools": [{"name": "Grep", "input_schema": {
                    "type": "object",
                    "properties": {
                        "pattern": {"type": "string", "pattern": "^.+$"},
                        "paths": {"type": "array", "items": {"type": "string", "pattern": "^[^\\0]*$"}},
                        "id": {"anyOf": [{"const": "latest"}, {"type": "string", "pattern": "^[0-9]+$", "maxLength": 9}]}
                    },
                    "required": ["pattern"]
                }}]
            })),
            "wollf",
            false,
        );
        let parameters = &chat["tools"][0]["function"]["parameters"];
        assert_eq!(
            parameters["properties"]["pattern"],
            json!({"type": "string"})
        );
        assert_eq!(
            parameters["properties"]["paths"]["items"],
            json!({"type": "string"})
        );
        assert_eq!(
            parameters["properties"]["id"]["anyOf"][1],
            json!({"type": "string", "maxLength": 9})
        );
        assert_eq!(parameters["required"], json!(["pattern"]));
        assert!(
            !chat.to_string().contains("\"pattern\":\""),
            "no pattern keyword survives"
        );
    }

    #[test]
    fn a_ping_starts_the_message_once_and_stops_after_the_end() {
        let mut t = StreamTranslator::new("msg_p".into(), "wollf".into(), 27_000);
        let mut bytes = t.ping();
        bytes.extend(t.ping());
        bytes.extend(t.on_chunk(
            &json!({"choices": [{"delta": {"content": "ok"}, "finish_reason": "stop"}]}),
        ));
        bytes.extend(t.finish());
        let names: Vec<String> = events(&bytes).into_iter().map(|(name, _)| name).collect();
        assert_eq!(
            names,
            [
                "message_start",
                "ping",
                "ping",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
        assert_eq!(
            events(&bytes)[0].1["message"]["usage"]["input_tokens"],
            27_000
        );
        assert!(t.ping().is_empty(), "no ping after message_stop");
    }

    #[test]
    fn text_stream_becomes_anthropic_events() {
        let mut t = StreamTranslator::new("msg_1".into(), "wollf".into(), 12);
        let mut bytes =
            t.on_chunk(&json!({"choices": [{"delta": {"role": "assistant", "content": "Hel"}}]}));
        bytes.extend(t.on_chunk(
            &json!({"choices": [{"delta": {"content": "lo"}, "finish_reason": "stop"}]}),
        ));
        bytes.extend(t.on_chunk(
            &json!({"choices": [], "usage": {"prompt_tokens": 40, "completion_tokens": 2}}),
        ));
        bytes.extend(t.finish());
        let events = events(&bytes);
        let names: Vec<&str> = events.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
        assert_eq!(events[0].1["message"]["id"], "msg_1");
        assert_eq!(events[0].1["message"]["model"], "wollf");
        assert_eq!(events[0].1["message"]["usage"]["input_tokens"], 12);
        assert_eq!(events[1].1["content_block"]["type"], "text");
        assert_eq!(events[2].1["delta"]["text"], "Hel");
        assert_eq!(events[3].1["delta"]["text"], "lo");
        assert_eq!(events[5].1["delta"]["stop_reason"], "end_turn");
        assert_eq!(events[5].1["usage"]["output_tokens"], 2);
        assert_eq!(events[5].1["usage"]["input_tokens"], 40);
        assert!(t.is_finished());
        assert!(t.finish().is_empty(), "finish is idempotent");
    }

    #[test]
    fn tool_call_fragments_become_a_tool_use_block() {
        let mut t = StreamTranslator::new("msg_2".into(), "wollf".into(), 1);
        let mut bytes = t.on_chunk(&json!({"choices": [{"delta": {"content": "Running."}}]}));
        bytes.extend(t.on_chunk(&json!({"choices": [{"delta": {"tool_calls": [
            {"index": 0, "id": "call_a", "type": "function", "function": {"name": "Bash", "arguments": "{\"com"}}
        ]}}]})));
        bytes.extend(t.on_chunk(&json!({"choices": [{"delta": {"tool_calls": [
            {"index": 0, "function": {"arguments": "mand\":\"ls\"}"}}
        ]}}]})));
        bytes.extend(
            t.on_chunk(&json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]})),
        );
        bytes.extend(t.finish());
        let events = events(&bytes);
        let names: Vec<&str> = events.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
        let tool = &events[4].1;
        assert_eq!(tool["index"], 1);
        assert_eq!(tool["content_block"]["type"], "tool_use");
        assert_eq!(tool["content_block"]["id"], "call_a");
        assert_eq!(tool["content_block"]["name"], "Bash");
        let json: String = [&events[5].1, &events[6].1]
            .iter()
            .map(|e| e["delta"]["partial_json"].as_str().unwrap())
            .collect();
        assert_eq!(json, r#"{"command":"ls"}"#);
        assert_eq!(events[8].1["delta"]["stop_reason"], "tool_use");
    }

    #[test]
    fn a_whole_tool_call_without_arguments_gets_an_empty_object() {
        let mut t = StreamTranslator::new("msg_3".into(), "wollf".into(), 1);
        let mut bytes = t.on_chunk(&json!({"choices": [{"delta": {"tool_calls": [
            {"id": "", "type": "function", "function": {"name": "TodoRead", "arguments": ""}}
        ]}}]}));
        bytes.extend(t.finish());
        let events = events(&bytes);
        assert_eq!(events[1].1["content_block"]["id"], "call_msg_3_0");
        assert_eq!(events[2].1["delta"]["partial_json"], "{}");
        assert_eq!(events[4].1["delta"]["stop_reason"], "tool_use");
    }

    #[test]
    fn reasoning_becomes_a_thinking_block_before_the_text() {
        let mut t = StreamTranslator::new("msg_4".into(), "wollf".into(), 1);
        let mut bytes =
            t.on_chunk(&json!({"choices": [{"delta": {"reasoning_content": "think"}}]}));
        bytes.extend(t.on_chunk(&json!({"choices": [{"delta": {"content": "answer"}}]})));
        bytes.extend(t.finish());
        let events = events(&bytes);
        let names: Vec<&str> = events.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
        assert_eq!(events[1].1["content_block"]["type"], "thinking");
        assert_eq!(events[3].1["delta"]["type"], "signature_delta");
        assert_eq!(events[5].1["content_block"]["type"], "text");
    }

    #[test]
    fn an_empty_stream_still_closes_the_message() {
        let mut t = StreamTranslator::new("msg_5".into(), "wollf".into(), 3);
        let events = events(&t.finish());
        let names: Vec<&str> = events.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["message_start", "message_delta", "message_stop"]);
    }

    #[test]
    fn an_upstream_error_chunk_becomes_an_error_event() {
        let mut t = StreamTranslator::new("msg_6".into(), "wollf".into(), 3);
        let bytes = t.on_chunk(&json!({"error": {"message": "model not found"}}));
        let events = events(&bytes);
        assert_eq!(events[0].0, "error");
        assert_eq!(events[0].1["error"]["message"], "model not found");
        assert!(t.is_finished());
        assert!(t.finish().is_empty());
    }

    #[test]
    fn sse_payloads_drain_across_chunk_boundaries_and_stop_at_done() {
        let mut buffer = b"data: {\"a\":1}\n\ndata: {\"b\"".to_vec();
        let (first, done) = drain_sse_payloads(&mut buffer);
        assert_eq!(first, vec![json!({"a": 1})]);
        assert!(!done);
        buffer.extend_from_slice(b":2}\r\n\r\n: comment\n\ndata: [DONE]\n\ndata: {\"c\":3}\n\n");
        let (second, done) = drain_sse_payloads(&mut buffer);
        assert_eq!(second, vec![json!({"b": 2})]);
        assert!(done);
    }

    #[test]
    fn non_streaming_response_becomes_an_anthropic_message() {
        let message = to_anthropic_message(
            &json!({
                "choices": [{"message": {"content": "ok", "tool_calls": [
                    {"id": "call_z", "function": {"name": "Read", "arguments": "{\"file_path\":\"a\"}"}},
                    {"id": "", "function": {"name": "TodoRead", "arguments": "not json"}}
                ]}, "finish_reason": "tool_calls"}],
                "usage": {"prompt_tokens": 9, "completion_tokens": 4}
            }),
            "msg_7",
            "wollf",
        );
        assert_eq!(message["stop_reason"], "tool_use");
        assert_eq!(message["usage"]["input_tokens"], 9);
        assert_eq!(message["content"][0]["text"], "ok");
        assert_eq!(message["content"][1]["input"]["file_path"], "a");
        assert_eq!(message["content"][2]["id"], "call_msg_7_1");
        assert_eq!(message["content"][2]["input"], json!({}));
    }

    #[test]
    fn token_estimate_is_a_quarter_of_the_bytes() {
        assert_eq!(estimate_tokens(&json!({"a": "bcdefg"})), 4);
    }
}
