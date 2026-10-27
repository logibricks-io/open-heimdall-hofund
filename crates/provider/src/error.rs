//! Failures from an upstream call.
//!
//! The router decides which of these may try another upstream. This type only
//! classifies the failure.

use std::time::Duration;

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
    /// The upstream-directed retry delay, when it sent one.
    pub retry_after: Option<Duration>,
}

/// Classification of [`ProviderError`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderErrorKind {
    /// The connection could not be established, or the body failed in transit.
    Connect,
    /// The upstream did not answer within the configured time.
    Timeout,
    /// The request was sent, but its response failed in transit.
    Transport,
    /// The upstream returned an HTTP status.
    Status,
    /// The body was not a response this adapter's protocol understands.
    Decode,
    /// The provider does not implement this call.
    Unsupported,
    /// Authentication or authorization failed.
    Auth,
    /// The upstream quota is exhausted.
    Quota,
    /// Configuration did not validate.
    Config,
    /// The caller cancelled the call.
    Cancelled,
    /// The filesystem or a process call failed.
    Io,
    /// The provider hit an internal failure.
    Internal,
}

impl ProviderError {
    /// A connection failure.
    #[must_use]
    pub fn connect(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::Connect, message)
    }

    /// A timeout.
    #[must_use]
    pub fn timeout(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::Timeout, message)
    }

    /// A transport failure after the request was sent.
    #[must_use]
    pub fn transport(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::Transport, message)
    }

    /// An HTTP status from the upstream.
    #[must_use]
    pub fn status(status: u16, message: impl Into<String>) -> Self {
        Self {
            kind: ProviderErrorKind::Status,
            message: message.into(),
            status: Some(status),
            retry_after: None,
        }
    }

    /// A body this protocol could not decode.
    #[must_use]
    pub fn decode(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::Decode, message)
    }

    /// The provider does not implement this call.
    #[must_use]
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::Unsupported, message)
    }

    /// An authentication or authorization failure.
    #[must_use]
    pub fn auth(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::Auth, message)
    }

    /// An exhausted upstream quota.
    #[must_use]
    pub fn quota(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::Quota, message)
    }

    /// A configuration validation failure.
    #[must_use]
    pub fn config(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::Config, message)
    }

    /// A call cancelled by the caller.
    #[must_use]
    pub fn cancelled(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::Cancelled, message)
    }

    /// A filesystem or process failure.
    #[must_use]
    pub fn io(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::Io, message)
    }

    /// An internal provider failure.
    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::Internal, message)
    }

    /// Attaches an upstream-directed retry delay.
    #[must_use]
    pub fn with_retry_after(mut self, retry_after: Duration) -> Self {
        self.retry_after = Some(retry_after);
        self
    }

    /// The upstream-directed retry delay, when one was supplied.
    #[must_use]
    pub const fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }

    fn new(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            status: None,
            retry_after: None,
        }
    }
}
