//! One error type for every plugin call.

use std::error::Error;
use std::fmt;
use std::time::Duration;

/// Why a plugin call failed.
///
/// The router matches this with a wildcard arm, so a new variant does not
/// break the router. [`CountOutcome`](crate::CountOutcome) `Unsupported` is a
/// success. This kind means the call was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PluginErrorKind {
    /// The connection could not be established, or the body failed in transit.
    Connect,
    /// The upstream did not answer within the kernel deadline.
    Timeout,
    /// The upstream returned an HTTP status.
    Status,
    /// The body was not a response this protocol understands.
    Decode,
    /// The plugin does not implement this call.
    Unsupported,
    /// Credentials were missing or rejected.
    Auth,
    /// A quota or rate limit rejected the call.
    Quota,
    /// Configuration did not validate.
    Config,
    /// The kernel cancelled the call.
    Cancelled,
}

/// A plugin call failed.
///
/// Fields are private. Callers branch on [`PluginError::kind`] and read
/// [`PluginError::status`], [`PluginError::retry_after`], and
/// [`PluginError::source`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
#[error("{message}")]
pub struct PluginError {
    kind: PluginErrorKind,
    message: String,
    status: Option<u16>,
    retry_after: Option<Duration>,
    #[source]
    cause: Option<Box<dyn Error + Send + Sync>>,
}

impl PluginError {
    fn new(kind: PluginErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            status: None,
            retry_after: None,
            cause: None,
        }
    }

    /// A connection failure.
    #[must_use]
    pub fn connect(message: impl Into<String>) -> Self {
        Self::new(PluginErrorKind::Connect, message)
    }

    /// A timeout.
    #[must_use]
    pub fn timeout(message: impl Into<String>) -> Self {
        Self::new(PluginErrorKind::Timeout, message)
    }

    /// An HTTP status from the upstream.
    #[must_use]
    pub fn status_error(status: u16, message: impl Into<String>) -> Self {
        Self {
            status: Some(status),
            ..Self::new(PluginErrorKind::Status, message)
        }
    }

    /// A body this protocol could not decode.
    #[must_use]
    pub fn decode(message: impl Into<String>) -> Self {
        Self::new(PluginErrorKind::Decode, message)
    }

    /// The plugin does not implement this call.
    #[must_use]
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(PluginErrorKind::Unsupported, message)
    }

    /// Credentials were missing or rejected.
    #[must_use]
    pub fn auth(message: impl Into<String>) -> Self {
        Self::new(PluginErrorKind::Auth, message)
    }

    /// A quota or rate limit rejected the call.
    #[must_use]
    pub fn quota(message: impl Into<String>) -> Self {
        Self::new(PluginErrorKind::Quota, message)
    }

    /// Configuration did not validate.
    #[must_use]
    pub fn config(message: impl Into<String>) -> Self {
        Self::new(PluginErrorKind::Config, message)
    }

    /// The kernel cancelled the call.
    #[must_use]
    pub fn cancelled(message: impl Into<String>) -> Self {
        Self::new(PluginErrorKind::Cancelled, message)
    }

    /// Attaches a retry hint. The kernel, not the plugin, cools the member down.
    #[must_use]
    pub fn with_retry_after(mut self, after: Duration) -> Self {
        self.retry_after = Some(after);
        self
    }

    /// Attaches the underlying error.
    #[must_use]
    pub fn with_source(mut self, source: impl Error + Send + Sync + 'static) -> Self {
        self.cause = Some(Box::new(source));
        self
    }

    /// How the call failed.
    #[must_use]
    pub const fn kind(&self) -> PluginErrorKind {
        self.kind
    }

    /// HTTP status, when the upstream sent one.
    #[must_use]
    pub const fn status(&self) -> Option<u16> {
        self.status
    }

    /// How long the kernel should wait before trying this member again.
    #[must_use]
    pub const fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }

    /// The underlying error, when one was attached.
    #[must_use]
    pub fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.cause
            .as_deref()
            .map(|error| error as &(dyn Error + 'static))
    }

    /// Text safe to show. It does not include credentials.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for PluginErrorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Connect => "connect",
            Self::Timeout => "timeout",
            Self::Status => "status",
            Self::Decode => "decode",
            Self::Unsupported => "unsupported",
            Self::Auth => "auth",
            Self::Quota => "quota",
            Self::Config => "config",
            Self::Cancelled => "cancelled",
        })
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::time::Duration;

    use super::{PluginError, PluginErrorKind};

    #[test]
    fn accessors_round_trip() {
        let error = PluginError::status_error(429, "slow down")
            .with_retry_after(Duration::from_secs(2))
            .with_source(std::io::Error::other("body"));
        assert_eq!(error.kind(), PluginErrorKind::Status);
        assert_eq!(error.status(), Some(429));
        assert_eq!(error.retry_after(), Some(Duration::from_secs(2)));
        let source = Error::source(&error).expect("source");
        assert_eq!(source.to_string(), "body");
        assert_eq!(error.message(), "slow down");
    }

    #[test]
    fn unsupported_is_its_own_kind() {
        let error = PluginError::unsupported("this upstream has no model list");
        assert_eq!(error.kind(), PluginErrorKind::Unsupported);
        assert_eq!(error.status(), None);
        assert_eq!(error.retry_after(), None);
    }
}
