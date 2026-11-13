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
    after: bool,
}

enum BareStep {
    Next(String),
    Wait,
    Error,
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
            after: false,
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
            let mut combined = format!("{}{}", self.text, input);
            self.text.clear();
            if self.after {
                let Some(rest) = self.left(&combined, false) else {
                    self.text = combined;
                    break;
                };
                combined = rest;
                if combined.is_empty() {
                    break;
                }
            }
            let wrapped = combined.find(CALL_OPEN);
            let bare = if self.tools.is_empty() {
                None
            } else {
                xml_at(&combined)
            };
            if let Some(index) = bare.filter(|index| wrapped.is_none_or(|at| *index < at)) {
                match self.bare(&mut out, &combined, index, false) {
                    BareStep::Next(rest) => input = rest,
                    BareStep::Wait => break,
                    BareStep::Error => return out,
                }
                continue;
            }
            let Some(index) = wrapped else {
                let keep = partial_marker(&combined, !self.tools.is_empty());
                let end = combined.len() - keep;
                self.text_out(&mut out, &combined[..end]);
                self.text = combined[end..].to_owned();
                break;
            };
            if index > 0 {
                self.text_out(&mut out, &combined[..index]);
            }
            input = combined[index + CALL_OPEN.len()..].to_owned();
            self.in_call = true;
        }
        out
    }

    fn bare(&mut self, out: &mut Vec<Piece>, combined: &str, index: usize, end: bool) -> BareStep {
        let head = &combined[..index];
        let at = &combined[index..];
        let opener_end = at.find('>').map_or(at.len(), |end| end + 1);
        let opener = &at[..opener_end];
        if quoted(&format!("{}{}", self.sent, head)) {
            self.text_out(out, &format!("{head}{opener}"));
            return BareStep::Next(at[opener_end..].to_owned());
        }
        match xml_call(at, end, &self.tools) {
            Bare::Wait => {
                self.text_out(out, head);
                self.text = at.to_owned();
                BareStep::Wait
            }
            Bare::Text => {
                self.text_out(out, &format!("{head}{opener}"));
                BareStep::Next(at[opener_end..].to_owned())
            }
            Bare::Error => {
                self.text_out(out, head);
                out.push(Piece::Error("the model's tool call, written as text, was cut short; it is not sent on as the answer".to_owned()));
                BareStep::Error
            }
            Bare::Call(call, rest) => {
                self.text_out(out, head);
                self.saw_tool = true;
                self.after = true;
                out.push(Piece::Call(call));
                BareStep::Next(rest)
            }
        }
    }

    fn left(&mut self, combined: &str, end: bool) -> Option<String> {
        let rest = xml_left(combined);
        if !end
            && (rest.is_empty()
                || XML_CLOSES
                    .iter()
                    .any(|close| close.len() > rest.len() && close.starts_with(rest)))
        {
            return None;
        }
        self.after = false;
        Some(rest.to_owned())
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
        let mut text = std::mem::take(&mut self.text);
        if self.after {
            text = self.left(&text, true).unwrap_or_default();
        }
        while !text.is_empty() {
            let Some(index) = (!self.tools.is_empty()).then(|| xml_at(&text)).flatten() else {
                self.text_out(&mut out, &text);
                break;
            };
            match self.bare(&mut out, &text, index, true) {
                BareStep::Next(rest) => {
                    text = if self.after {
                        self.left(&rest, true).unwrap_or_default()
                    } else {
                        rest
                    };
                }
                BareStep::Wait | BareStep::Error => break,
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

const XML_CLOSES: &[&str] = &[
    "</function_calls>",
    "</antml:function_calls>",
    "</invoke>",
    "</antml:invoke>",
    "</function>",
];

fn xml_left(text: &str) -> &str {
    let mut rest = text;
    loop {
        rest = rest.trim_start();
        if let Some(after) = XML_CLOSES.iter().find_map(|close| rest.strip_prefix(close)) {
            rest = after;
        } else {
            return rest;
        }
    }
}

fn xml_at(text: &str) -> Option<usize> {
    text.match_indices('<')
        .map(|(index, _)| index)
        .find(|index| {
            let tail = &text[*index..];
            tail.starts_with("<function=")
                || tail.starts_with("<function_calls>")
                || tail.starts_with("<antml:function_calls>")
                || invoke_marker(tail)
        })
}

fn invoke_prefix(text: &str) -> bool {
    ["<invoke", "<antml:invoke"].iter().any(|prefix| {
        text.strip_prefix(prefix)
            .and_then(|rest| rest.chars().next())
            .is_some_and(char::is_whitespace)
    })
}

fn invoke_marker(text: &str) -> bool {
    if !invoke_prefix(text) {
        return false;
    }
    let rest = text
        .strip_prefix("<antml:invoke")
        .or_else(|| text.strip_prefix("<invoke"))
        .expect("invoke prefix")
        .trim_start();
    rest.strip_prefix("name")
        .is_some_and(|rest| rest.trim_start().strip_prefix('=').is_some())
}

fn partial_marker(text: &str, xml: bool) -> usize {
    let mut keep = 0;
    let markers: &[&str] = if xml {
        &[
            CALL_OPEN,
            "<function=",
            "<function_calls>",
            "<antml:function_calls>",
            "<invoke ",
            "<antml:invoke ",
        ]
    } else {
        &[CALL_OPEN]
    };
    for marker in markers {
        for length in 1..=marker.len().min(text.len()) {
            if text.ends_with(&marker[..length]) {
                keep = keep.max(length);
            }
        }
    }
    if xml && let Some(start) = text.rfind('<') {
        let tail = &text[start..];
        if tail.len() <= 302
            && !tail.contains('>')
            && (tail.starts_with("<function=") || invoke_prefix(tail))
        {
            keep = keep.max(tail.len());
        }
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
    match xml_call(body, true, tools) {
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

fn xml_call(text: &str, end: bool, tools: &[Value]) -> Bare {
    let mut text = text;
    if let Some(wrapper) = ["<function_calls>", "<antml:function_calls>"]
        .iter()
        .find_map(|wrapper| text.strip_prefix(wrapper).map(|_| *wrapper))
    {
        let rest = text[wrapper.len()..].trim_start();
        if !end && (rest.is_empty() || xml_start_partial(rest)) {
            return Bare::Wait;
        }
        text = rest;
    }
    let invoke = invoke_open(text);
    let function = function_open(text);
    let Some((name, opened, is_invoke)) = invoke
        .map(|(name, opened)| (name, opened, true))
        .or_else(|| function.map(|(name, opened)| (name, opened, false)))
    else {
        if !end && xml_open_partial(text) {
            return Bare::Wait;
        }
        return Bare::Text;
    };
    if !tools
        .iter()
        .any(|tool| tool.pointer("/function/name").and_then(Value::as_str) == Some(name.as_str()))
    {
        return Bare::Text;
    }
    let body = &text[opened..];
    let close = if is_invoke {
        find_close(body, &["</invoke>", "</antml:invoke>"])
    } else {
        find_close(body, &["</function>"])
    };
    let next = xml_next(body);
    let stop = match (close, next) {
        (Some((left, _)), Some(right)) => Some(left.min(right)),
        (Some((index, _)), None) | (None, Some(index)) => Some(index),
        (None, None) => None,
    };
    let stop = match stop {
        Some(stop) => stop,
        None if !end => return Bare::Wait,
        None => body.len(),
    };
    let whole = close.is_some_and(|(index, _)| index == stop);
    let (args, closed, has_parameters) = xml_args(&body[..stop], &name, tools);
    if !whole && !closed {
        return if has_parameters {
            Bare::Error
        } else {
            Bare::Text
        };
    }
    let rest = if whole {
        let (index, length) = close.expect("whole close");
        &body[index + length..]
    } else {
        &body[stop..]
    };
    Bare::Call(
        LiftedCall {
            name,
            args: Value::Object(args).to_string(),
        },
        rest.to_owned(),
    )
}

fn xml_start_partial(text: &str) -> bool {
    ["<invoke ", "<antml:invoke ", "<function="]
        .iter()
        .any(|start| start.starts_with(text))
}

fn xml_open_partial(text: &str) -> bool {
    xml_start_partial(text)
        || ((text.starts_with("<function=") || invoke_prefix(text))
            && text.len() <= 302
            && !text[1..].chars().any(|ch| matches!(ch, '<' | '>')))
}

fn invoke_open(text: &str) -> Option<(String, usize)> {
    if !invoke_prefix(text) {
        return None;
    }
    let rest = text
        .strip_prefix("<antml:invoke")
        .or_else(|| text.strip_prefix("<invoke"))?
        .trim_start()
        .strip_prefix("name")?
        .trim_start()
        .strip_prefix('=')?
        .trim_start()
        .strip_prefix('"')?;
    let end = rest.find('"')?;
    let name = &rest[..end];
    let after = rest[end + 1..].trim_start().strip_prefix('>')?;
    Some((name.trim().to_owned(), text.len() - after.len()))
}

fn function_open(text: &str) -> Option<(String, usize)> {
    let rest = text.strip_prefix("<function=")?;
    let end = rest.find('>')?;
    Some((rest[..end].trim().to_owned(), end + "<function=>".len()))
}

fn find_close(text: &str, closes: &[&str]) -> Option<(usize, usize)> {
    closes
        .iter()
        .filter_map(|close| text.find(close).map(|index| (index, close.len())))
        .min_by_key(|(index, _)| *index)
}

fn xml_next(text: &str) -> Option<usize> {
    text.match_indices('<')
        .map(|(index, _)| index)
        .find(|index| {
            let tail = &text[*index..];
            tail.starts_with("<function=") || invoke_marker(tail)
        })
}

fn xml_args(
    body: &str,
    name: &str,
    tools: &[Value],
) -> (hofund_plugin_sdk::json::Map<String, Value>, bool, bool) {
    let opens: Vec<_> = body
        .match_indices('<')
        .filter_map(|(index, _)| {
            parameter_open(&body[index..]).map(|(key, used)| (index, index + used, key))
        })
        .collect();
    let mut closed = !opens.is_empty();
    let mut args = hofund_plugin_sdk::json::Map::new();
    for (position, (_, value_start, key)) in opens.iter().enumerate() {
        let limit = opens
            .get(position + 1)
            .map_or(body.len(), |(next, _, _)| *next);
        let raw = &body[*value_start..limit];
        let close = find_close(raw, &["</parameter>", "</antml:parameter>"]);
        if close.is_none() {
            closed = false;
        }
        let value = trim_xml_newlines(close.map_or(raw, |(index, _)| &raw[..index]));
        let property = tools
            .iter()
            .find(|tool| tool.pointer("/function/name").and_then(Value::as_str) == Some(name))
            .and_then(|tool| tool.pointer(&format!("/function/parameters/properties/{key}")));
        let value = if property
            .and_then(|property| property.get("type"))
            .and_then(Value::as_str)
            == Some("string")
        {
            Value::String(value.to_owned())
        } else {
            hofund_plugin_sdk::json::from_str(value.trim())
                .unwrap_or_else(|_| Value::String(value.to_owned()))
        };
        args.insert(key.clone(), value);
    }
    let has_parameters = body.match_indices('<').any(|(index, _)| {
        let tail = &body[index..];
        ["<parameter", "<antml:parameter"]
            .iter()
            .any(|prefix| tail.starts_with(prefix))
    });
    (args, closed, has_parameters)
}

fn parameter_open(text: &str) -> Option<(String, usize)> {
    let rest = text
        .strip_prefix("<antml:parameter")
        .or_else(|| text.strip_prefix("<parameter"))?;
    if let Some(equal) = rest.strip_prefix('=') {
        let end = equal.find('>')?;
        return Some((
            equal[..end].trim().to_owned(),
            text.len() - equal.len() + end + 1,
        ));
    }
    if !rest.chars().next().is_some_and(char::is_whitespace) {
        return None;
    }
    let rest = rest
        .trim_start()
        .strip_prefix("name")?
        .trim_start()
        .strip_prefix('=')?
        .trim_start()
        .strip_prefix('"')?;
    let end = rest.find('"')?;
    let key = &rest[..end];
    let after = rest[end + 1..].trim_start().strip_prefix('>')?;
    Some((key.to_owned(), text.len() - after.len()))
}

fn trim_xml_newlines(mut text: &str) -> &str {
    if let Some(rest) = text.strip_prefix("\r\n") {
        text = rest;
    } else if let Some(rest) = text.strip_prefix('\n') {
        text = rest;
    }
    if let Some(rest) = text.strip_suffix("\r\n") {
        text = rest;
    } else if let Some(rest) = text.strip_suffix('\n') {
        text = rest;
    }
    text
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
    fn r54b_xml_that_is_not_a_declared_call_passes_through_byte_for_byte() {
        fn assert_passthrough(text: &str, tools: Vec<hofund_plugin_sdk::json::Value>) {
            for size in [1, 3, 8, text.len().max(1)] {
                let mut splitter = super::Splitter::new(tools.clone());
                let mut pieces = Vec::new();
                for chunk in text.as_bytes().chunks(size) {
                    pieces.extend(splitter.feed(std::str::from_utf8(chunk).expect("chunk")));
                }
                pieces.extend(splitter.flush());
                let mut actual = String::new();
                for piece in pieces {
                    match piece {
                        super::Piece::Text(part) => actual.push_str(&part),
                        other => panic!("unexpected piece for {text:?}: {other:?}"),
                    }
                }
                assert_eq!(actual, text, "chunk size {size}");
            }
        }

        let tools = vec![json!({
            "type": "function",
            "function": {"name": "read", "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}}
        })];
        let unknown = "<function_calls><invoke name=\"delete_everything\"><parameter name=\"path\">/</parameter></invoke></function_calls>";
        assert_passthrough(unknown, tools.clone());
        assert_passthrough(
            "<function=nope><parameter=x>1</parameter></function>",
            tools.clone(),
        );
        assert_passthrough(
            "<function_calls><invoke name=\"read\"><parameter name=\"path\">a.go</parameter></invoke></function_calls>",
            Vec::new(),
        );
        for text in [
            "Claude wraps its calls in <function_calls> tags; each one is an <invoke> block.",
            "Use `<function_calls><invoke name=\"read\"><parameter name=\"path\">a.go</parameter></invoke></function_calls>` to call it.",
            "Example:\n```xml\n<function_calls>\n<invoke name=\"read\">\n<parameter name=\"path\">a.go</parameter>\n</invoke>\n</function_calls>\n```\nThat is the format.",
            "Qwen writes `<function=read>` and then its parameters.",
            "The <invoke name=\"read\"> tag names a tool, and the call ends later.",
            "a < b and c > d, <function_calls without a close",
        ] {
            assert_passthrough(text, tools.clone());
        }
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
