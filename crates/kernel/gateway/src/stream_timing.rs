//! Stream timing for usage rows: first content, text, and content flow.

use std::time::Instant;

use hofund_ir::{BlockStart, StreamEvent};
use hofund_usage::FlowTracker;

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
        }
    }

    pub(crate) fn note_stream_event(&mut self, event: &StreamEvent, encoded_bytes: usize) {
        if !self.timed || self.off {
            return;
        }
        let elapsed = self.elapsed_ms();
        let (content, text, reasoning) = match event {
            StreamEvent::TextDelta(_) => (true, true, false),
            StreamEvent::ThinkingDelta(_) => (true, false, true),
            StreamEvent::ToolArgumentsDelta(_) => (true, false, false),
            StreamEvent::ContentBlockStart(start) => match &start.block {
                BlockStart::ToolCall { .. } | BlockStart::Text => (true, false, false),
                BlockStart::Thinking { .. } => (true, false, true),
                BlockStart::Unknown { .. } => (false, false, false),
            },
            _ => (false, false, false),
        };
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
            let (content, text, reasoning) = timing_kind(&event);
            if !content {
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

fn event_end(pending: &[u8]) -> Option<usize> {
    pending
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|index| index + 2)
}

fn flow_content(event: &[u8]) -> bool {
    event.windows(5).any(|w| w == b"delta") || event.windows(6).any(|w| w == br#""parts""#)
}

fn timing_kind(event: &[u8]) -> (bool, bool, bool) {
    if event.windows(18).any(|w| w == b"content_block_start") {
        let tool = event.windows(9).any(|w| w == b"tool_use")
            || event.windows(15).any(|w| w == b"server_tool_use");
        return (
            true,
            false,
            !tool && event.windows(8).any(|w| w == b"thinking"),
        );
    }
    if !flow_content(event) && !event.windows(16).any(|w| w == b"output_item.added") {
        return (false, false, false);
    }
    let text = event.windows(6).any(|w| w == b"\"text\"");
    let reasoning =
        event.windows(8).any(|w| w == b"thinking") || event.windows(9).any(|w| w == b"reasoning");
    (true, text, reasoning)
}

#[cfg(test)]
mod tests {
    use super::StreamTiming;
    use hofund_ir::{
        BlockStart, ContentBlockStart, Extensions, StreamEvent, TextDelta, ToolArgumentsDelta,
    };
    use std::time::Instant;

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
}
