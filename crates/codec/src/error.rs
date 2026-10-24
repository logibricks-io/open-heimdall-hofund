//! Errors from protocol codecs.
//!
//! Malformed input returns one of these values. Library code does not panic on it.

use thiserror::Error;

/// A payload could not be decoded or encoded.
#[derive(Debug, Error)]
pub enum CodecError {
    /// The input was empty or only whitespace.
    #[error("input is empty")]
    Empty,
    /// The bytes are not JSON.
    #[error("JSON is not valid: {0}")]
    Json(#[from] serde_json::Error),
    /// A JSON value was expected to be an object.
    #[error("expected a JSON object")]
    ExpectedObject,
    /// A JSON value was expected to be an array.
    #[error("expected a JSON array")]
    ExpectedArray,
    /// A required field is absent.
    #[error("missing field `{0}`")]
    MissingField(&'static str),
    /// A field is present but has the wrong JSON type.
    #[error("field `{field}` has the wrong type")]
    WrongType {
        /// Field name.
        field: &'static str,
    },
    /// A message role is not one this codec understands.
    #[error("unknown role `{0}`")]
    UnknownRole(String),
    /// The IR contains something this protocol cannot represent, and dropping it
    /// would hide data the caller asked to send.
    #[error("{message}")]
    Unsupported {
        /// What could not be encoded.
        message: String,
    },
    /// An SSE frame is not usable.
    #[error("SSE frame is not valid: {message}")]
    Sse {
        /// What is wrong with the frame.
        message: String,
    },
}
