//! Stream timing for usage rows: first content, text, and content flow.

use std::time::Instant;

use hofund_ir::{BlockStart, StreamEvent};
use hofund_usage::FlowTracker;
use serde::Deserialize;
use serde_json::Value;

/// Tracks a streamed reply for TTFT and flow_ms.
#[derive(Debug)]
pub(crate) struct StreamTiming {
    start: Instant,
    first_content_ms: Option<i64>,
    first_text_ms: Option<i64>,
    first_reasoning_ms: Option<i64>,
    flow: FlowTracker,
    /// False when the body is a single JSON value, not a stream.
    timed: bool,
    off: bool,
    began: bool,
    pending: Vec<u8>,
    read: usize,
    flow_discarded: bool,
    timing_hidden: bool,
}

const FIRST_MOST: usize = 8 << 20;

impl StreamTiming {
    #[must_use]
    pub(crate) fn new(start: Instant) -> Self {
        Self {
            start,
            first_content_ms: None,
            first_text_ms: None,
            first_reasoning_ms: None,
            flow: FlowTracker::default(),
            timed: true,
            off: false,
            began: false,
            pending: Vec::new(),
            read: 0,
            flow_discarded: false,
            timing_hidden: false,
        }
    }

    #[must_use]
    pub(crate) fn timing_hidden(&self) -> bool {
        self.timing_hidden
    }

    pub(crate) fn note_stream_event(&mut self, event: &StreamEvent, encoded_bytes: usize) {
        if !self.timed || self.off {
            return;
        }
        let elapsed = self.elapsed_ms();
        let (content, text, reasoning, hidden) = match event {
            StreamEvent::TextDelta(_) => (true, true, false, false),
            StreamEvent::ThinkingDelta(_) => (true, false, true, false),
            StreamEvent::ToolArgumentsDelta(_) => (true, false, false, false),
            StreamEvent::ContentBlockStart(start) => match &start.block {
                BlockStart::ToolCall { .. } => (true, false, false, false),
                BlockStart::Unknown { kind } if kind == "server_tool_use" => {
                    (true, false, false, false)
                }
                BlockStart::Thinking { redacted: true, .. } => {
                    self.timing_hidden = true;
                    (false, false, false, true)
                }
                BlockStart::Text | BlockStart::Thinking { .. } | BlockStart::Unknown { .. } => {
                    (false, false, false, false)
                }
            },
            _ => (false, false, false, false),
        };
        self.timing_hidden = self.timing_hidden || hidden;
        if !content {
            return;
        }
        self.note_content(elapsed, text, reasoning, encoded_bytes.max(1));
    }

    pub(crate) fn ingest_raw(&mut self, chunk: &[u8]) {
        if !self.timed || self.off || chunk.is_empty() {
            return;
        }
        if !self.began {
            let trimmed = trim_left(chunk);
            if trimmed.is_empty() {
                return;
            }
            if trimmed[0] == b'{' || trimmed[0] == b'[' {
                self.off = true;
                self.timed = false;
                return;
            }
            self.began = true;
        }
        self.read += chunk.len();
        if self.read > FIRST_MOST {
            self.off = true;
            self.flow_discarded = true;
            self.pending.clear();
            return;
        }
        self.pending.extend_from_slice(chunk);
        let elapsed = self.elapsed_ms();
        while !self.off {
            let Some(end) = event_end(&self.pending) else {
                break;
            };
            let event = self.pending[..end].to_vec();
            self.pending.drain(..end);
            if self.first_text_ms.is_some() && flow_content(&event) {
                self.flow.mark(elapsed, event.len() as i64);
                continue;
            }
            let (content, text, reasoning, hidden) = timing_kind(&event);
            self.timing_hidden = self.timing_hidden || hidden;
            if !content && !text {
                continue;
            }
            self.note_content(elapsed, text, reasoning, event.len().max(1));
        }
    }

    fn note_content(&mut self, elapsed: i64, text: bool, reasoning: bool, nbytes: usize) {
        if self.first_content_ms.is_none() {
            self.first_content_ms = Some(elapsed);
        }
        if reasoning && self.first_reasoning_ms.is_none() {
            self.first_reasoning_ms = Some(elapsed);
        }
        if text && self.first_text_ms.is_none() {
            self.first_text_ms = Some(elapsed);
        }
        if !self.flow_discarded && self.first_content_ms.is_some() {
            self.flow.mark(elapsed, nbytes as i64);
        }
    }

    fn elapsed_ms(&self) -> i64 {
        i64::try_from(self.start.elapsed().as_millis())
            .unwrap_or(i64::MAX)
            .max(1)
    }

