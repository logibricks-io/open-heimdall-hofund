//! Turn Zed `/completions` NDJSON into provider SSE bytes for the wire decoder.

use std::collections::HashSet;

use hofund_plugin_api::{Body, BodyStream, PluginError};
use hofund_plugin_sdk::json::Value;

use crate::body::Wire;
use crate::errors;

const CUT_SHORT_NOTE: &str =
    "(The reply was cut short: Zed closed the stream before it finished. Ask again to go on.)";

enum ParsedLine {
    Event(Value),
    Ended,
    Failed { code: String, message: String },
}

/// Reads all of `raw` NDJSON, wraps provider events as SSE, and returns one body stream.
pub(crate) fn ndjson_to_sse_body(
    raw: &[u8],
    wire: Wire,
    wrapped: bool,
    streaming: bool,
) -> Result<BodyStream, String> {
    let lines = parse_lines(raw, wrapped)?;
    if !wrapped {
        let mut out = Vec::new();
        for line in &lines {
            let ParsedLine::Event(event) = line else {
                continue;
            };
            write_sse(&mut out, wire, event);
        }
        if out.is_empty() {
            return Err("502:Zed ended without an answer".to_owned());
        }
        if wire == Wire::Chat {
            out.extend_from_slice(b"data: [DONE]\n\n");
        }
        return Ok(BodyStream::from_bytes(out));
    }

    if !streaming {
        return whole_wrapped(lines, wire);
    }

    streamed_wrapped(lines, wire)
}

/// Incrementally turns upstream NDJSON chunks into provider SSE without buffering the full body.
pub(crate) async fn ndjson_to_sse_stream(
    mut inner: BodyStream,
    wire: Wire,
    wrapped: bool,
) -> Result<BodyStream, String> {
    if !wrapped {
        return Ok(unwrapped_incremental(inner, wire));
    }
    let mut buf = Vec::new();
    let mut head = Vec::new();
    let mut ended = false;
    let mut first_said: Option<Value> = None;
    loop {
        while let Some(line) = take_line(&mut buf) {
            match parse_wrapped_line(&line)? {
                ParsedLine::Ended => {
                    ended = true;
                    continue;
                }
                ParsedLine::Failed { code, message } => {
                    if first_said.is_none() {
                        let status = errors::failed_status(&code);
                        return Err(format!("{status}:{message}"));
                    }
                    let mut out = Vec::new();
                    let status = errors::failed_status(&code);
                    write_sse_error(&mut out, wire, status, &message);
                    return Ok(BodyStream::resume(
                        Body::from_bytes(out).bytes().clone(),
                        continue_wrapped(inner, buf, wire, ended, first_said, head, true),
                    ));
                }
                ParsedLine::Event(event) => {
                    if let Some(message) = event_error(wire, &event) {
                        if first_said.is_none() {
                            return Err(format!("502:{message}"));
                        }
                        head.push(json_error_event(wire, &message));
                        continue;
                    }
                    if first_said.is_none() {
                        if said(wire, &event) {
                            first_said = Some(event);
                            break;
                        }
                        head.push(event);
                    } else {
                        head.push(event);
                    }
                }
            }
        }
        if first_said.is_some() {
            break;
        }
        let Some(chunk) = inner.next_chunk().await else {
            if !ended {
                return Err("502:the reply ended before it was complete".to_owned());
            }
            return Err("502:Zed ended without an answer".to_owned());
        };
        let chunk = chunk.map_err(|error| error.message().to_owned())?;
        buf.extend_from_slice(&chunk);
    }
    let Some(first) = first_said else {
        return Err("502:Zed ended without an answer".to_owned());
    };
    let mut initial = Vec::new();
    for event in &head {
        write_sse(&mut initial, wire, event);
    }
    write_sse(&mut initial, wire, &first);
    Ok(BodyStream::resume(
        Body::from_bytes(initial).bytes().clone(),
        continue_wrapped(
            inner,
            buf,
            wire,
            ended,
            Some(first.clone()),
            Vec::new(),
            false,
        ),
    ))
}

