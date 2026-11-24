//! Detects empty Responses and Chat replies for failover.

use hofund_ir::{ContentPart, Response, StreamEvent, Usage};
use serde_json::{Map, Value};

/// Router and gateway treat this as a shape failure that does not rest the member.
pub const EMPTY_REPLY_MESSAGE: &str = "an empty reply (the model answered nothing; try again)";

/// Formats an empty-reply failure with the output and reasoning tokens counted.
#[must_use]
pub fn empty_reply_message(usage: &Usage) -> String {
    let output = usage.output_tokens.unwrap_or(0);
    let reasoning = usage.reasoning_tokens.unwrap_or(0);
    let detail = if reasoning > 0 && output >= reasoning {
        format!(
            "the model reasoned for {reasoning} of the {output} output tokens it counted, and its reply carried no text or tool call"
        )
    } else if reasoning > 0 {
        format!(
            "the model counted {reasoning} reasoning tokens, and its reply carried no text or tool call"
        )
    } else if output > 0 {
        format!(
            "the model counted {output} output tokens, and its reply carried no text or tool call"
        )
    } else {
        return EMPTY_REPLY_MESSAGE.to_owned();
    };
    format!(
        "{EMPTY_REPLY_MESSAGE} — {detail}; if trying again ends the same way, switch to another model or compact the conversation"
    )
}

/// Incremental state for a relayed Chat event stream.
///
/// Role-only chunks and usage do not start a reply. Text, refusal, reasoning,
/// a tool call, or a non-empty finish reason does.
#[derive(Debug, Default)]
pub struct ChatStreamCheck {
    pending: Vec<u8>,
    reply: bool,
    terminal: bool,
    ended: bool,
    opaque: bool,
    usage: Usage,
}

impl ChatStreamCheck {
    /// Reads another upstream byte chunk.
    pub fn observe(&mut self, bytes: &[u8]) {
        if self.opaque {
            return;
        }
        self.pending.extend_from_slice(bytes);
        while let Some(end) = self.pending.iter().position(|byte| *byte == b'\n') {
            let mut line: Vec<u8> = self.pending.drain(..=end).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.line(&line);
        }
    }

    /// Marks a clean upstream EOF.
    pub fn finish(&mut self) {
        if !self.opaque && !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            let line = line.strip_suffix(b"\r").unwrap_or(&line);
            self.line(line);
        }
        self.ended = true;
    }

    /// Whether the stream carried output or a finish reason.
    #[must_use]
    pub const fn has_reply(&self) -> bool {
        self.reply
    }

    /// Whether `[DONE]` has arrived.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        self.terminal
    }

    /// Whether these bytes are not a JSON Chat event stream.
    #[must_use]
    pub const fn is_opaque(&self) -> bool {
        self.opaque
    }

    /// Usage for an empty terminal reply, or `None` for a real reply.
    #[must_use]
    pub fn empty_usage(&self) -> Option<Usage> {
        ((self.terminal || self.ended) && !self.reply && !self.opaque).then(|| self.usage.clone())
    }

    fn line(&mut self, line: &[u8]) {
        if line.is_empty()
            || line.starts_with(b":")
            || line.starts_with(b"event:")
            || line.starts_with(b"id:")
            || line.starts_with(b"retry:")
        {
            return;
        }
        let Some(data) = line.strip_prefix(b"data:") else {
            self.opaque = true;
            return;
        };
        let data = data.strip_prefix(b" ").unwrap_or(data);
        if data.is_empty() {
            return;
        }
        if data == b"[DONE]" {
            self.terminal = true;
            return;
        }
        let Ok(value) = serde_json::from_slice::<Value>(data) else {
            self.opaque = true;
            return;
        };
        let Some(object) = value.as_object() else {
            self.opaque = true;
            return;
        };
        if object.contains_key("error") {
            self.opaque = true;
            return;
        }
        if let Some(choices) = object.get("choices").and_then(Value::as_array) {
            for choice in choices {
                if choice
                    .get("finish_reason")
                    .and_then(Value::as_str)
                    .is_some_and(|reason| !reason.is_empty())
                {
                    self.reply = true;
                }
                for key in ["delta", "message"] {
                    if let Some(delta) = choice.get(key).and_then(Value::as_object) {
                        self.delta(delta);
                    }
                }
            }
        }
        if let Some(usage) = object.get("usage").and_then(Value::as_object) {
            self.note_usage(usage);
        }
    }

    fn delta(&mut self, delta: &Map<String, Value>) {
        self.reply |= map_says(delta);
    }

    fn note_usage(&mut self, usage: &Map<String, Value>) {
        if let Some(output) = optional_token_field(usage, &["completion_tokens", "output_tokens"]) {
            self.usage.output_tokens = u64::try_from(output).ok();
        }
        let top = optional_token_field(usage, &["reasoning_tokens"]);
        let details = usage
            .get("completion_tokens_details")
            .and_then(Value::as_object)
            .and_then(|details| optional_token_field(details, &["reasoning_tokens"]));
        let reasoning = match details {
            Some(count) if count > 0 => Some(count),
            other => top.or(other),
        };
        if let Some(reasoning) = reasoning {
            self.usage.reasoning_tokens = u64::try_from(reasoning).ok();
        }
    }
}

