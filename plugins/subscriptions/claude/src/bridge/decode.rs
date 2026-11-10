//! Decode Claude Code stream-json lines into IR events and bridge signals.
//!
//! A text-only turn feeds Claude Code stdout into this decoder and returns
//! an upstream provider. Tool parking and error mapping stay in later slices.

use hofund_plugin_api::{Stop, StopReason, StreamEvent};
use serde::Deserialize;
use serde_json::{Value, json};

#[cfg(test)]
const MESSAGE_DELTA: &str = include_str!("fixtures/stream/message_delta.jsonl");
#[cfg(test)]
const TOOL_USE: &str = include_str!("fixtures/stream/tool_use.jsonl");
#[cfg(test)]
const RATE_LIMIT: &str = include_str!("fixtures/stream/rate_limit_event.jsonl");
#[cfg(test)]
const RESULT_ERROR: &str = include_str!("fixtures/stream/result_error.jsonl");

/// One decoded step from a stream-json line.
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    /// `system` / `init` reported a Claude Code session id.
    Session(String),
    /// IR events mapped from a `stream_event` envelope.
    Events(Vec<StreamEvent>),
    /// A `rate_limit_event`. It must not end the turn or become a terminal error.
    RateLimit(RateLimitInfo),
    /// A `result` envelope with `is_error`.
    ResultError(ResultError),
    /// A successful `result` envelope. Later slices may use this to finish a turn.
    Completed,
}

/// Allowance windows from a `rate_limit_event`.
#[derive(Clone, Debug, PartialEq)]
pub struct RateLimitInfo {
    /// `status` as Claude Code sent it (`allowed` appears on ordinary turns).
    pub status: Option<String>,
    /// `rateLimitType` when present.
    pub rate_limit_type: Option<String>,
}

/// Terminal error carried by a failed `result` envelope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResultError {
    /// `result` text, or joined `errors` when that field is empty.
    pub message: String,
    /// HTTP status Claude Code reported, when present.
    pub api_error_status: Option<u16>,
    /// Error kind string (`rate_limit`, `server_error`, …), when present.
    pub kind: Option<String>,
}

/// Incremental stream-json decoder.
#[derive(Debug, Default)]
pub struct Decoder {
    session_id: Option<String>,
}

impl Decoder {
    /// Builds an empty decoder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Session id captured from `system` / `init`, if any line has provided one.
    #[must_use]
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Decodes one stream-json line.
    ///
    /// Blank lines and envelopes this slice does not map (including
    /// unparseable JSON, matching the reference reader) return `None`.
    pub fn push_line(&mut self, line: &str) -> Option<Step> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        let envelope: Envelope = serde_json::from_str(line).ok()?;
        match envelope.typ.as_str() {
            "system" if envelope.subtype.as_deref() == Some("init") => {
                let id = envelope.session_id.filter(|id| !id.is_empty())?;
                self.session_id = Some(id.clone());
                Some(Step::Session(id))
            }
            "rate_limit_event" => Some(Step::RateLimit(rate_limit_info(&envelope))),
            "result" if envelope.is_error => Some(Step::ResultError(ResultError {
                message: result_message(&envelope),
                api_error_status: envelope.api_error_status,
                kind: error_kind(&envelope.error),
            })),
            "result" => Some(Step::Completed),
            "stream_event" => envelope
                .event
                .and_then(decode_stream_event)
                .map(Step::Events),
            _ => None,
        }
    }
}

