//! Bounded JSON values accepted from session files.

use serde_json::Value;
use thiserror::Error;

/// Maximum object or array nesting accepted in one session line.
pub const MAX_JSON_DEPTH: usize = 32;

/// A line is not JSON within the session input limits.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum JsonError {
    /// The line is malformed JSON.
    #[error("invalid session JSON")]
    Syntax,
    /// The value nests deeper than [`MAX_JSON_DEPTH`].
    #[error("session JSON exceeds depth 32")]
    Depth,
}

/// Parses one complete JSON line and enforces the depth bound.
///
/// # Errors
///
/// Returns [`JsonError`] for malformed JSON or nesting deeper than 32.
pub fn parse_line(line: &[u8]) -> Result<Value, JsonError> {
    let value = serde_json::from_slice(line).map_err(|_| JsonError::Syntax)?;
    if depth(&value) > MAX_JSON_DEPTH {
        return Err(JsonError::Depth);
    }
    Ok(value)
}

fn depth(value: &Value) -> usize {
    match value {
        Value::Array(items) => 1 + items.iter().map(depth).max().unwrap_or(0),
        Value::Object(items) => 1 + items.values().map(depth).max().unwrap_or(0),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_depth_refused() {
        let accepted = format!("{}0{}", "[".repeat(32), "]".repeat(32));
        assert!(parse_line(accepted.as_bytes()).is_ok());
        let refused = format!("{}0{}", "[".repeat(33), "]".repeat(33));
        assert_eq!(parse_line(refused.as_bytes()), Err(JsonError::Depth));
    }

    #[test]
    fn malformed_and_non_utf8_lines_are_refused() {
        assert_eq!(parse_line(b"{"), Err(JsonError::Syntax));
        assert_eq!(parse_line(&[0xff]), Err(JsonError::Syntax));
    }
}
