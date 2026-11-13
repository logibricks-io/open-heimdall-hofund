//! Incremental COSY SSE to Chat Completions SSE, including tool lifting.

use std::collections::{HashMap, VecDeque};

use hofund_plugin_api::{BodyStream, PluginError};
use hofund_plugin_sdk::bytes::Bytes;
use hofund_plugin_sdk::futures::unfold;
use hofund_plugin_sdk::json::{Value, json};
use hofund_plugin_sdk::transport::HttpResponse;
use hofund_plugin_sdk::uuid;

const CALL_OPEN: &str = "<tool_call>";
const CALL_CLOSE: &str = "</tool_call>";

pub(crate) fn transform(response: HttpResponse, tools: Vec<Value>) -> HttpResponse {
    let (parts, body) = response.into_parts();
    let state = State {
        body,
        parser: Parser::new(tools),
        finished: false,
    };
    let body = BodyStream::from_stream(unfold(state, |mut state| async move {
        state.next().await.map(|item| (item, state))
    }));
    HttpResponse::from_parts(parts, body)
}

struct State {
    body: BodyStream,
    parser: Parser,
    finished: bool,
}

impl State {
    async fn next(&mut self) -> Option<Result<Bytes, PluginError>> {
        loop {
            if let Some(output) = self.parser.output.pop_front() {
                return Some(output.map(|frames| Bytes::from(encode_frames(&frames))));
            }
            if self.finished {
                return None;
            }
            match self.body.next_chunk().await {
                Some(Ok(chunk)) => {
                    if let Err(error) = self.parser.feed(&chunk) {
                        self.finished = true;
                        return Some(Err(error));
                    }
                }
                Some(Err(error)) => {
                    self.finished = true;
                    return Some(Err(error));
                }
                None => {
                    self.finished = true;
                    let output = self.parser.finish();
                    return Some(output.map(|frames| Bytes::from(encode_frames(&frames))));
                }
            }
        }
    }
}

enum Frame {
    Json(Value),
    Done,
}

fn encode_frames(frames: &[Frame]) -> Vec<u8> {
    let mut out = Vec::new();
    for frame in frames {
        match frame {
            Frame::Json(value) => {
                out.extend_from_slice(b"data: ");
                out.extend_from_slice(value.to_string().as_bytes());
                out.extend_from_slice(b"\n\n");
            }
            Frame::Done => out.extend_from_slice(b"data: [DONE]\n\n"),
        }
    }
    out
}

struct Parser {
    pending: Vec<u8>,
    output: VecDeque<Result<Vec<Frame>, PluginError>>,
    splitter: Splitter,
    native: HashMap<u64, Native>,
    next_index: u64,
    finish_reason: String,
    said: bool,
    done: bool,
}

struct Native {
    id: String,
    args: String,
    output_index: Option<u64>,
}

impl Parser {
    fn new(tools: Vec<Value>) -> Self {
        Self {
            pending: Vec::new(),
            output: VecDeque::new(),
            splitter: Splitter::new(tools),
            native: HashMap::new(),
            next_index: 0,
            finish_reason: String::new(),
            said: false,
            done: false,
        }
    }