fn map_says(delta: &Map<String, Value>) -> bool {
    [
        "content",
        "refusal",
        "reasoning_content",
        "reasoning",
        "function_call",
    ]
    .into_iter()
    .any(|key| raw_says(delta.get(key)))
        || ["tool_calls", "reasoning_details"].into_iter().any(|key| {
            delta
                .get(key)
                .and_then(Value::as_array)
                .is_some_and(|items| !items.is_empty())
        })
}

fn raw_says(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::String(text)) => !text.is_empty(),
        Some(_) => true,
    }
}

/// Whether a decoded Chat stream carried no output and no finish reason.
/// Returns its merged usage when it is empty.
#[must_use]
pub fn decoded_chat_stream_empty(events: &[StreamEvent]) -> Option<Usage> {
    let mut reply = false;
    let mut usage = Usage::default();
    for event in events {
        match event {
            StreamEvent::TextDelta(delta) if !delta.text.is_empty() => reply = true,
            StreamEvent::ThinkingDelta(delta) if !delta.text.is_empty() => reply = true,
            StreamEvent::ToolArgumentsDelta(delta) if !delta.partial_json.is_empty() => {
                reply = true;
            }
            StreamEvent::ContentBlockStart(start)
                if matches!(start.block, hofund_ir::BlockStart::ToolCall { .. }) =>
            {
                reply = true;
            }
            StreamEvent::MessageStart(start) => {
                if let Some(patch) = &start.usage {
                    usage.merge_from(patch);
                }
            }
            StreamEvent::MessageDelta(delta) => {
                reply |= delta.stop.is_some() || map_says(delta.extensions.as_map());
                if let Some(patch) = &delta.usage {
                    usage.merge_from(patch);
                }
            }
            _ => {}
        }
    }
    (!reply).then_some(usage)
}

/// Whether a whole Chat response has no output and no finish reason.
#[must_use]
pub fn chat_response_empty(response: &Response) -> bool {
    response.stop.is_none()
        && !response.message.content.iter().any(|part| match part {
            ContentPart::Text(text) => !text.text.is_empty(),
            ContentPart::Thinking(thinking) => {
                !thinking.text.is_empty() || thinking.signature.is_some() || thinking.data.is_some()
            }
            ContentPart::Unknown(_) => false,
            _ => true,
        })
}

/// Whether a `response.completed` event object names empty output and zero
/// billed tokens. Missing `output` or `usage` is not an empty reply.
#[must_use]
pub fn completed_event_empty(event: &Map<String, Value>) -> bool {
    let Some(response) = event.get("response").and_then(Value::as_object) else {
        return false;
    };
    response_empty_with_zero_usage(response)
}

/// Whether a whole Responses JSON body is a completed turn with empty output
/// and zero usage.
#[must_use]
pub fn response_body_empty(body: &Map<String, Value>) -> bool {
    if body.get("object").and_then(Value::as_str) == Some("response") || body.contains_key("output")
    {
        return response_empty_with_zero_usage(body);
    }
    completed_event_empty(body)
}