    #[allow(clippy::type_complexity)]
    pub(crate) fn finish_fields(
        &self,
        reasoning_tokens: i64,
    ) -> Option<(Option<i64>, Option<i64>, Option<i64>, Option<i64>)> {
        if !self.timed || (self.off && self.first_content_ms.is_none()) {
            return None;
        }
        let flow = if self.flow_discarded {
            None
        } else {
            let flow = self.flow.flow_for(
                reasoning_tokens,
                self.first_text_ms.unwrap_or(0),
                self.first_content_ms.unwrap_or(0),
            );
            (flow > 0).then_some(flow)
        };
        Some((
            self.first_content_ms,
            self.first_text_ms,
            self.first_reasoning_ms,
            flow,
        ))
    }
}

fn trim_left(bytes: &[u8]) -> &[u8] {
    let mut index = 0;
    while index < bytes.len() && matches!(bytes[index], b' ' | b'\t' | b'\r' | b'\n') {
        index += 1;
    }
    &bytes[index..]
}

fn has(event: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return false;
    }
    event.windows(needle.len()).any(|window| window == needle)
}

fn event_end(pending: &[u8]) -> Option<usize> {
    for index in 0..pending.len().saturating_sub(1) {
        if pending[index] != b'\n' {
            continue;
        }
        if pending.get(index + 1) == Some(&b'\n') {
            return Some(index + 2);
        }
        if pending.get(index + 1) == Some(&b'\r') && pending.get(index + 2) == Some(&b'\n') {
            return Some(index + 3);
        }
    }
    None
}