fn decode_stream_event(event: StreamEnvelope) -> Option<Vec<StreamEvent>> {
    let events = match event.typ.as_str() {
        "message_start" => {
            let message = event.message.unwrap_or_default();
            vec![ir_event(json!({
                "type": "message_start",
                "id": empty_to_none(message.id),
                "model": empty_to_none(message.model),
                "usage": message.usage.and_then(CliUsage::into_usage),
            }))]
        }
        "content_block_start" => {
            let block = event.content_block?;
            let mut events = Vec::new();
            match block.typ.as_str() {
                "text" => {
                    events.push(ir_event(json!({
                        "type": "content_block_start",
                        "index": event.index,
                        "block": { "type": "text" },
                    })));
                    if !block.text.is_empty() {
                        events.push(ir_event(json!({
                            "type": "text_delta",
                            "index": event.index,
                            "text": block.text,
                        })));
                    }
                }
                "tool_use" => events.push(ir_event(json!({
                    "type": "content_block_start",
                    "index": event.index,
                    "block": {
                        "type": "tool_call",
                        "id": block.id.unwrap_or_default(),
                        "name": block.name.unwrap_or_default(),
                    },
                }))),
                "thinking" => events.push(ir_event(json!({
                    "type": "content_block_start",
                    "index": event.index,
                    "block": { "type": "thinking" },
                }))),
                "redacted_thinking" => events.push(ir_event(json!({
                    "type": "content_block_start",
                    "index": event.index,
                    "block": { "type": "thinking", "redacted": true },
                }))),
                _ => return None,
            }
            events
        }
        "content_block_delta" => {
            let delta = event.delta?;
            let ev = match delta.typ.as_str() {
                "text_delta" => ir_event(json!({
                    "type": "text_delta",
                    "index": event.index,
                    "text": delta.text,
                })),
                "input_json_delta" => ir_event(json!({
                    "type": "tool_arguments_delta",
                    "index": event.index,
                    "partial_json": delta.partial_json,
                })),
                "thinking_delta" => ir_event(json!({
                    "type": "thinking_delta",
                    "index": event.index,
                    "text": delta.thinking,
                })),
                "signature_delta" => ir_event(json!({
                    "type": "signature_delta",
                    "index": event.index,
                    "signature": delta.signature,
                })),
                _ => return None,
            };
            vec![ev]
        }
        "content_block_stop" => vec![ir_event(json!({
            "type": "content_block_stop",
            "index": event.index,
        }))],
        "message_delta" => {
            let reason = event
                .delta
                .as_ref()
                .and_then(|d| empty_to_none(d.stop_reason.clone()))
                .map(stop_reason);
            let sequence = event
                .delta
                .as_ref()
                .and_then(|d| d.stop_sequence.clone())
                .and_then(empty_to_none);
            let stop = reason.map(|reason| Stop { reason, sequence });
            vec![ir_event(json!({
                "type": "message_delta",
                "stop": stop,
                "usage": event.usage.and_then(CliUsage::into_usage),
            }))]
        }
        "message_stop" => vec![ir_event(json!({"type": "message_stop"}))],
        _ => return None,
    };
    Some(events)
}

fn rate_limit_info(envelope: &Envelope) -> RateLimitInfo {
    let info = envelope.rate_limit_info.as_ref();
    RateLimitInfo {
        status: envelope
            .status
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                info.and_then(|v| {
                    v.get("status")
                        .and_then(|s| s.as_str())
                        .map(ToOwned::to_owned)
                })
            }),
        rate_limit_type: info.and_then(|v| {
            v.get("rateLimitType")
                .and_then(|s| s.as_str())
                .map(ToOwned::to_owned)
        }),
    }
}

fn result_message(envelope: &Envelope) -> String {
    if !envelope.result.is_empty() {
        envelope.result.clone()
    } else {
        envelope.errors.join("; ")
    }
}

fn error_kind(value: &Option<serde_json::Value>) -> Option<String> {
    let value = value.as_ref()?;
    if let Some(s) = value.as_str() {
        return empty_to_none(s.to_owned());
    }
    value
        .get("type")
        .and_then(|v| v.as_str())
        .map(ToOwned::to_owned)
        .and_then(empty_to_none)
}

fn stop_reason(raw: String) -> StopReason {
    match raw.as_str() {
        "end_turn" => StopReason::EndTurn,
        "max_tokens" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence,
        "tool_use" => StopReason::ToolUse,
        "content_filter" => StopReason::ContentFilter,
        "refusal" => StopReason::Refusal,
        _ => StopReason::Other(raw),
    }
}

fn empty_to_none(s: String) -> Option<String> {
    if s.is_empty() { None } else { Some(s) }
}

/// Decodes every line of a script and keeps the decoder (for the session id).
#[cfg(test)]
pub(crate) fn decode_script(script: &str) -> (Decoder, Vec<Step>) {
    let mut decoder = Decoder::new();
    let steps = script
        .lines()
        .filter_map(|line| decoder.push_line(line))
        .collect();
    (decoder, steps)
}

