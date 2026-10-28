use serde::{Deserialize, Serialize};

use crate::{API_VERSION, FieldIssue, MAX_ERROR_MESSAGE_BYTES, MAX_REQUEST_ID_LEN};

/// An error response body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ErrorEnvelope {
    /// Body wire version.
    pub version: u32,
    /// Failure details.
    pub error: ControlError,
}

impl Default for ErrorEnvelope {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            error: ControlError::default(),
        }
    }
}

/// A sanitized, transport-independent control failure.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ControlError {
    /// Stable failure category.
    pub kind: ControlErrorKind,
    /// Bounded human-readable message.
    pub message: String,
    /// Bounded request correlation id, or a fixed placeholder when invalid.
    pub request_id: String,
    /// Optional retry hint in unsigned milliseconds.
    pub retry_after_ms: Option<u64>,
    /// Field-level validation or application issues.
    pub details: Vec<FieldIssue>,
}

impl ControlError {
    /// Constructs a bounded error. The message is truncated on a character
    /// boundary. An empty, over-long, or non-ASCII-graphic request id becomes
    /// `invalid-request-id` rather than being reflected onto the wire.
    #[must_use]
    pub fn new(
        kind: ControlErrorKind,
        message: impl Into<String>,
        request_id: impl Into<String>,
    ) -> Self {
        let mut message = message.into();
        if message.len() > MAX_ERROR_MESSAGE_BYTES {
            let mut end = MAX_ERROR_MESSAGE_BYTES;
            while !message.is_char_boundary(end) {
                end -= 1;
            }
            message.truncate(end);
        }
        let request_id = request_id.into();
        let request_id = if valid_request_id(&request_id) {
            request_id
        } else {
            "invalid-request-id".to_owned()
        };
        Self {
            kind,
            message,
            request_id,
            retry_after_ms: None,
            details: Vec::new(),
        }
    }
}

impl Default for ControlError {
    fn default() -> Self {
        Self {
            kind: ControlErrorKind::Internal,
            message: String::new(),
            request_id: String::new(),
            retry_after_ms: None,
            details: Vec::new(),
        }
    }
}

fn valid_request_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_REQUEST_ID_LEN
        && value.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
}

/// Stable control failure categories and their default HTTP statuses.
///
/// `BadRequest` covers malformed JSON, fields, shapes, and body limits.
/// `UnsupportedVersion` covers path or body version mismatches.
/// `Unprocessable` covers semantic request validation other than configuration
/// validation, which remains `InvalidConfig`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(rename_all = "snake_case")]
pub enum ControlErrorKind {
    /// Authentication failed.
    Unauthorized,
    /// The peer or request origin is forbidden.
    Forbidden,
    /// The requested id does not exist.
    NotFound,
    /// A revision or optimistic-concurrency check failed.
    Conflict,
    /// The JSON body or request shape is malformed.
    BadRequest,
    /// The path or body version is unsupported.
    UnsupportedVersion,
    /// Configuration validation failed.
    InvalidConfig,
    /// Semantic non-configuration request validation failed.
    Unprocessable,
    /// The daemon is stopping or unavailable.
    Unavailable,
    /// An upstream operation failed.
    Upstream,
    /// An operation timed out.
    Timeout,
    /// An internal operation failed.
    #[default]
    Internal,
    /// A newer peer supplied an unknown category.
    #[serde(other)]
    Unknown,
}

impl ControlErrorKind {
    /// Default HTTP status for this category. An upstream 429 remains
    /// `Upstream`; the daemon's mapping selects status 429 for that case.
    #[must_use]
    pub const fn http_status(self) -> u16 {
        match self {
            Self::Unauthorized => 401,
            Self::Forbidden => 403,
            Self::NotFound => 404,
            Self::Conflict => 409,
            Self::BadRequest | Self::UnsupportedVersion => 400,
            Self::InvalidConfig | Self::Unprocessable => 422,
            Self::Unavailable => 503,
            Self::Upstream => 502,
            Self::Timeout => 504,
            Self::Internal | Self::Unknown => 500,
        }
    }
}