struct ContinueWrapped {
    inner: BodyStream,
    buf: Vec<u8>,
    wire: Wire,
    ended: bool,
    chat: ChatTracker,
    tail_failed: bool,
}

fn continue_wrapped(
    inner: BodyStream,
    buf: Vec<u8>,
    wire: Wire,
    ended: bool,
    first: Option<Value>,
    pending: Vec<Value>,
    tail_failed: bool,
) -> BodyStream {
    let mut chat = ChatTracker::default();
    if wire == Wire::Chat {
        for event in &pending {
            chat.see(event);
        }
        if let Some(event) = &first {
            chat.see(event);
        }
    }
    BodyStream::unfold(
        ContinueWrapped {
            inner,
            buf,
            wire,
            ended,
            chat,
            tail_failed,
        },
        |mut state| async move {
            if state.tail_failed {
                return None;
            }
            loop {
                while let Some(line) = take_line(&mut state.buf) {
                    let parsed = match parse_wrapped_line(&line) {
                        Ok(line) => line,
                        Err(detail) if detail.is_empty() => continue,
                        Err(detail) => {
                            return Some((Err(PluginError::internal(detail)), state));
                        }
                    };
                    match parsed {
                        ParsedLine::Ended => {
                            state.ended = true;
                            continue;
                        }
                        ParsedLine::Failed { code, message } => {
                            let status = errors::failed_status(&code);
                            let mut out = Vec::new();
                            write_sse_error(&mut out, state.wire, status, &message);
                            return Some((Ok(out), state));
                        }
                        ParsedLine::Event(event) => {
                            if let Some(message) = event_error(state.wire, &event) {
                                let mut out = Vec::new();
                                write_sse(
                                    &mut out,
                                    state.wire,
                                    &json_error_event(state.wire, &message),
                                );
                                return Some((Ok(out), state));
                            }
                            if state.wire == Wire::Chat {
                                state.chat.see(&event);
                            }
                            let mut out = Vec::new();
                            write_sse(&mut out, state.wire, &event);
                            return Some((Ok(out), state));
                        }
                    }
                }
                let Some(chunk) = state.inner.next_chunk().await else {
                    return finish_incremental(state);
                };
                let chunk = match chunk {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        return Some((Err(error), state));
                    }
                };
                state.buf.extend_from_slice(&chunk);
            }
        },
    )
}

fn finish_incremental(
    state: ContinueWrapped,
) -> Option<(Result<Vec<u8>, PluginError>, ContinueWrapped)> {
    if !state.ended {
        if state.wire == Wire::Chat {
            if let Some(frames) = state.chat.end_frames() {
                return Some((Ok(frames), state));
            }
            return Some((
                Err(PluginError::internal(
                    "502:the reply ended before it was complete",
                )),
                state,
            ));
        }
        let mut out = Vec::new();
        write_sse_error(
            &mut out,
            state.wire,
            502,
            "the reply ended before it was complete",
        );
        return Some((Ok(out), state));
    }
    if state.wire == Wire::Chat {
        return Some((Ok(b"data: [DONE]\n\n".to_vec()), state));
    }
    None
}

struct UnwrappedRead {
    inner: BodyStream,
    buf: Vec<u8>,
    wire: Wire,
    sent_any: bool,
    done_sent: bool,
}