#[cfg(test)]
fn collected_events(steps: &[Step]) -> Vec<&StreamEvent> {
    steps
        .iter()
        .filter_map(|step| match step {
            Step::Events(events) => Some(events),
            _ => None,
        })
        .flatten()
        .collect()
}

#[derive(Debug, Default, Deserialize)]
struct Envelope {
    #[serde(rename = "type")]
    typ: String,
    #[serde(default)]
    subtype: Option<String>,
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    result: String,
    #[serde(default)]
    errors: Vec<String>,
    #[serde(default)]
    api_error_status: Option<u16>,
    #[serde(default)]
    error: Option<serde_json::Value>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    rate_limit_info: Option<serde_json::Value>,
    #[serde(default)]
    event: Option<StreamEnvelope>,
}

#[derive(Debug, Default, Deserialize)]
struct StreamEnvelope {
    #[serde(rename = "type")]
    typ: String,
    #[serde(default)]
    index: u32,
    #[serde(default)]
    message: Option<StartMessage>,
    #[serde(default)]
    content_block: Option<StartBlock>,
    #[serde(default)]
    delta: Option<Delta>,
    #[serde(default)]
    usage: Option<CliUsage>,
}

#[derive(Debug, Default, Deserialize)]
struct StartMessage {
    #[serde(default)]
    id: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    usage: Option<CliUsage>,
}

#[derive(Debug, Default, Deserialize)]
struct StartBlock {
    #[serde(rename = "type")]
    typ: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    text: String,
}

#[derive(Debug, Default, Deserialize)]
struct Delta {
    #[serde(default, rename = "type")]
    typ: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    thinking: String,
    #[serde(default)]
    signature: String,
    #[serde(default)]
    partial_json: String,
    #[serde(default)]
    stop_reason: String,
    #[serde(default)]
    stop_sequence: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct CliUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens_details: Option<OutputDetails>,
}

#[derive(Debug, Default, Deserialize)]
struct OutputDetails {
    #[serde(default)]
    thinking_tokens: Option<u64>,
}

impl CliUsage {
    fn into_usage(self) -> Option<Value> {
        let mut usage = serde_json::Map::new();
        if let Some(v) = self.input_tokens {
            usage.insert("input_tokens".into(), json!(v));
        }
        if let Some(v) = self.output_tokens {
            usage.insert("output_tokens".into(), json!(v));
        }
        if let Some(v) = self.cache_read_input_tokens {
            usage.insert("cache_read_tokens".into(), json!(v));
        }
        if let Some(v) = self.cache_creation_input_tokens {
            usage.insert("cache_write_tokens".into(), json!(v));
        }
        if let Some(v) = self.output_tokens_details.and_then(|d| d.thinking_tokens) {
            usage.insert("reasoning_tokens".into(), json!(v));
        }
        if usage.is_empty() {
            None
        } else {
            Some(Value::Object(usage))
        }
    }
}

/// Builds a [`StreamEvent`] from its IR JSON shape.
fn ir_event(value: Value) -> StreamEvent {
    serde_json::from_value(value).expect("stream-json maps onto a typed IR event")
}

/// `type` field of a `content_block_start` block, when `event` is one.
#[cfg(test)]
fn start_block_type(event: &StreamEvent) -> Option<String> {
    let StreamEvent::ContentBlockStart(start) = event else {
        return None;
    };
    serde_json::to_value(&start.block)
        .ok()?
        .get("type")?
        .as_str()
        .map(ToOwned::to_owned)
}

