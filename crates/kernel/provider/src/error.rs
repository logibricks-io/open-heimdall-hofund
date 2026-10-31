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
    credit: bool,
    cooldown: bool,
}

/// Classification of [`ProviderError`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderErrorKind {
    /// The connection could not be established before the request was sent.
    Connect,
    /// The upstream did not answer with a status within the configured time.
    /// A body-read timeout after a 2xx status is [`Interrupted`](Self::Interrupted).
    Timeout,
    /// The request was sent, but its response failed before a status arrived.
    Transport,
    /// The upstream answered with a 2xx status, then its body failed or stalled before it was complete.
    /// The request was received, so it is never resent to the same member unless the call is idempotent.
    Interrupted,
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

    /// A transport failure after the request was sent and before a status arrived.
    #[must_use]
    pub fn transport(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::Transport, message)
    }

    /// The response body failed or stalled after the upstream answered.
    #[must_use]
    pub fn interrupted(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::Interrupted, message)
    }

    /// An HTTP status from the upstream.
    #[must_use]
    pub fn status(status: u16, message: impl Into<String>) -> Self {
        Self {
            kind: ProviderErrorKind::Status,
            message: message.into(),
            status: Some(status),
            retry_after: None,
            credit: false,
            cooldown: true,
        }
    }

    /// An HTTP status and response body from the upstream.
    #[must_use]
    pub(crate) fn response(status: u16, body: &[u8]) -> Self {
        Self::response_for(status, body, false)
    }

    pub(crate) fn response_for(status: u16, body: &[u8], allocation_quota_rate: bool) -> Self {
        let message = crate::stream::message_from(body);
        if allocation_quota_rate
            && hofund_plugin_sdk::transport::is_allocation_quota_rate(status, body)
        {
            Self::status(status, message)
        } else if hofund_plugin_sdk::transport::is_credit_error(status, body) {
            Self::credit_status(status, message)
        } else if hofund_plugin_sdk::transport::is_quota_error(status, body) {
            Self {
                kind: ProviderErrorKind::Quota,
                message,
                status: Some(status),
                retry_after: None,
                credit: false,
                cooldown: true,
            }
        } else {
            Self::status(status, message)
        }
    }

    /// An upstream credit refusal reported with an HTTP status.
    #[must_use]
    pub fn credit_status(status: u16, message: impl Into<String>) -> Self {
        Self {
            credit: true,
            ..Self::status(status, message)
        }
    }

    pub(crate) fn from_plugin(error: hofund_plugin_api::PluginError) -> Self {
        let kind = error.kind();
        let status = error.status();
        let message = error.message().to_owned();
        let credit = error.is_credit();
        let cooldown = error.cools();
        let mut converted = match kind {
            hofund_plugin_api::PluginErrorKind::Connect => Self::connect(message),
            hofund_plugin_api::PluginErrorKind::Timeout => Self::timeout(message),
            hofund_plugin_api::PluginErrorKind::Transport => Self::transport(message),
            hofund_plugin_api::PluginErrorKind::Interrupted => Self::interrupted(message),
            hofund_plugin_api::PluginErrorKind::Status if credit => {
                Self::credit_status(status.unwrap_or(402), message)
            }
            hofund_plugin_api::PluginErrorKind::Status => {
                Self::status(status.unwrap_or(502), message)
            }
            hofund_plugin_api::PluginErrorKind::Decode => Self::decode(message),
            hofund_plugin_api::PluginErrorKind::Unsupported => Self::unsupported(message),
            hofund_plugin_api::PluginErrorKind::Auth => Self::auth(message),
            hofund_plugin_api::PluginErrorKind::Quota => Self::quota(message),
            hofund_plugin_api::PluginErrorKind::Config => Self::config(message),
            hofund_plugin_api::PluginErrorKind::Cancelled => Self::cancelled(message),
            hofund_plugin_api::PluginErrorKind::Io => Self::io(message),
            hofund_plugin_api::PluginErrorKind::Internal => Self::internal(message),
            _ => Self::decode(message),
        };
        converted.status = status.or(converted.status);
        converted.credit = credit;
        converted.cooldown = cooldown;
        if let Some(after) = error.retry_after() {
            converted = converted.with_retry_after(after);
        }
        converted
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

    /// Marks a failed response shape that must not cool the member.
    #[must_use]
    pub const fn without_cooldown(mut self) -> Self {
        self.cooldown = false;
        self
    }

    /// Whether the upstream refused the call for lack of credit.
    #[must_use]
    pub const fn is_credit(&self) -> bool {
        self.credit
    }

    /// Whether a failure should cool the member.
    #[must_use]
    pub const fn cools(&self) -> bool {
        self.cooldown
    }

    fn new(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            status: None,
            retry_after: None,
            credit: false,
            cooldown: true,
        }
    }
}