fn unwrapped_incremental(inner: BodyStream, wire: Wire) -> BodyStream {
    BodyStream::unfold(
        UnwrappedRead {
            inner,
            buf: Vec::new(),
            wire,
            sent_any: false,
            done_sent: false,
        },
        |mut state| async move {
            loop {
                while let Some(line) = take_line(&mut state.buf) {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let event: Value = match hofund_plugin_sdk::json::from_str(trimmed) {
                        Ok(value) => value,
                        Err(_) => {
                            return Some((
                                Err(PluginError::internal("Zed sent an unreadable line")),
                                state,
                            ));
                        }
                    };
                    let mut out = Vec::new();
                    write_sse(&mut out, state.wire, &event);
                    state.sent_any = true;
                    return Some((Ok(out), state));
                }
                let Some(chunk) = state.inner.next_chunk().await else {
                    if !state.sent_any {
                        return Some((
                            Err(PluginError::internal("502:Zed ended without an answer")),
                            state,
                        ));
                    }
                    if state.wire == Wire::Chat && !state.done_sent {
                        state.done_sent = true;
                        return Some((Ok(b"data: [DONE]\n\n".to_vec()), state));
                    }
                    return None;
                };
                let chunk = match chunk {
                    Ok(bytes) => bytes,
                    Err(error) => return Some((Err(error), state)),
                };
                state.buf.extend_from_slice(&chunk);
            }
        },
    )
}

fn take_line(buf: &mut Vec<u8>) -> Option<String> {
    let pos = buf.iter().position(|b| *b == b'\n')?;
    let line = buf.drain(..=pos).collect::<Vec<_>>();
    let text = std::str::from_utf8(&line[..line.len().saturating_sub(1)])
        .ok()?
        .to_owned();
    Some(text)
}

fn parse_wrapped_line(line: &str) -> Result<ParsedLine, String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Err(String::new());
    }
    let value: Value = hofund_plugin_sdk::json::from_str(trimmed)
        .map_err(|_| "Zed sent an unreadable line".to_owned())?;
    if value.get("event").is_some() {
        return Ok(ParsedLine::Event(
            value.get("event").cloned().unwrap_or(Value::Null),
        ));
    }
    if value.get("status").and_then(Value::as_str) == Some("stream_ended") {
        return Ok(ParsedLine::Ended);
    }
    if let Some(failed) = value.get("status").and_then(|s| s.get("failed")) {
        let code = failed
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("upstream_error")
            .to_owned();
        let message = failed
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or(&code)
            .to_owned();
        return Ok(ParsedLine::Failed { code, message });
    }
    Err("Zed sent an unreadable line".to_owned())
}

#[derive(Default)]
struct ChatTracker {
    wrote: HashSet<u32>,
    open: HashSet<u32>,
    tools: HashSet<u32>,
    done: HashSet<u32>,
    meta_id: Option<String>,
    meta_created: u64,
    meta_model: Option<String>,
}

