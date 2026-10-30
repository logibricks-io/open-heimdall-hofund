//! A Chat Completions reply as Ollama's: NDJSON lines while streaming, one
//! object when not, and `{"error": ...}` for a failure.

use std::time::{Instant, SystemTime};

use hofund_plugin_sdk::json::{self, Map, Value, json};
use hofund_plugin_sdk::time::format_rfc3339;

/// Ollama's error body.
pub(crate) fn error_body(message: &str) -> Vec<u8> {
    line(&json!({"error": message}))
}

/// `value` as one NDJSON line.
fn line(value: &Value) -> Vec<u8> {
    let mut out = value.to_string().into_bytes();
    out.push(b'\n');
    out
}

/// The time an Ollama reply says it was made.
fn now() -> String {
    format_rfc3339(SystemTime::now())
}

/// What a request with nothing to say answers: the model is loaded.
pub(crate) fn load_reply(model: &str, generate: bool) -> Vec<u8> {
    let mut out = Map::new();
    out.insert("model".to_owned(), json!(model));
    out.insert("created_at".to_owned(), json!(now()));
    out.insert("done".to_owned(), json!(true));
    out.insert("done_reason".to_owned(), json!("load"));
    if generate {
        out.insert("response".to_owned(), json!(""));
    } else {
        out.insert(
            "message".to_owned(),
            json!({"role": "assistant", "content": ""}),
        );
    }
    line(&Value::Object(out))
}

/// The message in a failed reply's body: `error.message`, `error`, the text,
/// or the status's reason.
pub(crate) fn failure_message(status: u16, body: &[u8]) -> String {
    let parsed = json::from_slice::<Value>(body).ok();
    let error = parsed.as_ref().and_then(|value| value.get("error"));
    if let Some(message) = error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .filter(|message| !message.is_empty())
    {
        return message.to_owned();
    }
    match error {
        Some(Value::String(message)) if !message.is_empty() => return message.clone(),
        Some(other @ (Value::Object(_) | Value::Array(_) | Value::Number(_) | Value::Bool(_))) => {
            return other.to_string();
        }
        _ => {}
    }
    let text = String::from_utf8_lossy(body).trim().to_owned();
    if !text.is_empty() {
        return text;
    }
    reason(status).to_owned()
}

/// A status's reason phrase, for a failure with no body.
fn reason(status: u16) -> &'static str {
    match status {
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        408 => "Request Timeout",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "request failed",
    }
}

/// The most tool calls one reply keeps. A call indexed past it is dropped,
/// so a stream's `index` cannot make the writer allocate without bound.
pub(crate) const MAX_CALLS: usize = 128;

/// One tool call as the stream builds it.
#[derive(Default)]
struct ToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// Turns a Chat Completions reply into Ollama's, as its bytes arrive.
pub(crate) struct Writer {
    model: String,
    generate: bool,
    stream: bool,
    thinking_off: bool,
    start: Instant,
    /// A partial SSE line, or the whole reply when it is not a stream.
    buffer: Vec<u8>,
    sse: bool,
    /// An error line ended the reply.
    done: bool,
    content: String,
    thinking: String,
    calls: Vec<ToolCall>,
    finish_reason: String,
    prompt_tokens: u64,
    output_tokens: u64,
    first_token: Option<Instant>,
}

impl Writer {
    /// A writer for `model`'s reply, asked at `start`. `sse` says the reply
    /// is a Chat Completions stream rather than one completion.
    pub(crate) fn new(
        model: &str,
        generate: bool,
        stream: bool,
        thinking_off: bool,
        sse: bool,
        start: Instant,
    ) -> Self {
        Self {
            model: model.to_owned(),
            generate,
            stream,
            thinking_off,
            start,
            buffer: Vec::new(),
            sse,
            done: false,
            content: String::new(),
            thinking: String::new(),
            calls: Vec::new(),
            finish_reason: String::new(),
            prompt_tokens: 0,
            output_tokens: 0,
            first_token: None,
        }
    }

