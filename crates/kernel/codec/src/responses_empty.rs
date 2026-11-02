//! Detects an empty OpenAI Responses terminal reply for failover.

use serde_json::{Map, Value};

/// Router and gateway treat this as a shape failure that does not rest the member.
pub const EMPTY_REPLY_MESSAGE: &str = "an empty reply (the model answered nothing; try again)";

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
    for name in names {
        if let Some(value) = usage.get(*name).and_then(as_i64) {
            return value.max(0);
        }
    }
    0
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
    use serde_json::json;

    use super::{completed_event_empty, relayed_sse_empty_without_content, response_body_empty};

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