/// Whether buffered SSE lines contain a terminal empty completed before any
/// reply content (`output_text`, `function_call_arguments`, or non-empty
/// `output_item.done` text).
#[must_use]
pub fn relayed_sse_empty_without_content(sse: &[u8]) -> bool {
    let mut saw_content = false;
    let mut terminal_empty = false;
    for line in sse.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some(data) = line.strip_prefix(b"data:") else {
            continue;
        };
        let data = data.strip_prefix(b" ").unwrap_or(data);
        if data == b"[DONE]" {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<Value>(data) else {
            continue;
        };
        let Some(object) = value.as_object() else {
            continue;
        };
        let kind = object
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if matches!(
            kind,
            "response.output_text.delta"
                | "response.output_text.done"
                | "response.function_call_arguments.delta"
                | "response.function_call_arguments.done"
        ) {
            saw_content = true;
        }
        if kind == "response.output_item.done"
            && object
                .get("item")
                .and_then(|item| item.get("content"))
                .is_some_and(content_has_text)
        {
            saw_content = true;
        }
        if kind == "response.completed" && completed_event_empty(object) {
            terminal_empty = true;
        }
    }
    terminal_empty && !saw_content
}

/// Whether a decoded Responses stream ended with no visible content and zero
/// usage on the terminal delta.
#[must_use]
pub fn decoded_stream_empty(events: &[hofund_ir::StreamEvent]) -> bool {
    use hofund_ir::StreamEvent;
    let mut saw_content = false;
    let mut terminal_usage_zero = false;
    let mut saw_terminal = false;
    for event in events {
        match event {
            StreamEvent::TextDelta(delta) if !delta.text.is_empty() => saw_content = true,
            StreamEvent::ThinkingDelta(delta) if !delta.text.is_empty() => saw_content = true,
            StreamEvent::ToolArgumentsDelta(delta) if !delta.partial_json.is_empty() => {
                saw_content = true;
            }
            StreamEvent::ContentBlockStart(start)
                if matches!(start.block, hofund_ir::BlockStart::ToolCall { .. }) =>
            {
                saw_content = true;
            }
            StreamEvent::MessageDelta(delta) => {
                if delta.stop.is_some() {
                    saw_terminal = true;
                    terminal_usage_zero = delta
                        .usage
                        .as_ref()
                        .map(|usage| {
                            usage.input_tokens.unwrap_or(0) + usage.output_tokens.unwrap_or(0) == 0
                        })
                        .unwrap_or(false);
                }
            }
            StreamEvent::MessageStop(_) => saw_terminal = true,
            _ => {}
        }
        if let StreamEvent::MessageStart(start) = event
            && start
                .usage
                .as_ref()
                .is_some_and(|usage| usage.input_tokens.unwrap_or(0) > 0)
        {
            saw_content = true;
        }
        if let StreamEvent::Unknown(unknown) = event
            && unknown.name == "response.completed"
            && let Some(object) = unknown.payload.as_object()
            && completed_event_empty(object)
        {
            saw_terminal = true;
            terminal_usage_zero = true;
        }
    }
    saw_terminal && terminal_usage_zero && !saw_content
}

/// Whether relayed SSE already carries reply content (not only lifecycle).
#[must_use]
pub fn relayed_sse_has_content(sse: &[u8]) -> bool {
    for line in sse.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some(data) = line.strip_prefix(b"data:") else {
            continue;
        };
        let data = data.strip_prefix(b" ").unwrap_or(data);
        if data == b"[DONE]" {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<Value>(data) else {
            continue;
        };
        let Some(object) = value.as_object() else {
            continue;
        };
        let kind = object
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if matches!(
            kind,
            "response.output_text.delta"
                | "response.output_text.done"
                | "response.function_call_arguments.delta"
                | "response.function_call_arguments.done"
        ) {
            return true;
        }
        if kind == "response.output_item.done"
            && object
                .get("item")
                .and_then(|item| item.get("content"))
                .is_some_and(content_has_text)
        {
            return true;
        }
    }
    false
}

