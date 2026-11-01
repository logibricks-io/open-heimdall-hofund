//! Reasoning items in a Responses reply relayed as it came.
//!
//! A client stores the reasoning items it is handed and sends them back,
//! possibly to an upstream that refuses a reasoning item with content parts
//! and cannot open another vendor's seal. A relayed reply's reasoning item
//! that carries its text in content parts is therefore made into the item the
//! gateway makes of any other upstream's reasoning (see
//! `openai_responses::made_reasoning`). Every other byte, event and sequence
//! number stays as the upstream wrote it.
//!
//! reference: internal/gateway/reasoning_content.go:96-206,
//! internal/gateway/gateway.go:5645-5655, 5795-5797, 5831-5833, 5870-5890

use serde_json::Value;

use crate::openai_responses::made_reasoning;
use crate::passthrough::RawTap;

const MARK: &[u8] = b"\"reasoning\"";

/// A stream tap that runs the model-name tap first, then makes over the
/// reasoning items in each finished `data:` line.
pub(crate) struct ReasoningTap {
    inner: Box<dyn RawTap>,
    held: Vec<u8>,
}

impl ReasoningTap {
    pub(crate) fn new(inner: Box<dyn RawTap>) -> Self {
        Self {
            inner,
            held: Vec::new(),
        }
    }

    /// Takes `bytes` and returns the lines finished so far, tidied. An
    /// unfinished line is held for the next call.
    fn write(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.held.extend_from_slice(bytes);
        let Some(end) = self.held.iter().rposition(|&byte| byte == b'\n') else {
            return Vec::new();
        };
        let rest = self.held.split_off(end + 1);
        let done = std::mem::replace(&mut self.held, rest);
        tidy_lines(&done)
    }

    fn flush(&mut self) -> Vec<u8> {
        tidy_lines(&std::mem::take(&mut self.held))
    }
}

impl RawTap for ReasoningTap {
    fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        let out = self.inner.push(chunk);
        self.write(&out)
    }

    fn finish(&mut self) -> Vec<u8> {
        let tail = self.inner.finish();
        let mut out = self.write(&tail);
        out.extend(self.flush());
        out
    }

    fn tokens(&self) -> (Option<i64>, Option<i64>) {
        self.inner.tokens()
    }
}

/// SSE lines with each `data:` line's reasoning items made over, line ends
/// kept.
pub(crate) fn tidy_lines(bytes: &[u8]) -> Vec<u8> {
    if !contains(bytes, MARK) {
        return bytes.to_vec();
    }
    let mut out = Vec::with_capacity(bytes.len());
    for line in bytes.split_inclusive(|&byte| byte == b'\n') {
        let body_len = line
            .iter()
            .rposition(|&byte| byte != b'\n' && byte != b'\r')
            .map_or(0, |index| index + 1);
        let (body, end) = line.split_at(body_len);
        let tidied = body
            .strip_prefix(b"data:")
            .filter(|data| contains(data, MARK))
            .and_then(tidy_body);
        match tidied {
            Some(data) => {
                out.extend_from_slice(b"data: ");
                out.extend(data);
                out.extend_from_slice(end);
            }
            None => out.extend_from_slice(line),
        }
    }
    out
}

/// A whole reply, or one event's data, with its reasoning items made over;
/// `None` when nothing was.
///
/// The items looked at are an event's `item`, a `response`'s `output`, and
/// the `output` of a body that is itself a response object.
pub(crate) fn tidy_body(data: &[u8]) -> Option<Vec<u8>> {
    let mut value: Value = serde_json::from_slice(data).ok()?;
    let event = value.as_object_mut()?;
    let mut changed = event.get_mut("item").is_some_and(make_over);
    let response = if event.get("response").is_some_and(Value::is_object) {
        event.get_mut("response").and_then(Value::as_object_mut)
    } else if event.get("object").and_then(Value::as_str) == Some("response") {
        Some(&mut *event)
    } else {
        None
    };
    if let Some(Value::Array(output)) = response.and_then(|response| response.get_mut("output")) {
        for item in output {
            changed |= make_over(item);
        }
    }
    if !changed {
        return None;
    }
    serde_json::to_vec(&value).ok()
}

/// A whole relayed reply with its reasoning items made over, else as it came.
pub(crate) fn tidy_whole(bytes: Vec<u8>) -> Vec<u8> {
    if !contains(&bytes, MARK) {
        return bytes;
    }
    tidy_body(&bytes).unwrap_or(bytes)
}

fn make_over(item: &mut Value) -> bool {
    let Some(made) = item.as_object().and_then(made_reasoning) else {
        return false;
    };
    *item = Value::Object(made);
    true
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}