fn flow_content(event: &[u8]) -> bool {
    let (_, data) = parse_sse_event(event);
    if data.is_empty() {
        return false;
    }
    if has(&data, br#""type":"response.completed""#)
        || has(&data, br#""type": "response.completed""#)
    {
        return false;
    }
    has(&data, b"delta") || has(&data, br#""parts""#)
}

fn parse_sse_event(event: &[u8]) -> (String, Vec<u8>) {
    let mut name = String::new();
    let mut data = Vec::new();
    for line in event.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if let Some(rest) = line.strip_prefix(b"event:") {
            name = String::from_utf8_lossy(rest.trim_ascii()).into_owned();
        } else if let Some(rest) = line.strip_prefix(b"data:") {
            // The reference joins data lines with nothing between them.
            data.extend_from_slice(rest.trim_ascii());
        }
    }
    (name, data)
}

fn timing_gate(data: &[u8]) -> bool {
    if data.is_empty() {
        return false;
    }
    has(data, b"delta")
        || has(data, br#""parts""#)
        || has(data, b"content_block_start")
        || has(data, b"output_item.added")
        || has(data, b"encrypted_content")
}

fn filled(raw: &Value) -> bool {
    match raw {
        Value::Null => false,
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
        Value::Number(_) | Value::Bool(_) => true,
    }
}

/// A JSON `null` decodes as the field's zero value, as the reference's
/// typed decode leaves a field it reads `null` for untouched.
fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Deserialize)]
struct TimingEnvelope {
    #[serde(default, deserialize_with = "null_as_default")]
    r#type: String,
    #[serde(default, deserialize_with = "null_as_default")]
    delta: Value,
    #[serde(default, deserialize_with = "null_as_default")]
    content_block: Option<TypedBlock>,
    #[serde(default, deserialize_with = "null_as_default")]
    item: Option<TimingItem>,
    #[serde(default, deserialize_with = "null_as_default")]
    choices: Vec<ChatChoice>,
    #[serde(default, deserialize_with = "null_as_default")]
    candidates: Vec<GeminiCandidate>,
}

#[derive(Debug, Deserialize)]
struct TypedBlock {
    #[serde(default, deserialize_with = "null_as_default")]
    r#type: String,
}

#[derive(Debug, Deserialize)]
struct TimingItem {
    #[serde(default, deserialize_with = "null_as_default")]
    r#type: String,
    #[serde(default, deserialize_with = "null_as_default")]
    encrypted_content: String,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    #[serde(default, deserialize_with = "null_as_default")]
    delta: ChatDelta,
}

#[derive(Debug, Default, Deserialize)]
struct ChatDelta {
    #[serde(default, deserialize_with = "null_as_default")]
    content: Value,
    #[serde(default, deserialize_with = "null_as_default")]
    refusal: Value,
    #[serde(default, deserialize_with = "null_as_default")]
    reasoning_content: Value,
    #[serde(default, deserialize_with = "null_as_default")]
    reasoning: Value,
    #[serde(default, deserialize_with = "null_as_default")]
    tool_calls: Value,
    #[serde(default, deserialize_with = "null_as_default")]
    function_call: Value,
}

#[derive(Debug, Deserialize)]
struct GeminiCandidate {
    #[serde(default, deserialize_with = "null_as_default")]
    content: GeminiContent,
}

#[derive(Debug, Default, Deserialize)]
struct GeminiContent {
    #[serde(default, deserialize_with = "null_as_default")]
    parts: Vec<GeminiPart>,
}

#[derive(Debug, Default, Deserialize)]
struct GeminiPart {
    #[serde(default, deserialize_with = "null_as_default")]
    text: String,
    #[serde(default, deserialize_with = "null_as_default")]
    thought: bool,
    #[serde(default, rename = "functionCall", deserialize_with = "null_as_default")]
    function_call: Value,
}

#[derive(Debug, Default, Deserialize)]
struct AnthropicDelta {
    #[serde(default, deserialize_with = "null_as_default")]
    text: String,
    #[serde(default, deserialize_with = "null_as_default")]
    thinking: String,
    #[serde(default, deserialize_with = "null_as_default")]
    partial_json: String,
}

fn timing_kind(event: &[u8]) -> (bool, bool, bool, bool) {
    let (name, data) = parse_sse_event(event);
    if !timing_gate(&data) {
        return (false, false, false, false);
    }
    let Ok(v) = serde_json::from_slice::<TimingEnvelope>(&data) else {
        return (false, false, false, false);
    };
    let mut hidden = false;
    if let Some(item) = &v.item
        && !item.encrypted_content.is_empty()
    {
        hidden = true;
    }
    if let Some(block) = &v.content_block
        && block.r#type == "redacted_thinking"
    {
        hidden = true;
    }
    let mut typ = v.r#type;
    if typ.is_empty() {
        typ = name;
    }
    if typ == "response.completed" {
        return (false, false, false, hidden);
    }
    match typ.as_str() {
        "content_block_delta" => {
            let delta =
                serde_json::from_value::<AnthropicDelta>(v.delta.clone()).unwrap_or_default();
            if !delta.text.is_empty() {
                return (true, true, false, hidden);
            }
            if !delta.thinking.is_empty() {
                return (true, false, true, false);
            }
            if !delta.partial_json.is_empty() {
                return (true, false, false, hidden);
            }
            (false, false, false, hidden)
        }
        "content_block_start" => {
            let tool = v.content_block.as_ref().is_some_and(|block| {
                block.r#type == "tool_use" || block.r#type == "server_tool_use"
            });
            (tool, false, false, hidden)
        }
        "response.output_text.delta" | "response.refusal.delta" => {
            let delta = filled(&v.delta);
            (delta, delta, false, false)
        }
        "response.reasoning_summary_text.delta"
        | "response.reasoning_text.delta"
        | "response.function_call_arguments.delta"
        | "response.custom_tool_call_input.delta" => {
            let delta = filled(&v.delta);
            let reasoning = matches!(
                typ.as_str(),
                "response.reasoning_text.delta" | "response.reasoning_summary_text.delta"
            ) && delta;
            (delta, false, reasoning, false)
        }
        "response.output_item.added" => {
            let tool = v.item.as_ref().is_some_and(|item| {
                item.r#type == "function_call" || item.r#type == "custom_tool_call"
            });
            (tool, false, false, hidden)
        }
        _ => {
            let mut content = false;
            let text = false;
            let mut reasoning = false;
            for choice in &v.choices {
                let delta = &choice.delta;
                reasoning =
                    reasoning || filled(&delta.reasoning_content) || filled(&delta.reasoning);
                if filled(&delta.content) || filled(&delta.refusal) {
                    return (true, true, reasoning, hidden);
                }
                if filled(&delta.reasoning_content)
                    || filled(&delta.reasoning)
                    || filled(&delta.tool_calls)
                    || filled(&delta.function_call)
                {
                    content = true;
                }
            }
            for candidate in &v.candidates {
                for part in &candidate.content.parts {
                    if !part.text.is_empty() && !part.thought {
                        return (true, true, reasoning, hidden);
                    }
                    if !part.text.is_empty() || filled(&part.function_call) {
                        content = true;
                        if part.thought && !part.text.is_empty() {
                            reasoning = true;
                        }
                    }
                }
            }
            (content, text, reasoning, hidden)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{StreamTiming, event_end, flow_content, has, timing_kind};
    use hofund_ir::{
        BlockStart, ContentBlockStart, Extensions, StreamEvent, TextDelta, ToolArgumentsDelta,
    };
    use std::time::Instant;

    fn frame(body: &str) -> Vec<u8> {
        format!("{body}\n\n").into_bytes()
    }

    fn first_kind(body: &str) -> (bool, bool) {
        let (content, text, _, _) = timing_kind(&frame(body));
        (content, text)
    }

    #[test]
    fn r10_has_uses_needle_length_for_windows() {
        assert!(!has(b"abcde", b"abcdef"));
        assert!(has(b"content_block_start", b"content_block_start"));
        assert!(!has(b"content_block_star", b"content_block_start"));
    }

    #[test]
    fn r10_event_end_whole_sse_frame() {
        let complete = b"data: {}\n\n";
        assert_eq!(event_end(complete), Some(complete.len()));
        let partial = b"data: {}\n";
        assert!(event_end(partial).is_none());
        let crlf = b"data: y\r\n\r\n";
        assert_eq!(event_end(crlf), Some(crlf.len()));
    }

    #[test]
    fn r10_flow_content_delta_and_parts() {
        assert!(flow_content(b"data: {\"delta\":{\"text\":\"a\"}}\n\n"));
        assert!(flow_content(
            b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"a\"}]}}]}\n\n"
        ));
        assert!(!flow_content(b"data: {\"type\":\"message_start\"}\n\n"));
        assert!(!flow_content(
            br#"event: response.completed
data: {"type":"response.completed","response":{"output":[{"type":"output_text","text":"hi"}]}}

"#
        ));
    }

    #[test]
    fn r10_timing_kind_null_fields_read_as_empty() {
        let added = br#"data: {"type":"response.output_item.added","item":{"type":"function_call","encrypted_content":null}}"#;
        assert_eq!(timing_kind(added), (true, false, false, false));
        let gemini =
            br#"data: {"candidates":[{"content":{"parts":[{"text":"hi","thought":null}]}}]}"#;
        assert_eq!(timing_kind(gemini), (true, true, false, false));
        let chat = br#"data: {"choices":[{"delta":{"content":"hi","refusal":null}}]}"#;
        assert_eq!(timing_kind(chat), (true, true, false, false));
    }

    #[test]
    fn r10_timing_kind_joins_data_lines_like_the_reference() {
        let split = b"data: {\"type\":\"response.output_text.delta\",\"del\n\
data: ta\":\"hi\"}";
        assert_eq!(timing_kind(split), (true, true, false, false));
    }

    #[test]
    fn r10_timing_kind_reference_table() {
        for (body, want_content, want_text) in [
            (
                r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t","name":"ls","input":{}}}"#,
                true,
                false,
            ),
            (
                r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                false,
                false,
            ),
            (
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"a"}}"#,
                true,
                false,
            ),
            (
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"x"}}"#,
                false,
                false,
            ),
            (
                r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
                false,
                false,
            ),
            (
                r#"data: {"choices":[{"delta":{"role":"assistant","content":null}}]}"#,
                false,
                false,
            ),
            (
                r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"ls"}}]}}]}"#,
                true,
                false,
            ),
            (
                r#"data: {"choices":[{"delta":{"reasoning":"hm"}}]}"#,
                true,
                false,
            ),
            (
                r#"data: {"choices":[{"delta":{"content":"hi"}}]}"#,
                true,
                true,
            ),
            (
                "event: response.function_call_arguments.delta\ndata: {\"type\":\"response.function_call_arguments.delta\",\"delta\":\"{\"}",
                true,
                false,
            ),
            (
                r#"data: {"type":"response.output_item.added","item":{"type":"function_call","name":"ls"}}"#,
                true,
                false,
            ),
            (
                r#"data: {"type":"response.output_item.added","item":{"type":"message"}}"#,
                false,
                false,
            ),
            (
                r#"data: {"type":"response.output_text.delta","delta":""}"#,
                false,
                false,
            ),
            (
                r#"data: {"candidates":[{"content":{"parts":[{"text":"hm","thought":true}]}}]}"#,
                true,
                false,
            ),
            (
                r#"data: {"candidates":[{"content":{"parts":[{"functionCall":{"name":"ls"}}]}}]}"#,
                true,
                false,
            ),
            (
                r#"data: {"candidates":[{"content":{"parts":[{"text":"hi"}]}}]}"#,
                true,
                true,
            ),
        ] {
            let (content, text) = first_kind(body);
            assert_eq!((content, text), (want_content, want_text), "body={body:?}");
        }
    }

    #[test]
    fn r10_timing_kind_output_text_delta_is_visible_text() {
        let ev = frame(
            r#"event: response.output_text.delta
data: {"type":"response.output_text.delta","delta":"hi"}"#,
        );
        let (content, text, reasoning, hidden) = timing_kind(&ev);
        assert!(content && text && !reasoning && !hidden);
    }

    #[test]
    fn r10_timing_kind_function_call_output_not_content() {
        let ev = frame(
            r#"data: {"type":"response.output_item.added","item":{"type":"function_call_output","output":"x"}}"#,
        );
        let (content, _, _, _) = timing_kind(&ev);
        assert!(!content);
    }

    #[test]
    fn r10_timing_kind_server_tool_use_block_start_is_content() {
        let ev = frame(
            r#"data: {"type":"content_block_start","content_block":{"type":"server_tool_use","id":"t","name":"n"}}"#,
        );
        let (content, text, _, _) = timing_kind(&ev);
        assert!(content);
        assert!(!text);
    }

    #[test]
    fn r10_timing_kind_completed_not_first_content_even_with_text() {
        let ev = frame(
            r#"event: response.completed
data: {"type":"response.completed","response":{"output":[{"type":"output_text","text":"hi"}],"usage":{"output_tokens":1}},"item":{"encrypted_content":"sealed"}}"#,
        );
        let (content, text, _, hidden) = timing_kind(&ev);
        assert!(!content && !text);
        assert!(hidden);
    }

    #[test]
    fn r10_timing_kind_encrypted_reasoning_item_hidden_not_content() {
        let item = frame(
            r#"event: response.output_item.added
data: {"type":"response.output_item.added","item":{"type":"reasoning","encrypted_content":"sealed"}}"#,
        );
        let (content, _, _, hidden) = timing_kind(&item);
        assert!(!content);
        assert!(hidden);
    }

    #[test]
    fn r10_stream_timing_raw_tool_start_records_ttft() {
        let mut timing = StreamTiming::new(Instant::now());
        timing.ingest_raw(
            br#"event: content_block_start
data: {"type":"content_block_start","content_block":{"type":"tool_use","id":"t","name":"n"}}

"#,
        );
        assert!(timing.finish_fields(0).expect("fields").0.is_some());
    }

    #[test]
    fn r10_stream_timing_server_tool_use_on_decode_path() {
        let mut timing = StreamTiming::new(Instant::now());
        timing.note_stream_event(
            &StreamEvent::ContentBlockStart(ContentBlockStart {
                index: 0,
                block: BlockStart::Unknown {
                    kind: "server_tool_use".to_owned(),
                },
                extensions: Extensions::default(),
            }),
            64,
        );
        assert!(timing.finish_fields(0).expect("fields").0.is_some());
    }

    #[test]
    fn r10_stream_timing_single_text_chunk_records_flow() {
        let mut timing = StreamTiming::new(Instant::now());
        timing.note_stream_event(
            &StreamEvent::TextDelta(TextDelta {
                index: 0,
                text: "ok".to_owned(),
                extensions: Extensions::default(),
            }),
            64,
        );
        let fields = timing.finish_fields(0).expect("timing");
        assert!(fields.3.is_some_and(|flow| flow > 0));
    }

    #[test]
    fn r10_stream_timing_tool_burst_records_flow() {
        let mut timing = StreamTiming::new(Instant::now());
        timing.note_stream_event(
            &StreamEvent::ContentBlockStart(ContentBlockStart {
                index: 0,
                block: BlockStart::ToolCall {
                    id: "call".to_owned(),
                    name: "demo".to_owned(),
                },
                extensions: Extensions::default(),
            }),
            128,
        );
        timing.note_stream_event(
            &StreamEvent::ToolArgumentsDelta(ToolArgumentsDelta {
                index: 0,
                partial_json: r#"{"x":1}"#.to_owned(),
                extensions: Extensions::default(),
            }),
            32,
        );
        let fields = timing.finish_fields(0).expect("timing");
        assert!(fields.0.is_some(), "ttft from first tool content");
        assert!(fields.3.is_some_and(|flow| flow > 0));
    }

    #[test]
    fn r10_stream_timing_redacted_thinking_start_is_hidden() {
        let mut timing = StreamTiming::new(Instant::now());
        timing.note_stream_event(
            &StreamEvent::ContentBlockStart(ContentBlockStart {
                index: 0,
                block: BlockStart::Thinking {
                    redacted: true,
                    data: None,
                },
                extensions: Extensions::default(),
            }),
            64,
        );
        assert!(timing.timing_hidden());
        assert!(timing.finish_fields(0).expect("fields").0.is_none());
    }
}