    fn feed(&mut self, chunk: &[u8]) -> Result<(), PluginError> {
        self.pending.extend_from_slice(chunk);
        while let Some(newline) = self.pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=newline).collect();
            let frames = self.line(&line)?;
            if !frames.is_empty() {
                self.output.push_back(Ok(frames));
            }
        }
        Ok(())
    }

    fn line(&mut self, line: &[u8]) -> Result<Vec<Frame>, PluginError> {
        let Ok(line) = std::str::from_utf8(line) else {
            return Ok(Vec::new());
        };
        let Some(payload) = line.trim().strip_prefix("data:") else {
            return Ok(Vec::new());
        };
        let Ok(envelope) = hofund_plugin_sdk::json::from_str::<Value>(payload.trim()) else {
            return Ok(Vec::new());
        };
        let inner = envelope.get("body").and_then(Value::as_str).unwrap_or("");
        if let Some(status) = envelope.get("statusCodeValue").and_then(Value::as_u64)
            && status != 200
        {
            return Err(failure(status as u16, inner));
        }
        if inner.trim() == "[DONE]" {
            self.done = true;
            return Ok(Vec::new());
        }
        let Ok(chunk) =
            hofund_plugin_sdk::json::from_str::<Value>(inner.trim().trim_start_matches('\u{feff}'))
        else {
            return Ok(Vec::new());
        };
        if !chunk.get("choices").and_then(Value::as_array).is_some()
            && (chunk.get("error").is_some()
                || (chunk.get("code").is_some() && chunk.get("message").is_some()))
        {
            return Err(failure(502, inner));
        }
        self.chunk(chunk)
    }

    fn chunk(&mut self, chunk: Value) -> Result<Vec<Frame>, PluginError> {
        let mut frames = Vec::new();
        let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            if chunk.get("usage").is_some() {
                frames.push(Frame::Json(chunk));
            }
            return Ok(frames);
        };
        let delta = choice.get("delta").cloned().unwrap_or_else(|| json!({}));
        if let Some(reasoning) = delta
            .get("reasoning_content")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            self.said = true;
            frames.push(Frame::Json(openai_chunk(
                &chunk,
                json!({"reasoning_content": reasoning}),
                None,
            )));
        }
        if let Some(content) = delta.get("content").and_then(Value::as_str) {
            for piece in self.splitter.feed(content) {
                self.piece(piece, &chunk, &mut frames)?;
            }
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                self.native_call(call, &chunk, &mut frames)?;
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = reason.to_owned();
        }
        Ok(frames)
    }

    fn piece(
        &mut self,
        piece: Piece,
        chunk: &Value,
        frames: &mut Vec<Frame>,
    ) -> Result<(), PluginError> {
        match piece {
            Piece::Text(text) => {
                if !text.is_empty() {
                    self.said = true;
                    frames.push(Frame::Json(openai_chunk(
                        chunk,
                        json!({"content": text}),
                        None,
                    )));
                }
            }
            Piece::Call(call) => {
                self.said = true;
                self.splitter.saw_tool = true;
                let index = self.take_index();
                let id = format!("call_{}", uuid::v4().unwrap_or_default().replace('-', ""));
                frames.push(Frame::Json(openai_chunk(
                    chunk,
                    json!({"tool_calls": [{"index": index, "id": id, "type": "function", "function": {"name": call.name, "arguments": call.args}}]}),
                    None,
                )));
            }
            Piece::Error(message) => return Err(PluginError::status_error(502, message)),
        }
        Ok(())
    }

    fn native_call(
        &mut self,
        call: &Value,
        chunk: &Value,
        frames: &mut Vec<Frame>,
    ) -> Result<(), PluginError> {
        let upstream = call.get("index").and_then(Value::as_u64).unwrap_or(0);
        let name = call
            .pointer("/function/name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let args = call
            .pointer("/function/arguments")
            .and_then(Value::as_str)
            .unwrap_or("");
        let id = call.get("id").and_then(Value::as_str).unwrap_or("");
        if id.is_empty() && name.is_empty() && args.is_empty() {
            return Ok(());
        }
        let native = self.native.entry(upstream).or_insert_with(|| Native {
            id: String::new(),
            args: String::new(),
            output_index: None,
        });
        if !id.is_empty() {
            native.id = id.to_owned();
        }
        if native.output_index.is_none() {
            native.args.push_str(args);
            if name.is_empty() {
                return Ok(());
            }
            for piece in self.splitter.flush() {
                self.piece(piece, chunk, frames)?;
            }
            let output = self.take_index();
            let native = self.native.get_mut(&upstream).expect("native entry");
            native.output_index = Some(output);
            self.said = true;
            self.splitter.saw_tool = true;
            let id = if native.id.is_empty() {
                format!("call_{}", uuid::v4().unwrap_or_default().replace('-', ""))
            } else {
                native.id.clone()
            };
            frames.push(Frame::Json(openai_chunk(
                chunk,
                json!({"tool_calls": [{"index": output, "id": id, "type": "function", "function": {"name": name, "arguments": native.args}}]}),
                None,
            )));
        } else if let Some(output) = native.output_index
            && !args.is_empty()
        {
            frames.push(Frame::Json(openai_chunk(
                chunk,
                json!({"tool_calls": [{"index": output, "function": {"arguments": args}}]}),
                None,
            )));
        }
        Ok(())
    }

    fn take_index(&mut self) -> u64 {
        let index = self.next_index;
        self.next_index += 1;
        index
    }

    fn finish(&mut self) -> Result<Vec<Frame>, PluginError> {
        let pending = std::mem::take(&mut self.pending);
        let mut frames = if pending.is_empty() {
            Vec::new()
        } else {
            self.line(&pending)?
        };
        let terminal = json!({"choices": [{"index": 0, "delta": {}, "finish_reason": null}], "object": "chat.completion.chunk"});
        for piece in self.splitter.flush() {
            self.piece(piece, &terminal, &mut frames)?;
        }
        let nameless = self.native.values().any(|call| call.output_index.is_none());
        if nameless && !self.said {
            return Err(PluginError::status_error(
                502,
                "the model's native tool call has no name; it is not sent on as a tool",
            ));
        }
        let reason = if self.splitter.saw_tool {
            "tool_calls"
        } else if self.finish_reason == "length" {
            "length"
        } else {
            "stop"
        };
        frames.push(Frame::Json(openai_chunk(
            &terminal,
            json!({}),
            Some(reason),
        )));
        if self.done || !frames.is_empty() {
            frames.push(Frame::Done);
        }
        Ok(frames)
    }
}