/// Whether relayed SSE carries non-Responses payload bytes (plain text or
/// non-JSON `data:` lines). Those streams must not be read until a terminal
/// `response.completed` before handing the body to the client.
#[must_use]
pub fn relayed_sse_has_opaque_content(sse: &[u8]) -> bool {
    for line in sse.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        if let Some(data) = line.strip_prefix(b"data:") {
            let data = data.strip_prefix(b" ").unwrap_or(data);
            if data == b"[DONE]" || data.is_empty() {
                continue;
            }
            if serde_json::from_slice::<Value>(data).is_err() {
                return true;
            }
            continue;
        }
        if line.starts_with(b":")
            || line.starts_with(b"event:")
            || line.starts_with(b"id:")
            || line.starts_with(b"retry:")
        {
            continue;
        }
        return true;
    }
    false
}

/// Whether `sse` already contains a full SSE event (blank line delimiter).
#[must_use]
pub fn relayed_sse_event_boundary(sse: &[u8]) -> bool {
    sse.windows(2).any(|window| window == b"\n\n")
}

/// Whether every `data:` event in `sse` so far is a Responses lifecycle event
/// (a JSON object whose `type` starts with `response.`), with at least one.
///
/// Such a stream is still held after an event boundary, so an empty
/// `response.completed` arriving in a later chunk can still fail over.
#[must_use]
pub fn relayed_sse_lifecycle_only(sse: &[u8]) -> bool {
    let mut saw = false;
    for line in sse.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some(data) = line.strip_prefix(b"data:") else {
            continue;
        };
        let data = data.strip_prefix(b" ").unwrap_or(data);
        if data == b"[DONE]" || data.is_empty() {
            continue;
        }
        let lifecycle = serde_json::from_slice::<Value>(data).is_ok_and(|value| {
            value
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind.starts_with("response."))
        });
        if !lifecycle {
            return false;
        }
        saw = true;
    }
    saw
}

fn response_empty_with_zero_usage(response: &Map<String, Value>) -> bool {
    let output_empty = match response.get("output") {
        None => return false,
        Some(Value::Null) => true,
        Some(Value::Array(items)) => items.is_empty(),
        Some(_) => false,
    };
    if !output_empty {
        return false;
    }
    let Some(usage) = response.get("usage").and_then(Value::as_object) else {
        return false;
    };
    billed_tokens(usage) == 0
}

fn billed_tokens(usage: &Map<String, Value>) -> i64 {
    let input = token_field(
        usage,
        &[
            "input_tokens",
            "prompt_tokens",
            "inputTokenCount",
            "promptTokenCount",
        ],
    );
    let output = token_field(
        usage,
        &[
            "output_tokens",
            "completion_tokens",
            "outputTokenCount",
            "candidatesTokenCount",
        ],
    );
    input.saturating_add(output)
}

fn token_field(usage: &Map<String, Value>, names: &[&str]) -> i64 {
    optional_token_field(usage, names).unwrap_or(0)
}

fn optional_token_field(usage: &Map<String, Value>, names: &[&str]) -> Option<i64> {
    for name in names {
        if let Some(value) = usage.get(*name).and_then(as_i64) {
            return Some(value.max(0));
        }
    }
    None
}

fn as_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|v| i64::try_from(v).ok()))
}

fn content_has_text(content: &Value) -> bool {
    let Some(items) = content.as_array() else {
        return false;
    };
    items.iter().any(|part| {
        part.get("text")
            .and_then(Value::as_str)
            .is_some_and(|text| !text.is_empty())
    })
}

#[cfg(test)]
mod tests {
    use hofund_ir::Usage;
    use serde_json::json;

    use super::{
        ChatStreamCheck, completed_event_empty, decoded_chat_stream_empty, empty_reply_message,
        relayed_sse_empty_without_content, response_body_empty,
    };
    use crate::{Codec, OpenAiChat};

