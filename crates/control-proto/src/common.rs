use serde::{Deserialize, Serialize};

use crate::{ControlError, ControlErrorKind, MAX_ID_BYTES};

/// A stable field-level issue. `reason` is a snake_case code such as
/// `env_override`, `bind_failed`, or `invalid`; `message` is human-readable.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct FieldIssue {
    /// Dotted field name.
    pub field: String,
    /// Stable machine-readable reason code.
    pub reason: String,
    /// Sanitized human-readable explanation.
    pub message: String,
}

impl FieldIssue {
    /// Constructs an issue for use by a server or validator.
    #[must_use]
    pub fn new(
        field: impl Into<String>,
        reason: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            field: field.into(),
            reason: reason.into(),
            message: message.into(),
        }
    }
}

pub(crate) fn validate_revision(field: &str, revision: &str) -> Result<(), ControlError> {
    if revision.len() <= MAX_ID_BYTES {
        return Ok(());
    }
    let message = "a revision exceeds the 256-byte limit";
    let mut error = ControlError::new(ControlErrorKind::Unprocessable, message, "validate");
    error
        .details
        .push(FieldIssue::new(field, "too_large", message));
    Err(error)
}