fn openai_chunk(source: &Value, delta: Value, finish: Option<&str>) -> Value {
    let mut chunk = json!({
        "id": source.get("id").and_then(Value::as_str).unwrap_or("chatcmpl-qoder"),
        "object": "chat.completion.chunk",
        "created": source.get("created").and_then(Value::as_u64).unwrap_or(0),
        "model": source.get("model").and_then(Value::as_str).unwrap_or("qoder"),
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    });
    if let Some(usage) = source.get("usage") {
        chunk["usage"] = usage.clone();
    }
    chunk
}

pub(crate) fn failure(status: u16, body: &str) -> PluginError {
    let value = hofund_plugin_sdk::json::from_str::<Value>(body).ok();
    let code = value
        .as_ref()
        .and_then(|value| value.get("code"))
        .map(|code| {
            code.as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| code.to_string())
        })
        .unwrap_or_default();
    let message = value
        .as_ref()
        .and_then(|value| value.get("message"))
        .and_then(Value::as_str)
        .filter(|message| !message.is_empty())
        .unwrap_or("Qoder request failed");
    if matches!(
        code.as_str(),
        "110" | "112" | "113" | "114" | "115" | "116" | "117" | "118" | "119" | "122"
    ) || status == 429
    {
        return PluginError::quota_status(429, format!("usage limit reached: {message}"));
    }
    if code == "10605" {
        return PluginError::status_error(
            503,
            format!("Qoder queued the request; try again later: {message}"),
        )
        .with_retry_after(std::time::Duration::from_secs(30));
    }
    PluginError::status_error(status.clamp(400, 599), message)
}

#[derive(Clone, Debug)]
struct LiftedCall {
    name: String,
    args: String,
}

#[derive(Debug)]
enum Piece {
    Text(String),
    Call(LiftedCall),
    Error(String),
}

struct Splitter {
    tools: Vec<Value>,
    text: String,
    call: String,
    in_call: bool,
    sent: String,
    saw_tool: bool,
}

impl Splitter {
    fn new(tools: Vec<Value>) -> Self {
        Self {
            tools,
            text: String::new(),
            call: String::new(),
            in_call: false,
            sent: String::new(),
            saw_tool: false,
        }
    }