    #[test]
    fn d248_chat_usage_only_stream_is_empty_at_done_or_eof() {
        let role =
            "data: {\"choices\":[{\"delta\":{\"content\":null,\"role\":\"assistant\"}}]}\n\n";
        let usage = "data: {\"choices\":[],\"usage\":{\"completion_tokens\":2359,\"prompt_tokens\":91000,\"reasoning_tokens\":2351}}\n\n";
        for terminal in ["", "data: [DONE]\n\n"] {
            let bytes = format!("{role}{usage}{terminal}");
            let mut check = ChatStreamCheck::default();
            for chunk in bytes.as_bytes().chunks(7) {
                check.observe(chunk);
            }
            check.finish();
            let usage = check.empty_usage().expect("empty reply");
            assert_eq!(usage.output_tokens, Some(2359));
            assert_eq!(usage.reasoning_tokens, Some(2351));
            assert!(!check.is_opaque());
        }
    }

    #[test]
    fn d248_chat_output_or_a_finish_reason_is_not_empty() {
        for delta in [
            "{\"content\":\"hello\"}",
            "{\"refusal\":\"no\"}",
            "{\"reasoning_content\":\"thought\"}",
            "{\"reasoning\":\"thought\"}",
            "{\"reasoning_details\":[{}]}",
            "{\"tool_calls\":[{}]}",
            "{\"function_call\":{}}",
        ] {
            let bytes =
                format!("data: {{\"choices\":[{{\"delta\":{delta}}}]}}\n\ndata: [DONE]\n\n");
            let mut check = ChatStreamCheck::default();
            check.observe(bytes.as_bytes());
            assert!(check.has_reply(), "{delta}");
            assert!(check.empty_usage().is_none(), "{delta}");
        }
        let stopped =
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        let mut check = ChatStreamCheck::default();
        check.observe(stopped.as_bytes());
        assert!(check.empty_usage().is_none());

        let text_without_finish = "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\n";
        let mut check = ChatStreamCheck::default();
        check.observe(text_without_finish.as_bytes());
        check.finish();
        assert!(check.empty_usage().is_none());
    }

    #[test]
    fn d248_decoded_chat_usage_only_stream_is_empty_not_stopped() {
        let stream = concat!(
            "data: {\"id\":\"c\",\"model\":\"m\",\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
            "data: {\"id\":\"c\",\"model\":\"m\",\"choices\":[],\"usage\":{\"completion_tokens\":12,\"reasoning_tokens\":9}}\n\n",
            "data: [DONE]\n\n"
        );
        let events = OpenAiChat.decode_stream(stream.as_bytes()).expect("decode");
        let usage = decoded_chat_stream_empty(&events).expect("empty");
        assert_eq!(usage.output_tokens, Some(12));
        assert_eq!(usage.reasoning_tokens, Some(9));

        let stopped =
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        let events = OpenAiChat
            .decode_stream(stopped.as_bytes())
            .expect("decode");
        assert!(decoded_chat_stream_empty(&events).is_none());

        for delta in [
            "{\"reasoning\":\"thought\"}",
            "{\"reasoning_details\":[{}]}",
            "{\"refusal\":\"no\"}",
            "{\"function_call\":{}}",
        ] {
            let stream =
                format!("data: {{\"choices\":[{{\"delta\":{delta}}}]}}\n\ndata: [DONE]\n\n");
            let events = OpenAiChat.decode_stream(stream.as_bytes()).expect("decode");
            assert!(decoded_chat_stream_empty(&events).is_none(), "{delta}");
        }
    }

