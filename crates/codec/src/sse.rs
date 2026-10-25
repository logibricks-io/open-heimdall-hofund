//! Server-sent event framing.
//!
//! The parser accepts arbitrary chunk boundaries, including a chunk that ends
//! in the middle of a line. `\n` and `\r\n` are both line endings. A blank line
//! ends an event. On [`SseParser::finish`], a trailing line without a newline
//! is still consumed, and a started event is flushed.

use crate::error::CodecError;

/// One SSE event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SseFrame {
    /// The `event:` field, when the sender set one.
    pub event: Option<String>,
    /// Joined `data:` lines, separated by `\n` when there is more than one.
    pub data: String,
}

/// Incremental SSE parser.
#[derive(Debug, Default)]
pub(crate) struct SseParser {
    buffer: Vec<u8>,
    /// First index of `buffer` that has not been searched for a line ending.
    scanned: usize,
    event: Option<String>,
    data: Vec<String>,
    started: bool,
}

impl SseParser {
    /// Feeds `chunk` and returns every event that is now complete.
    pub(crate) fn push(&mut self, chunk: &[u8]) -> Result<Vec<SseFrame>, CodecError> {
        self.buffer.extend_from_slice(chunk);
        self.drain(false)
    }

    /// Flushes a trailing partial line and a started event.
    pub(crate) fn finish(&mut self) -> Result<Vec<SseFrame>, CodecError> {
        self.drain(true)
    }

    fn drain(&mut self, end_of_stream: bool) -> Result<Vec<SseFrame>, CodecError> {
        let mut frames = Vec::new();
        loop {
            let Some(relative) = self.buffer[self.scanned..]
                .iter()
                .position(|byte| *byte == b'\n')
            else {
                self.scanned = self.buffer.len();
                break;
            };
            let newline = self.scanned + relative;
            let mut line: Vec<u8> = self.buffer.drain(..=newline).collect();
            self.scanned = 0;
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if let Some(frame) = self.push_line(&line)? {
                frames.push(frame);
            }
        }
        if end_of_stream && !self.buffer.is_empty() {
            let line = std::mem::take(&mut self.buffer);
            self.scanned = 0;
            if let Some(frame) = self.push_line(&line)? {
                frames.push(frame);
            }
        }
        if end_of_stream && let Some(frame) = self.finish_event() {
            frames.push(frame);
        }
        Ok(frames)
    }

    fn push_line(&mut self, line: &[u8]) -> Result<Option<SseFrame>, CodecError> {
        let line = String::from_utf8(line.to_vec()).map_err(|_| CodecError::Sse {
            message: "frame is not UTF-8".to_owned(),
        })?;
        if line.is_empty() {
            return Ok(self.finish_event());
        }
        if line.starts_with(':') {
            return Ok(None);
        }
        let (field, value) = match line.split_once(':') {
            Some((field, rest)) => (field, rest.strip_prefix(' ').unwrap_or(rest)),
            None => (line.as_str(), ""),
        };
        match field {
            "event" => self.event = Some(value.to_owned()),
            "data" => self.data.push(value.to_owned()),
            _ => {}
        }
        self.started = true;
        Ok(None)
    }

    fn finish_event(&mut self) -> Option<SseFrame> {
        if !self.started {
            return None;
        }
        let frame = SseFrame {
            event: self.event.take(),
            data: self.data.join("\n"),
        };
        self.data.clear();
        self.started = false;
        Some(frame)
    }
}

pub(crate) fn data_frame(json: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(json.len() + 8);
    out.extend_from_slice(b"data: ");
    out.extend_from_slice(json);
    out.extend_from_slice(b"\n\n");
    out
}

pub(crate) fn write_event(out: &mut Vec<u8>, event: Option<&str>, data: &str) {
    if let Some(event) = event {
        out.extend_from_slice(b"event: ");
        out.extend_from_slice(event.as_bytes());
        out.push(b'\n');
    }
    out.extend_from_slice(b"data: ");
    out.extend_from_slice(data.as_bytes());
    out.extend_from_slice(b"\n\n");
}

#[cfg(test)]
mod tests {
    use super::SseParser;

    #[test]
    fn splits_inside_a_line_and_a_crlf_blank_line() {
        let mut parser = SseParser::default();
        let first = parser.push(b"event: ping\r\ndata: {\"ty").unwrap();
        assert!(first.is_empty());
        let second = parser.push(b"pe\":\"ping\"}\r\n\r\n").unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].event.as_deref(), Some("ping"));
        assert_eq!(second[0].data, "{\"type\":\"ping\"}");
        assert!(parser.finish().unwrap().is_empty());
    }
}