    fn feed(&mut self, input: &str) -> Vec<Piece> {
        let mut out = Vec::new();
        let mut input = input.to_owned();
        while !input.is_empty() {
            if self.in_call {
                let combined = format!("{}{}", self.call, input);
                self.call.clear();
                if let Some(end) = combined.find(CALL_CLOSE) {
                    let body = &combined[..end];
                    match parse_call(body, &self.tools) {
                        Some(call) => {
                            self.saw_tool = true;
                            out.push(Piece::Call(call));
                        }
                        None if callish(body) && !quoted(&self.sent) => {
                            self.in_call = false;
                            out.push(Piece::Error("the model's tool call, written as text, can't be read; it is not sent on as the answer".to_owned()));
                            return out;
                        }
                        None => self.text_out(&mut out, &format!("{CALL_OPEN}{body}{CALL_CLOSE}")),
                    }
                    input = combined[end + CALL_CLOSE.len()..].to_owned();
                    self.in_call = false;
                } else {
                    self.call = combined;
                    break;
                }
                continue;
            }
            let combined = normalize_bare_xml(&format!("{}{}", self.text, input));
            self.text.clear();
            let wrapped = combined.find(CALL_OPEN);
            let bare = combined.find("<function=");
            let marker = match (wrapped, bare) {
                (Some(left), Some(right)) => Some(left.min(right)),
                (Some(index), None) | (None, Some(index)) => Some(index),
                (None, None) => None,
            };
            let Some(index) = marker else {
                let keep = partial_marker(&combined);
                let end = combined.len() - keep;
                self.text_out(&mut out, &combined[..end]);
                self.text = combined[end..].to_owned();
                break;
            };
            if index > 0 {
                self.text_out(&mut out, &combined[..index]);
            }
            if wrapped == Some(index) {
                input = combined[index + CALL_OPEN.len()..].to_owned();
                self.in_call = true;
                continue;
            }
            if quoted(&self.sent) {
                self.text_out(&mut out, "<function=");
                input = combined[index + "<function=".len()..].to_owned();
                continue;
            }
            match bare_call(&combined[index..], &self.tools, false) {
                Bare::Call(call, rest) => {
                    self.saw_tool = true;
                    out.push(Piece::Call(call));
                    input = rest;
                }
                Bare::Wait => {
                    self.text = combined[index..].to_owned();
                    break;
                }
                Bare::Text => {
                    self.text_out(&mut out, "<function=");
                    input = combined[index + "<function=".len()..].to_owned();
                }
                Bare::Error => {
                    out.push(Piece::Error("the model's tool call, written as text, was cut short; it is not sent on as the answer".to_owned()));
                    return out;
                }
            }
        }
        out
    }

    fn flush(&mut self) -> Vec<Piece> {
        let mut out = Vec::new();
        if self.in_call {
            self.in_call = false;
            let body = std::mem::take(&mut self.call);
            if callish(&body) && !quoted(&self.sent) {
                if let Some(call) = parse_call(&body, &self.tools) {
                    self.saw_tool = true;
                    out.push(Piece::Call(call));
                } else {
                    out.push(Piece::Error("the model's tool call, written as text, was cut short; it is not sent on as the answer".to_owned()));
                }
            } else {
                self.text_out(&mut out, &format!("{CALL_OPEN}{body}"));
            }
        }
        if !self.text.is_empty() {
            let text = std::mem::take(&mut self.text);
            if let Some(index) = text.find("<function=") {
                if index > 0 {
                    self.text_out(&mut out, &text[..index]);
                }
                match bare_call(&text[index..], &self.tools, true) {
                    Bare::Call(call, rest) => {
                        self.saw_tool = true;
                        out.push(Piece::Call(call));
                        self.text_out(&mut out, &rest);
                    }
                    Bare::Error => out.push(Piece::Error("the model's tool call, written as text, was cut short; it is not sent on as the answer".to_owned())),
                    Bare::Text | Bare::Wait => self.text_out(&mut out, &text[index..]),
                }
            } else {
                self.text_out(&mut out, &text);
            }
        }
        out
    }