    /// Whether the reply has ended on an error.
    pub(crate) const fn is_done(&self) -> bool {
        self.done
    }

    /// Takes `bytes` of the Chat reply, arriving at `now`, and returns the
    /// Ollama lines they complete.
    pub(crate) fn feed(&mut self, bytes: &[u8], now: Instant) -> Vec<u8> {
        let mut out = Vec::new();
        if self.done {
            return out;
        }
        self.buffer.extend_from_slice(bytes);
        if !self.sse {
            return out;
        }
        while let Some(end) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let raw: Vec<u8> = self.buffer.drain(..=end).collect();
            let text = String::from_utf8_lossy(&raw[..end]);
            self.sse_line(text.trim_end_matches('\r'), now, &mut out);
            if self.done {
                self.buffer.clear();
                break;
            }
        }
        out
    }

    /// Ends the reply with an error line: the stream broke.
    pub(crate) fn fail(&mut self, message: &str) -> Vec<u8> {
        if self.done {
            return Vec::new();
        }
        self.done = true;
        error_body(message)
    }

    /// Ends the reply at `now`: the tool calls and the last line when
    /// streaming, else the whole reply.
    pub(crate) fn finish(&mut self, now: Instant) -> Vec<u8> {
        let mut out = Vec::new();
        if self.done {
            return out;
        }
        let rest = std::mem::take(&mut self.buffer);
        if self.sse {
            let text = String::from_utf8_lossy(&rest);
            self.sse_line(text.trim_end_matches('\r'), now, &mut out);
        } else if let Ok(whole) = json::from_slice::<Value>(&rest) {
            self.chunk(&whole, now, &mut out);
        }
        if self.done {
            return out;
        }
        let calls = self.tool_calls();
        if self.stream {
            if !calls.is_empty() {
                out.extend(line(&self.part("", "", calls, None)));
            }
            out.extend(line(&self.part("", "", Vec::new(), Some(now))));
        } else {
            let content = std::mem::take(&mut self.content);
            let thinking = std::mem::take(&mut self.thinking);
            out.extend(line(&self.part(&content, &thinking, calls, Some(now))));
        }
        out
    }

    /// One line of the Chat stream.
    fn sse_line(&mut self, text: &str, now: Instant, out: &mut Vec<u8>) {
        let Some(data) = text.strip_prefix("data:") else {
            return;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            return;
        }
        let Ok(chunk) = json::from_str::<Value>(data) else {
            return;
        };
        if let Some(error) = chunk.get("error") {
            let message = match error.get("message").and_then(Value::as_str) {
                Some(message) if !message.is_empty() => message.to_owned(),
                _ => match error {
                    Value::String(message) => message.clone(),
                    other => other.to_string(),
                },
            };
            out.extend(error_body(&message));
            self.done = true;
            return;
        }
        self.chunk(&chunk, now, out);
    }

    /// One stream chunk, or a whole completion with its message where a
    /// chunk has its delta.
    fn chunk(&mut self, chunk: &Value, now: Instant, out: &mut Vec<u8>) {
        if let Some(usage) = chunk.get("usage").filter(|usage| usage.is_object()) {
            self.prompt_tokens = usage["prompt_tokens"].as_u64().unwrap_or(0);
            self.output_tokens = usage["completion_tokens"].as_u64().unwrap_or(0);
        }
        let Some(choice) = chunk.get("choices").and_then(|choices| choices.get(0)) else {
            return;
        };
        let delta = choice
            .get("delta")
            .or_else(|| choice.get("message"))
            .unwrap_or(&Value::Null);
        let text = delta.get("content").and_then(Value::as_str).unwrap_or("");
        let mut think = match delta.get("reasoning_content").and_then(Value::as_str) {
            Some(think) if !think.is_empty() => think,
            _ => delta.get("reasoning").and_then(Value::as_str).unwrap_or(""),
        };
        if self.thinking_off {
            think = "";
        }
        for call in delta
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let at = call
                .get("index")
                .and_then(Value::as_u64)
                .and_then(|index| usize::try_from(index).ok())
                .unwrap_or(self.calls.len());
            if at >= MAX_CALLS {
                continue;
            }
            if self.calls.len() <= at {
                self.calls.resize_with(at + 1, ToolCall::default);
            }
            let slot = &mut self.calls[at];
            if let Some(id) = call
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
            {
                id.clone_into(&mut slot.id);
            }
            let function = call.get("function").unwrap_or(&Value::Null);
            if let Some(name) = function
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
            {
                name.clone_into(&mut slot.name);
            }
            if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                slot.arguments.push_str(arguments);
            }
        }
        if let Some(reason) = choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .filter(|reason| !reason.is_empty())
        {
            reason.clone_into(&mut self.finish_reason);
        }
        if text.is_empty() && think.is_empty() {
            return;
        }
        self.first_token.get_or_insert(now);
        self.content.push_str(text);
        self.thinking.push_str(think);
        if self.stream {
            out.extend(line(&self.part(text, think, Vec::new(), None)));
        }
    }

    /// The calls the reply made, their arguments as objects.
    fn tool_calls(&self) -> Vec<Value> {
        let mut out = Vec::new();
        for (index, call) in self.calls.iter().enumerate() {
            if call.name.is_empty() {
                continue;
            }
            let id = if call.id.is_empty() {
                format!("call_{index}")
            } else {
                call.id.clone()
            };
            let raw = call.arguments.trim();
            let arguments = if raw.is_empty() {
                json!({})
            } else {
                // What a model wrote that is not JSON is kept, as a string.
                json::from_str(raw).unwrap_or_else(|_| json!(raw))
            };
            out.push(json!({
                "id": id,
                "function": {"index": index, "name": call.name, "arguments": arguments},
            }));
        }
        out
    }

    /// One line of Ollama's reply; `end` makes it the last.
    fn part(&self, text: &str, think: &str, calls: Vec<Value>, end: Option<Instant>) -> Value {
        let mut out = Map::new();
        out.insert("model".to_owned(), json!(self.model));
        out.insert("created_at".to_owned(), json!(now()));
        out.insert("done".to_owned(), json!(end.is_some()));
        if self.generate {
            out.insert("response".to_owned(), json!(text));
            if !think.is_empty() {
                out.insert("thinking".to_owned(), json!(think));
            }
        } else {
            let mut message = Map::new();
            message.insert("role".to_owned(), json!("assistant"));
            message.insert("content".to_owned(), json!(text));
            if !think.is_empty() {
                message.insert("thinking".to_owned(), json!(think));
            }
            if !calls.is_empty() {
                message.insert("tool_calls".to_owned(), Value::Array(calls));
            }
            out.insert("message".to_owned(), Value::Object(message));
        }
        if let Some(end) = end {
            let reason = if self.finish_reason == "length" {
                "length"
            } else {
                "stop"
            };
            let first = self.first_token.unwrap_or(end);
            let nanos = |from: Instant, to: Instant| {
                u64::try_from(to.saturating_duration_since(from).as_nanos()).unwrap_or(u64::MAX)
            };
            out.insert("done_reason".to_owned(), json!(reason));
            out.insert("total_duration".to_owned(), json!(nanos(self.start, end)));
            out.insert("load_duration".to_owned(), json!(0));
            out.insert("prompt_eval_count".to_owned(), json!(self.prompt_tokens));
            out.insert(
                "prompt_eval_duration".to_owned(),
                json!(nanos(self.start, first)),
            );
            out.insert("eval_count".to_owned(), json!(self.output_tokens));
            out.insert("eval_duration".to_owned(), json!(nanos(first, end)));
        }
        Value::Object(out)
    }
}
