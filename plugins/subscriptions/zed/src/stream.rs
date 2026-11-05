//! Turn Zed `/completions` NDJSON into provider SSE bytes for the wire decoder.

use hofund_plugin_api::BodyStream;
use hofund_plugin_sdk::json::Value;

use crate::body::Wire;
use crate::errors;

/// Reads all of `raw` NDJSON, wraps provider events as SSE, and returns one body stream.
pub(crate) fn ndjson_to_sse_body(
    raw: &[u8],
    wire: Wire,
    wrapped: bool,
) -> Result<BodyStream, String> {
    let mut out = Vec::new();
    let mut ended = false;
    for line in raw.split(|b| *b == b'\n') {
        let line = std::str::from_utf8(line).map_err(|_| "Zed sent a non-UTF8 line")?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: Value = hofund_plugin_sdk::json::from_str(trimmed)
            .map_err(|_| "Zed sent an unreadable line")?;
        if !wrapped {
            if let Some(event) = value.as_object().and_then(|_| value.get("event")) {
                write_sse(&mut out, wire, event);
            }
            continue;
        }
        if value.get("event").is_some() {
            write_sse(&mut out, wire, value.get("event").unwrap());
            continue;
        }
        if value.get("status").and_then(Value::as_str) == Some("stream_ended") {
            ended = true;
            continue;
        }
        if let Some(failed) = value.get("status").and_then(|s| s.get("failed")) {
            let code = failed
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("upstream_error");
            let message = failed
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or(code);
            let status = errors::failed_status(code);
            return Err(format!("{status}:{message}"));
        }
    }
    if wrapped && !ended && !out.is_empty() {
        return Err("502:the reply ended before it was complete".to_owned());
    }
    if out.is_empty() {
        return Err("502:Zed ended without an answer".to_owned());
    }
    if wire == Wire::Chat {
        out.extend_from_slice(b"data: [DONE]\n\n");
    }
    Ok(BodyStream::from_bytes(out))
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