    fn text_out(&mut self, out: &mut Vec<Piece>, text: &str) {
        if !text.is_empty() {
            self.sent.push_str(text);
            out.push(Piece::Text(text.to_owned()));
        }
    }
}

fn normalize_bare_xml(text: &str) -> String {
    let mut out = text
        .replace("<antml:", "<")
        .replace("</antml:", "</")
        .replace("<function_calls>", "")
        .replace("</function_calls>", "")
        .replace("<invoke name=\"", "<function=")
        .replace("</invoke>", "</function>")
        .replace("<parameter name=\"", "<parameter=");
    for marker in ["<function=", "<parameter="] {
        let mut from = 0;
        while let Some(found) = out[from..].find(marker) {
            let start = from + found;
            let Some(end) = out[start..].find('>').map(|end| start + end) else {
                break;
            };
            if out.as_bytes().get(end.wrapping_sub(1)) == Some(&b'"') {
                out.remove(end - 1);
            }
            from = start + marker.len();
        }
    }
    out
}

fn partial_marker(text: &str) -> usize {
    let mut keep = 0;
    for marker in [
        CALL_OPEN,
        "<function=",
        "<function_calls>",
        "<antml:function_calls>",
        "<invoke ",
        "<antml:invoke ",
        "</function_calls>",
        "</antml:function_calls>",
        "</invoke>",
        "</antml:invoke>",
    ] {
        for length in 1..=marker.len().min(text.len()) {
            if text.ends_with(&marker[..length]) {
                keep = keep.max(length);
            }
        }
    }
    if let Some(start) = text.rfind('<')
        && !text[start..].contains('>')
    {
        keep = keep.max(text.len() - start);
    }
    keep
}

fn callish(body: &str) -> bool {
    body.trim_start().starts_with('{') || body.contains("<function=")
}

fn quoted(text: &str) -> bool {
    if text.matches("```").count() % 2 == 1 {
        return true;
    }
    text.rsplit('\n')
        .next()
        .unwrap_or(text)
        .replace("```", "")
        .matches('`')
        .count()
        % 2
        == 1
}

fn parse_call(body: &str, tools: &[Value]) -> Option<LiftedCall> {
    if let Ok(value) = hofund_plugin_sdk::json::from_str::<Value>(body.trim())
        && let Some(name) = value
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
        && let Some(args) = value.get("arguments").and_then(Value::as_object)
    {
        return Some(LiftedCall {
            name: name.trim().to_owned(),
            args: Value::Object(args.clone()).to_string(),
        });
    }
    match bare_call(body, tools, true) {
        Bare::Call(call, _) => Some(call),
        _ => None,
    }
}

enum Bare {
    Call(LiftedCall, String),
    Wait,
    Text,
    Error,
}

fn bare_call(text: &str, tools: &[Value], end: bool) -> Bare {
    let Some(after) = text.strip_prefix("<function=") else {
        return Bare::Text;
    };
    let Some(open) = after.find('>') else {
        return if end { Bare::Text } else { Bare::Wait };
    };
    let name = after[..open].trim();
    if !tools
        .iter()
        .any(|tool| tool.pointer("/function/name").and_then(Value::as_str) == Some(name))
    {
        return Bare::Text;
    }
    let body = &after[open + 1..];
    let close = body.find("</function>");
    let next = body.find("<function=");
    let stop = match (close, next) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(index), None) | (None, Some(index)) => Some(index),
        (None, None) => None,
    };
    let Some(stop) = stop else {
        if !end {
            return Bare::Wait;
        }
        if !body.contains("<parameter=") {
            return Bare::Text;
        }
        return parse_bare_args(name, body, tools, "");
    };
    let whole = close == Some(stop);
    parse_bare_args(
        name,
        &body[..stop],
        tools,
        if whole {
            &body[stop + "</function>".len()..]
        } else {
            &body[stop..]
        },
    )
}

