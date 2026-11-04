//! Namespaced function calls in relayed Responses replies.
//!
//! Codex treats a namespaced call as carrying plain arguments only when
//! `encrypted_function_args` is present and empty. Third-party Responses
//! relays that omit the field leave Codex to treat the call as sealed.

use serde_json::{Value, json};

const FUNCTION_CALL: &str = "function_call";
const ENCRYPTED: &str = "encrypted_function_args";

/// Marks a namespaced `function_call` as unsealed when the upstream omitted
/// the field. Leaves existing seals and non-namespaced calls unchanged.
pub fn mark_namespaced_call(item: &mut Value) -> bool {
    let Some(object) = item.as_object_mut() else {
        return false;
    };
    if object.get("type").and_then(Value::as_str) != Some(FUNCTION_CALL) {
        return false;
    }
    if object
        .get("namespace")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .is_none()
    {
        return false;
    }
    if object.contains_key(ENCRYPTED) {
        return false;
    }
    object.insert(ENCRYPTED.to_owned(), json!([]));
    true
}

/// Tidies one SSE `data:` JSON value for relay; `None` when unchanged.
pub fn tidy_event(data: &[u8]) -> Option<Vec<u8>> {
    let mut value: Value = serde_json::from_slice(data).ok()?;
    let event = value.as_object_mut()?;
    let mut changed = event.get_mut("item").is_some_and(mark_namespaced_call);
    let response = if event.get("response").is_some_and(Value::is_object) {
        event.get_mut("response").and_then(Value::as_object_mut)
    } else if event.get("object").and_then(Value::as_str) == Some("response") {
        Some(&mut *event)
    } else {
        None
    };
    if let Some(Value::Array(output)) = response.and_then(|response| response.get_mut("output")) {
        for item in output {
            changed |= mark_namespaced_call(item);
        }
    }
    if !changed {
        return None;
    }
    serde_json::to_vec(&value).ok()
}

/// A whole relayed reply with namespaced calls marked unsealed.
#[must_use]
pub fn tidy_whole(bytes: Vec<u8>) -> Vec<u8> {
    if !bytes
        .windows(FUNCTION_CALL.len())
        .any(|w| w == FUNCTION_CALL.as_bytes())
    {
        return bytes;
    }
    tidy_event(&bytes).unwrap_or(bytes)
}

/// SSE lines with each finished `data:` payload marking namespaced calls.
pub fn tidy_lines(bytes: &[u8]) -> Vec<u8> {
    if !bytes
        .windows(FUNCTION_CALL.len())
        .any(|w| w == FUNCTION_CALL.as_bytes())
    {
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
            .and_then(|data| tidy_event(data.strip_prefix(b" ").unwrap_or(data)));
        match tidied {
            Some(data) if data.is_empty() => {}
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

/// Stream tap that marks namespaced calls unsealed on each finished SSE line.
pub struct NamespaceTap {
    inner: Box<dyn crate::RawTap>,
    held: Vec<u8>,
}

impl NamespaceTap {
    /// Wraps `inner` and marks namespaced calls on each finished SSE line.
    pub fn new(inner: Box<dyn crate::RawTap>) -> Self {
        Self {
            inner,
            held: Vec::new(),
        }
    }

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

impl crate::RawTap for NamespaceTap {
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