/// Tool-call id and name on a `content_block_start`, when `event` is one.
pub(crate) fn start_tool_call(event: &StreamEvent) -> Option<(u32, String, String)> {
    let StreamEvent::ContentBlockStart(start) = event else {
        return None;
    };
    let block = serde_json::to_value(&start.block).ok()?;
    if block.get("type")?.as_str()? != "tool_call" {
        return None;
    }
    Some((
        start.index,
        block.get("id")?.as_str()?.to_owned(),
        block.get("name")?.as_str()?.to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        Decoder, MESSAGE_DELTA, RATE_LIMIT, RESULT_ERROR, Step, TOOL_USE, collected_events,
        decode_script, start_block_type, start_tool_call,
    };
    use hofund_plugin_api::{StopReason, StreamEvent};

    #[test]
    fn message_delta_fixture_becomes_text_events_and_captures_session() {
        let (decoder, steps) = decode_script(MESSAGE_DELTA);
        assert_eq!(decoder.session_id(), Some("sess_text"));
        assert!(matches!(steps[0], Step::Session(ref id) if id == "sess_text"));
        assert!(matches!(steps.last(), Some(Step::Completed)));
        assert!(
            !steps
                .iter()
                .any(|s| matches!(s, Step::ResultError(_) | Step::RateLimit(_)))
        );

        let events = collected_events(&steps);
        assert!(
            events.iter().any(|e| matches!(e, StreamEvent::MessageStart(s) if s.id.as_deref() == Some("msg_text") && s.model.as_deref() == Some("claude-sonnet-4-5")))
        );
        assert!(
            events
                .iter()
                .any(|e| start_block_type(e).as_deref() == Some("text")
                    && matches!(e, StreamEvent::ContentBlockStart(s) if s.index == 0))
        );
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta(d) => Some(d.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello world");
        assert!(
            events.iter().any(|e| matches!(e, StreamEvent::MessageDelta(d) if d.stop.as_ref().is_some_and(|s| s.reason == StopReason::EndTurn)))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, StreamEvent::MessageStop(_)))
        );
    }

    #[test]
    fn tool_use_fixture_starts_a_tool_call_and_joins_argument_fragments() {
        let (decoder, steps) = decode_script(TOOL_USE);
        assert_eq!(decoder.session_id(), Some("sess_tool"));
        let events = collected_events(&steps);
        assert!(
            events
                .iter()
                .any(|e| start_tool_call(e).as_ref() == Some(&(0, "toolu_1".into(), "Bash".into())))
        );
        let args: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolArgumentsDelta(d) => Some(d.partial_json.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(args, "{\"command\":\"ls\"}");
        assert!(
            events.iter().any(|e| matches!(e, StreamEvent::MessageDelta(d) if d.stop.as_ref().is_some_and(|s| s.reason == StopReason::ToolUse)))
        );
        assert!(
            !steps
                .iter()
                .any(|s| matches!(s, Step::ResultError(_) | Step::Completed))
        );
    }

    #[test]
    fn rate_limit_event_does_not_end_the_turn() {
        let (decoder, steps) = decode_script(RATE_LIMIT);
        assert_eq!(decoder.session_id(), Some("sess_limit"));
        let limits: Vec<_> = steps
            .iter()
            .filter_map(|s| match s {
                Step::RateLimit(info) => Some(info),
                _ => None,
            })
            .collect();
        assert_eq!(limits.len(), 1);
        assert_eq!(limits[0].status.as_deref(), Some("allowed"));
        assert_eq!(limits[0].rate_limit_type.as_deref(), Some("five_hour"));
        assert!(!steps.iter().any(|s| matches!(s, Step::ResultError(_))));
        assert!(matches!(steps.last(), Some(Step::Completed)));
        let text: String = collected_events(&steps)
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta(d) => Some(d.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "before after");
    }

    #[test]
    fn result_error_is_terminal_and_keeps_status_and_kind() {
        let (decoder, steps) = decode_script(RESULT_ERROR);
        assert_eq!(decoder.session_id(), Some("sess_err"));
        let err = steps
            .iter()
            .find_map(|s| match s {
                Step::ResultError(err) => Some(err),
                _ => None,
            })
            .expect("result error");
        assert_eq!(err.api_error_status, Some(429));
        assert_eq!(err.kind.as_deref(), Some("rate_limit"));
        assert!(err.message.contains("rate limit"));
        assert!(!steps.iter().any(|s| matches!(s, Step::Completed)));
        assert!(collected_events(&steps).is_empty());
    }

    #[test]
    fn blank_and_unknown_lines_are_skipped() {
        let mut decoder = Decoder::new();
        assert!(decoder.push_line("").is_none());
        assert!(decoder.push_line("not-json").is_none());
        assert!(decoder.push_line("{\"type\":\"user\"}").is_none());
    }
}