fn parse_bare_args(name: &str, body: &str, tools: &[Value], remainder: &str) -> Bare {
    let mut args = hofund_plugin_sdk::json::Map::new();
    let mut rest = body;
    while let Some(start) = rest.find("<parameter=") {
        rest = &rest[start + "<parameter=".len()..];
        let Some(tag) = rest.find('>') else {
            return Bare::Error;
        };
        let key = rest[..tag].trim();
        rest = &rest[tag + 1..];
        let Some(end) = rest.find("</parameter>") else {
            return Bare::Error;
        };
        let value = rest[..end].trim_matches(['\r', '\n']).to_owned();
        let property = tools
            .iter()
            .find(|tool| tool.pointer("/function/name").and_then(Value::as_str) == Some(name))
            .and_then(|tool| tool.pointer(&format!("/function/parameters/properties/{key}")));
        let value = if property
            .and_then(|property| property.get("type"))
            .and_then(Value::as_str)
            == Some("string")
        {
            Value::String(value)
        } else {
            hofund_plugin_sdk::json::from_str(value.trim()).unwrap_or(Value::String(value))
        };
        args.insert(key.to_owned(), value);
        rest = &rest[end + "</parameter>".len()..];
    }
    Bare::Call(
        LiftedCall {
            name: name.to_owned(),
            args: Value::Object(args).to_string(),
        },
        remainder.to_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::Splitter;
    use hofund_plugin_sdk::json::json;

    #[test]
    fn r54b_tool_json_is_lifted_across_chunks() {
        let tools = vec![
            json!({"type": "function", "function": {"name": "read", "parameters": {"type": "object"}}}),
        ];
        let mut splitter = Splitter::new(tools);
        let first =
            splitter.feed("Reading.\n<tool_call>{\"name\":\"read\",\"arguments\":{\"path\":");
        let second = splitter.feed("\"a.go\"}}</tool_call>");
        assert_eq!(first.len(), 1);
        assert!(matches!(second.first(), Some(super::Piece::Call(call)) if call.name == "read"));
    }

    #[test]
    fn r54b_unreadable_tool_json_fails_closed() {
        let mut splitter = Splitter::new(Vec::new());
        let pieces = splitter.feed("<tool_call>{\"name\":\"read\",\"arguments\":</tool_call>");
        assert!(matches!(pieces.first(), Some(super::Piece::Error(_))));
    }

    #[test]
    fn r54b_anthropic_and_namespaced_xml_calls_are_lifted() {
        let tools = vec![json!({
            "type": "function",
            "function": {"name": "read", "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}}
        })];
        let mut splitter = Splitter::new(tools);
        let text = "<function_calls><invoke name=\"read\"><parameter name=\"path\">a.go</parameter></invoke><invoke name=\"read\"><parameter name=\"path\">b.go</parameter></invoke></function_calls>";
        let mut pieces = Vec::new();
        for chunk in text.as_bytes().chunks(7) {
            pieces.extend(splitter.feed(std::str::from_utf8(chunk).expect("chunk")));
        }
        pieces.extend(splitter.flush());
        let calls = pieces
            .iter()
            .filter(|piece| matches!(piece, super::Piece::Call(_)))
            .count();
        assert_eq!(calls, 2, "{pieces:?}");
        let namespaced = text
            .replace("<function_calls>", "<antml:function_calls>")
            .replace("</function_calls>", "</antml:function_calls>")
            .replace("<invoke ", "<antml:invoke ")
            .replace("</invoke>", "</antml:invoke>")
            .replace("<parameter ", "<antml:parameter ")
            .replace("</parameter>", "</antml:parameter>");
        let mut splitter = Splitter::new(vec![
            json!({"type": "function", "function": {"name": "read", "parameters": {"properties": {"path": {"type": "string"}}}}}),
        ]);
        let pieces = splitter.feed(&namespaced);
        assert_eq!(
            pieces
                .iter()
                .filter(|piece| matches!(piece, super::Piece::Call(_)))
                .count(),
            2
        );
    }
}