impl ChatTracker {
    fn see(&mut self, event: &Value) {
        let Some(choices) = event.get("choices").and_then(Value::as_array) else {
            return;
        };
        if self.meta_id.is_none() {
            self.meta_id = event.get("id").and_then(Value::as_str).map(str::to_owned);
            self.meta_created = event.get("created").and_then(Value::as_u64).unwrap_or(0);
            self.meta_model = event
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
        for choice in choices {
            let index = choice.get("index").and_then(Value::as_u64).unwrap_or(0) as u32;
            let delta = choice.get("delta").and_then(Value::as_object);
            if delta
                .and_then(|d| d.get("content"))
                .is_some_and(|c| !c.as_str().is_some_and(str::is_empty))
            {
                self.wrote.insert(index);
            }
            if delta.is_some_and(|d| {
                d.get("content").is_some()
                    || d.get("reasoning_content").is_some()
                    || d.get("reasoning").is_some()
            }) {
                self.open.insert(index);
            }
            if delta
                .and_then(|d| d.get("tool_calls"))
                .and_then(Value::as_array)
                .is_some_and(|calls| !calls.is_empty())
            {
                self.tools.insert(index);
            }
            if choice.get("finish_reason").is_some() {
                self.done.insert(index);
            }
        }
    }

    fn end_frames(&self) -> Option<Vec<u8>> {
        if self.open.is_empty() && self.tools.is_empty() && self.done.is_empty() {
            return None;
        }
        if self.tools.iter().any(|index| !self.done.contains(index)) {
            return None;
        }
        let mut left: Vec<u32> = self
            .open
            .iter()
            .filter(|index| !self.done.contains(index))
            .copied()
            .collect();
        left.sort_unstable();
        if left.is_empty() {
            return Some(b"data: [DONE]\n\n".to_vec());
        }
        let id = self.meta_id.as_deref().unwrap_or("chatcmpl-cut-short");
        let created = self.meta_created;
        let model = self.meta_model.as_deref().unwrap_or("");
        let mut out = Vec::new();
        let note_choices: Vec<Value> = left
            .iter()
            .map(|index| {
                let prefix = if self.wrote.contains(index) {
                    "\n\n"
                } else {
                    ""
                };
                hofund_plugin_sdk::json::json!({
                    "index": index,
                    "delta": {"content": format!("{prefix}{CUT_SHORT_NOTE}")},
                    "finish_reason": null
                })
            })
            .collect();
        write_sse(
            &mut out,
            Wire::Chat,
            &hofund_plugin_sdk::json::json!({
                "id": id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": note_choices
            }),
        );
        let stop_choices: Vec<Value> = left
            .iter()
            .map(|index| {
                hofund_plugin_sdk::json::json!({
                    "index": index,
                    "delta": {},
                    "finish_reason": "stop"
                })
            })
            .collect();
        write_sse(
            &mut out,
            Wire::Chat,
            &hofund_plugin_sdk::json::json!({
                "id": id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": stop_choices
            }),
        );
        out.extend_from_slice(b"data: [DONE]\n\n");
        Some(out)
    }
}

fn parse_lines(raw: &[u8], wrapped: bool) -> Result<Vec<ParsedLine>, String> {
    let mut out = Vec::new();
    for line in raw.split(|b| *b == b'\n') {
        let line = std::str::from_utf8(line).map_err(|_| "Zed sent a non-UTF8 line")?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: Value = hofund_plugin_sdk::json::from_str(trimmed)
            .map_err(|_| "Zed sent an unreadable line")?;
        if !wrapped {
            out.push(ParsedLine::Event(value));
            continue;
        }
        if value.get("event").is_some() {
            out.push(ParsedLine::Event(
                value.get("event").cloned().unwrap_or(Value::Null),
            ));
            continue;
        }
        if value.get("status").and_then(Value::as_str) == Some("stream_ended") {
            out.push(ParsedLine::Ended);
            continue;
        }
        if let Some(failed) = value.get("status").and_then(|s| s.get("failed")) {
            let code = failed
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("upstream_error")
                .to_owned();
            let message = failed
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or(&code)
                .to_owned();
            out.push(ParsedLine::Failed { code, message });
            continue;
        }
    }
    Ok(out)
}

fn whole_wrapped(lines: Vec<ParsedLine>, wire: Wire) -> Result<BodyStream, String> {
    let mut events = Vec::new();
    let mut ended = false;
    for line in lines {
        match line {
            ParsedLine::Ended => ended = true,
            ParsedLine::Failed { code, message } => {
                let status = errors::failed_status(&code);
                return Err(format!("{status}:{message}"));
            }
            ParsedLine::Event(event) => {
                if let Some(message) = event_error(wire, &event) {
                    return Err(format!("502:{message}"));
                }
                events.push(event);
            }
        }
    }
    if !ended {
        return Err("502:the reply ended before it was complete".to_owned());
    }
    let mut out = Vec::new();
    for event in events {
        write_sse(&mut out, wire, &event);
    }
    if out.is_empty() {
        return Err("502:the reply ended before it was complete".to_owned());
    }
    if wire == Wire::Chat {
        out.extend_from_slice(b"data: [DONE]\n\n");
    }
    Ok(BodyStream::from_bytes(out))
}

fn streamed_wrapped(lines: Vec<ParsedLine>, wire: Wire) -> Result<BodyStream, String> {
    let mut head = Vec::new();
    let mut rest = Vec::new();
    let mut ended = false;
    let mut first_said: Option<Value> = None;
    for line in lines {
        match line {
            ParsedLine::Ended => {
                ended = true;
                continue;
            }
            ParsedLine::Failed { code, message } => {
                if first_said.is_none() {
                    let status = errors::failed_status(&code);
                    return Err(format!("{status}:{message}"));
                }
                rest.push(ParsedLine::Failed { code, message });
                continue;
            }
            ParsedLine::Event(event) => {
                if let Some(message) = event_error(wire, &event) {
                    if first_said.is_none() {
                        return Err(format!("502:{message}"));
                    }
                    rest.push(ParsedLine::Event(json_error_event(wire, &message)));
                    continue;
                }
                if first_said.is_none() {
                    if said(wire, &event) {
                        first_said = Some(event);
                    } else {
                        head.push(event);
                    }
                } else {
                    rest.push(ParsedLine::Event(event));
                }
            }
        }
    }
    let Some(first) = first_said else {
        if !ended {
            return Err("502:the reply ended before it was complete".to_owned());
        }
        return Err("502:Zed ended without an answer".to_owned());
    };

    let mut out = Vec::new();
    let mut chat = ChatTracker::default();
    for event in &head {
        if wire == Wire::Chat {
            chat.see(event);
        }
        write_sse(&mut out, wire, event);
    }
    if wire == Wire::Chat {
        chat.see(&first);
    }
    write_sse(&mut out, wire, &first);
    for line in rest {
        match line {
            ParsedLine::Failed { code, message } => {
                let status = errors::failed_status(&code);
                write_sse_error(&mut out, wire, status, &message);
            }
            ParsedLine::Event(event) => {
                if wire == Wire::Chat {
                    chat.see(&event);
                }
                write_sse(&mut out, wire, &event);
            }
            ParsedLine::Ended => {}
        }
    }
    if !ended {
        if wire == Wire::Chat {
            if let Some(frames) = chat.end_frames() {
                out.extend_from_slice(&frames);
            } else {
                return Err("502:the reply ended before it was complete".to_owned());
            }
        } else {
            write_sse_error(
                &mut out,
                wire,
                502,
                "the reply ended before it was complete",
            );
        }
    } else if wire == Wire::Chat {
        out.extend_from_slice(b"data: [DONE]\n\n");
    }
    Ok(BodyStream::from_bytes(out))
}

fn json_error_event(wire: Wire, message: &str) -> Value {
    match wire {
        Wire::Anthropic => {
            hofund_plugin_sdk::json::json!({"type":"error","error":{"type":"api_error","message": message}})
        }
        Wire::Responses => hofund_plugin_sdk::json::json!({"type":"error","message": message}),
        Wire::Chat | Wire::Gemini => {
            hofund_plugin_sdk::json::json!({"error":{"message": message}})
        }
    }
}

fn write_sse_error(out: &mut Vec<u8>, wire: Wire, status: u16, message: &str) {
    let event = json_error_event(wire, message);
    match wire {
        Wire::Anthropic | Wire::Responses => {
            out.extend_from_slice(b"event: error\n");
        }
        Wire::Chat | Wire::Gemini => {}
    }
    let _ = status;
    write_sse(out, wire, &event);
}

fn write_sse(out: &mut Vec<u8>, wire: Wire, event: &Value) {
    let data = format!("data: {}\n\n", event);
    match wire {
        Wire::Anthropic | Wire::Responses => {
            if let Some(kind) = event.get("type").and_then(Value::as_str) {
                out.extend_from_slice(format!("event: {kind}\n").as_bytes());
            }
        }
        Wire::Chat | Wire::Gemini => {}
    }
    out.extend_from_slice(data.as_bytes());
}

fn event_error(wire: Wire, event: &Value) -> Option<String> {
    if !event.is_object() {
        return None;
    }
    match wire {
        Wire::Anthropic if event.get("type").and_then(Value::as_str) == Some("error") => event
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        Wire::Responses => {
            let ty = event.get("type").and_then(Value::as_str).unwrap_or("");
            if matches!(
                ty,
                "response.completed" | "response.incomplete" | "response.failed"
            ) && let Some(msg) = event
                .get("response")
                .and_then(|r| r.get("error"))
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
            {
                return Some(msg.to_owned());
            }
            if ty == "error" {
                return event
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .or_else(|| event.get("message"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            None
        }
        Wire::Chat | Wire::Gemini => event
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        _ => None,
    }
}

fn said(wire: Wire, event: &Value) -> bool {
    if !event.is_object() {
        return true;
    }
    match wire {
        Wire::Anthropic => match event.get("type").and_then(Value::as_str) {
            Some("message_start" | "ping" | "content_block_stop") => false,
            Some("content_block_start") => {
                let ty = event
                    .get("content_block")
                    .and_then(|b| b.get("type"))
                    .and_then(Value::as_str);
                !matches!(ty, Some("text" | "thinking" | "redacted_thinking"))
                    || event
                        .get("content_block")
                        .and_then(|b| b.get("text").or_else(|| b.get("thinking")))
                        .is_some_and(|v| !v.as_str().is_some_and(str::is_empty))
            }
            Some("content_block_delta") => {
                let d = event.get("delta").and_then(Value::as_object);
                d.is_some_and(|delta| {
                    !(delta.get("text").and_then(Value::as_str) == Some("")
                        || delta.get("thinking").and_then(Value::as_str) == Some("")
                        || delta.get("partial_json").and_then(Value::as_str) == Some(""))
                })
            }
            _ => true,
        },
        Wire::Responses => match event.get("type").and_then(Value::as_str) {
            Some(
                "response.created"
                | "response.in_progress"
                | "response.queued"
                | "response.content_part.added"
                | "response.reasoning_summary_part.added",
            ) => false,
            Some("response.output_item.added") => event
                .get("item")
                .and_then(|i| i.get("type"))
                .and_then(Value::as_str)
                .is_some_and(|ty| !matches!(ty, "message" | "reasoning")),
            Some(
                "response.output_text.delta"
                | "response.reasoning_summary_text.delta"
                | "response.reasoning_text.delta",
            ) => event.get("delta").and_then(Value::as_str) != Some(""),
            _ => true,
        },
        Wire::Chat => {
            if event.get("error").is_some() {
                return true;
            }
            let Some(choices) = event.get("choices").and_then(Value::as_array) else {
                return false;
            };
            choices.iter().any(|choice| {
                let delta = choice.get("delta").and_then(Value::as_object);
                choice.get("finish_reason").is_some()
                    || delta.is_some_and(|d| {
                        d.get("content").is_some()
                            || d.get("reasoning_content").is_some()
                            || d.get("reasoning").is_some()
                            || d.get("tool_calls")
                                .and_then(Value::as_array)
                                .is_some_and(|t| !t.is_empty())
                    })
            })
        }
        Wire::Gemini => {
            if event.get("error").is_some() {
                return true;
            }
            event
                .get("candidates")
                .and_then(Value::as_array)
                .is_some_and(|candidates| {
                    candidates.iter().any(|candidate| {
                        candidate.get("finishReason").is_some()
                            || candidate
                                .get("content")
                                .and_then(|c| c.get("parts"))
                                .and_then(Value::as_array)
                                .is_some_and(|parts| {
                                    parts.iter().any(|part| {
                                        part.get("text").is_some()
                                            || part.get("functionCall").is_some()
                                            || part.get("thought").is_some()
                                    })
                                })
                    })
                })
        }
    }
}