    #[test]
    fn d248_empty_reply_words_include_counted_output_and_reasoning() {
        assert_eq!(
            empty_reply_message(&Usage::default()),
            super::EMPTY_REPLY_MESSAGE
        );
        assert_eq!(
            empty_reply_message(&Usage {
                input_tokens: Some(9),
                ..Usage::default()
            }),
            super::EMPTY_REPLY_MESSAGE
        );
        assert_eq!(
            empty_reply_message(&Usage {
                output_tokens: Some(40),
                ..Usage::default()
            }),
            "an empty reply (the model answered nothing; try again) — the model counted 40 output tokens, and its reply carried no text or tool call; if trying again ends the same way, switch to another model or compact the conversation"
        );
        assert_eq!(
            empty_reply_message(&Usage {
                output_tokens: Some(10),
                reasoning_tokens: Some(30),
                ..Usage::default()
            }),
            "an empty reply (the model answered nothing; try again) — the model counted 30 reasoning tokens, and its reply carried no text or tool call; if trying again ends the same way, switch to another model or compact the conversation"
        );
        assert_eq!(
            empty_reply_message(&Usage {
                output_tokens: Some(2359),
                reasoning_tokens: Some(2351),
                ..Usage::default()
            }),
            "an empty reply (the model answered nothing; try again) — the model reasoned for 2351 of the 2359 output tokens it counted, and its reply carried no text or tool call; if trying again ends the same way, switch to another model or compact the conversation"
        );
    }

    #[test]
    fn d37_1_empty_completed_is_detected() {
        let event = json!({
            "type": "response.completed",
            "response": {
                "status": "completed",
                "output": [],
                "usage": {"input_tokens": 0, "output_tokens": 0}
            }
        });
        assert!(completed_event_empty(event.as_object().expect("object")));
    }

    #[test]
    fn d37_2_nonempty_output_is_not_empty() {
        let event = json!({
            "type": "response.completed",
            "response": {
                "output": [{"type": "message", "content": []}],
                "usage": {"input_tokens": 0, "output_tokens": 0}
            }
        });
        assert!(!completed_event_empty(event.as_object().expect("object")));
    }

    #[test]
    fn d37_3_empty_output_with_usage_stays_a_reply() {
        let event = json!({
            "type": "response.completed",
            "response": {
                "output": [],
                "usage": {"input_tokens": 3, "output_tokens": 0}
            }
        });
        assert!(!completed_event_empty(event.as_object().expect("object")));
    }

    #[test]
    fn d37_4_missing_both_fields_is_not_empty() {
        let event = json!({"type": "response.completed", "response": {"status": "completed"}});
        assert!(!completed_event_empty(event.as_object().expect("object")));
    }

    #[test]
    fn d37_5_unary_body_empty() {
        let body = json!({
            "id": "r",
            "status": "completed",
            "output": null,
            "usage": {"input_tokens": 0, "output_tokens": 0}
        });
        assert!(response_body_empty(body.as_object().expect("object")));
    }

    #[test]
    fn d37_6_relayed_sse_empty() {
        let sse = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"output\":[]}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"output\":[],\"usage\":{\"input_tokens\":0,\"output_tokens\":0}}}\n\n",
        );
        assert!(relayed_sse_empty_without_content(sse.as_bytes()));
    }

    #[test]
    fn d37_7_lifecycle_only_holds_responses_and_releases_others() {
        let created = "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"output\":[]}}\n\n";
        assert!(super::relayed_sse_lifecycle_only(created.as_bytes()));
        let chat = "data: {\"object\":\"chat.completion.chunk\",\"choices\":[]}\n\n";
        assert!(!super::relayed_sse_lifecycle_only(chat.as_bytes()));
        let messages = "event: message_start\ndata: {\"type\":\"message_start\"}\n\n";
        assert!(!super::relayed_sse_lifecycle_only(messages.as_bytes()));
        let mixed = format!("{created}{chat}");
        assert!(!super::relayed_sse_lifecycle_only(mixed.as_bytes()));
        assert!(!super::relayed_sse_lifecycle_only(b": ping\n\n"));
        assert!(!super::relayed_sse_lifecycle_only(b""));
    }

    #[test]
    fn d37_8_sse_fields_are_not_opaque() {
        let sse = "event: response.created\nid: 1\nretry: 10\n: ping\ndata: {\"type\":\"response.created\"}\n\n";
        assert!(!super::relayed_sse_has_opaque_content(sse.as_bytes()));
        assert!(super::relayed_sse_has_opaque_content(
            b"data: plain words\n\n"
        ));
        assert!(super::relayed_sse_has_opaque_content(b"hello there"));
    }
}
