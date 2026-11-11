use serde::{Deserialize, Serialize};

use crate::{API_VERSION, ControlError, ControlErrorKind};

/// Largest log-tail read accepted by the control API.
pub const MAX_LOG_TAIL_BYTES: usize = 65_536;

/// Parsed log-tail query parameters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct LogTailQuery {
    /// Byte offset to resume from.
    pub after: u64,
    /// Maximum bytes to read.
    pub max_bytes: usize,
}

impl Default for LogTailQuery {
    fn default() -> Self {
        Self {
            after: 0,
            max_bytes: MAX_LOG_TAIL_BYTES,
        }
    }
}

impl LogTailQuery {
    /// Parses `after` and `max_bytes`, rejecting unknown or duplicate fields.
    pub fn parse(query: Option<&str>) -> Result<Self, ControlError> {
        let mut parsed = Self::default();
        let mut seen_after = false;
        let mut seen_max = false;
        for pair in query
            .unwrap_or_default()
            .split('&')
            .filter(|pair| !pair.is_empty())
        {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            match name {
                "after" if !seen_after => {
                    parsed.after = value.parse().map_err(|_| bad_query())?;
                    seen_after = true;
                }
                "max_bytes" if !seen_max => {
                    parsed.max_bytes = value.parse().map_err(|_| bad_query())?;
                    seen_max = true;
                }
                _ => return Err(bad_query()),
            }
        }
        if parsed.max_bytes == 0 || parsed.max_bytes > MAX_LOG_TAIL_BYTES {
            return Err(bad_query());
        }
        Ok(parsed)
    }

    /// Encodes this query for a client request.
    #[must_use]
    pub fn encode(self) -> String {
        format!("after={}&max_bytes={}", self.after, self.max_bytes)
    }
}

fn bad_query() -> ControlError {
    ControlError::new(
        ControlErrorKind::BadRequest,
        "logs accepts one after offset and max_bytes from 1 to 65536",
        "validate",
    )
}

/// One bounded daemon log-tail response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LogTailResponse {
    /// Body wire version.
    pub version: u32,
    /// Informational daemon log path on the daemon host.
    pub path: String,
    /// Effective byte offset of this read.
    pub offset: u64,
    /// Byte offset for the next request.
    pub next_offset: u64,
    /// UTF-8 log lines, lossily decoded if a write split a codepoint.
    pub lines: Vec<String>,
}

impl Default for LogTailResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            path: String::new(),
            offset: 0,
            next_offset: 0,
            lines: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ControlErrorKind, LogTailQuery, MAX_LOG_TAIL_BYTES};

    #[test]
    fn log_query_defaults_bounds_and_round_trip() {
        let default = LogTailQuery::parse(None).expect("default");
        assert_eq!(default.after, 0);
        assert_eq!(default.max_bytes, MAX_LOG_TAIL_BYTES);
        let parsed = LogTailQuery::parse(Some("after=42&max_bytes=1024")).expect("query");
        assert_eq!(parsed.encode(), "after=42&max_bytes=1024");
        for bad in [
            "after=nope",
            "after=1&after=2",
            "max_bytes=1&max_bytes=2",
            "max_bytes=0",
            "max_bytes=65537",
            "follow=true",
        ] {
            let error = LogTailQuery::parse(Some(bad)).expect_err("invalid query");
            assert_eq!(error.kind, ControlErrorKind::BadRequest, "{bad}");
        }
    }
}
