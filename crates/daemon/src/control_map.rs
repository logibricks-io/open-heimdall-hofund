//! Stable mappings between internal failures and control wire values.

use std::time::Duration;

use hofund_control_proto::{ControlErrorKind, CooldownReason};
use hofund_provider::ProviderErrorKind;
use hofund_router::CooldownCause;

/// Transport-independent failure mapping produced by the daemon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WireFailure {
    /// Stable wire error category.
    pub kind: ControlErrorKind,
    /// HTTP status selected for this failure.
    pub http_status: u16,
    /// Retry hint rounded up to whole milliseconds.
    pub retry_after_ms: Option<u64>,
}

/// Maps an internal provider failure to its stable control representation.
#[must_use]
pub fn provider_error(
    kind: ProviderErrorKind,
    status: Option<u16>,
    retry_after: Option<Duration>,
) -> WireFailure {
    let (kind, http_status) = match kind {
        ProviderErrorKind::Timeout => (ControlErrorKind::Timeout, 504),
        ProviderErrorKind::Connect
        | ProviderErrorKind::Transport
        | ProviderErrorKind::Interrupted
        | ProviderErrorKind::Decode
        | ProviderErrorKind::Unsupported
        | ProviderErrorKind::Auth
        | ProviderErrorKind::Quota
        | ProviderErrorKind::Status => (
            ControlErrorKind::Upstream,
            if status == Some(429) { 429 } else { 502 },
        ),
        ProviderErrorKind::Config => (ControlErrorKind::InvalidConfig, 422),
        ProviderErrorKind::Cancelled => (ControlErrorKind::Unavailable, 503),
        ProviderErrorKind::Io | ProviderErrorKind::Internal => (ControlErrorKind::Internal, 500),
    };
    WireFailure {
        kind,
        http_status,
        retry_after_ms: retry_after.map(round_up_ms),
    }
}

/// Maps a cooldown cause to its stable wire reason.
#[must_use]
pub fn cooldown_reason(cause: CooldownCause) -> CooldownReason {
    match cause {
        CooldownCause::Failure => CooldownReason::Failure,
        CooldownCause::RateLimit => CooldownReason::RateLimit,
        CooldownCause::Quota => CooldownReason::Quota,
        CooldownCause::Credit => CooldownReason::Credit,
        CooldownCause::Observed => CooldownReason::Observed,
        CooldownCause::Panic => CooldownReason::Panic,
        _ => CooldownReason::Unknown,
    }
}

fn round_up_ms(duration: Duration) -> u64 {
    let milliseconds = duration.as_millis().saturating_add(u128::from(
        !duration.subsec_nanos().is_multiple_of(1_000_000),
    ));
    u64::try_from(milliseconds).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_error_kind_mapping_is_stable() {
        let cases = [
            (ProviderErrorKind::Timeout, ControlErrorKind::Timeout, 504),
            (ProviderErrorKind::Connect, ControlErrorKind::Upstream, 502),
            (
                ProviderErrorKind::Transport,
                ControlErrorKind::Upstream,
                502,
            ),
            (
                ProviderErrorKind::Interrupted,
                ControlErrorKind::Upstream,
                502,
            ),
            (ProviderErrorKind::Decode, ControlErrorKind::Upstream, 502),
            (
                ProviderErrorKind::Unsupported,
                ControlErrorKind::Upstream,
                502,
            ),
            (ProviderErrorKind::Auth, ControlErrorKind::Upstream, 502),
            (ProviderErrorKind::Quota, ControlErrorKind::Upstream, 502),
            (ProviderErrorKind::Status, ControlErrorKind::Upstream, 502),
            (
                ProviderErrorKind::Config,
                ControlErrorKind::InvalidConfig,
                422,
            ),
            (
                ProviderErrorKind::Cancelled,
                ControlErrorKind::Unavailable,
                503,
            ),
            (ProviderErrorKind::Io, ControlErrorKind::Internal, 500),
            (ProviderErrorKind::Internal, ControlErrorKind::Internal, 500),
        ];
        for (internal, kind, http_status) in cases {
            let mapped = provider_error(internal, None, None);
            assert_eq!(mapped.kind, kind, "{internal:?}");
            assert_eq!(mapped.http_status, http_status, "{internal:?}");
            assert_eq!(mapped.retry_after_ms, None, "{internal:?}");
        }
        assert_eq!(
            provider_error(ProviderErrorKind::Status, Some(429), None).http_status,
            429
        );
        assert_eq!(
            provider_error(ProviderErrorKind::Status, Some(503), None).http_status,
            502
        );
        assert_eq!(
            provider_error(
                ProviderErrorKind::Connect,
                None,
                Some(Duration::from_nanos(1)),
            )
            .retry_after_ms,
            Some(1)
        );
        assert_eq!(
            provider_error(
                ProviderErrorKind::Connect,
                None,
                Some(Duration::from_millis(1)),
            )
            .retry_after_ms,
            Some(1)
        );
        assert_eq!(
            provider_error(
                ProviderErrorKind::Connect,
                None,
                Some(Duration::from_millis(1) + Duration::from_nanos(1)),
            )
            .retry_after_ms,
            Some(2)
        );
        assert_eq!(
            provider_error(ProviderErrorKind::Connect, None, Some(Duration::MAX)).retry_after_ms,
            Some(u64::MAX)
        );
    }

    #[test]
    fn cooldown_cause_mapping_is_stable() {
        let causes = [
            CooldownCause::Failure,
            CooldownCause::RateLimit,
            CooldownCause::Quota,
            CooldownCause::Credit,
            CooldownCause::Observed,
            CooldownCause::Panic,
        ];
        for cause in causes {
            assert_eq!(cooldown_reason(cause).as_str(), cause.as_str());
        }
    }
}
