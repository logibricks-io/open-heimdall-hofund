//! Failures from an upstream call.
//!
//! The router decides which of these may try another upstream. This type only
//! classifies the failure.

use thiserror::Error;

/// An upstream call failed before a normal completion was produced.
#[derive(Debug, Error)]
#[error("{message}")]
pub struct ProviderError {
    /// How the call failed.
    pub kind: ProviderErrorKind,
    /// A short description safe to show to the caller. It does not include
    /// request credentials.
    pub message: String,
    /// HTTP status, when the upstream sent one.
    pub status: Option<u16>,
}

/// Classification of [`ProviderError`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderErrorKind {
    /// The connection could not be established, or the body failed in transit.
    Connect,
    /// The upstream did not answer within the configured time.
    Timeout,
    /// The upstream returned an HTTP status.
    Status,
    /// The body was not a response this adapter's protocol understands.
    Decode,
}

impl ProviderError {
    /// A connection failure.
    #[must_use]
    pub fn connect(message: impl Into<String>) -> Self {
        Self {
            kind: ProviderErrorKind::Connect,
            message: message.into(),
            status: None,
        }
    }

    /// A timeout.
    #[must_use]
    pub fn timeout(message: impl Into<String>) -> Self {
        Self {
            kind: ProviderErrorKind::Timeout,
            message: message.into(),
            status: None,
        }
    }

    /// An HTTP status from the upstream.
    #[must_use]
    pub fn status(status: u16, message: impl Into<String>) -> Self {
        Self {
            kind: ProviderErrorKind::Status,
            message: message.into(),
            status: Some(status),
        }
    }

    /// A body this protocol could not decode.
    #[must_use]
    pub fn decode(message: impl Into<String>) -> Self {
        Self {
            kind: ProviderErrorKind::Decode,
            message: message.into(),
            status: None,
        }
    }
}
