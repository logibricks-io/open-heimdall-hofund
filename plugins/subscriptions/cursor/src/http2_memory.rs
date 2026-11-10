//! Process-global memory of a transport's HTTP/2 unavailability.

use std::sync::Mutex;
#[cfg(test)]
use std::sync::MutexGuard;
use std::time::Duration;

use hofund_plugin_api::{HostContext, PluginError, PluginErrorKind};

const HEADERS_TIMEOUT: &str = "duplex headers timed out";
const HTTP2_UNAVAILABLE: &str = "http2 unavailable";
const STREAM_REFUSED: &str = "stream refused";

static BLOCKED_AT: Mutex<Option<Duration>> = Mutex::new(None);
#[cfg(test)]
static TEST_LOCK: Mutex<()> = Mutex::new(());

/// Whether HTTP/2 is still unavailable at the host's monotonic reading.
pub(crate) fn blocked(host: &HostContext, ttl: Duration) -> bool {
    let now = host.clock().monotonic();
    let mut slot = BLOCKED_AT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if slot.is_some_and(|at| now.saturating_sub(at) >= ttl) {
        *slot = None;
    }
    slot.is_some()
}

/// Remembers an HTTP/2 failure at the host's current monotonic reading.
pub(crate) fn remember(host: &HostContext) {
    *BLOCKED_AT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(host.clock().monotonic());
}

/// Forgets HTTP/2 unavailability, allowing a later region retry to dial it.
pub(crate) fn clear() {
    *BLOCKED_AT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

/// Classifies only stable host kinds and prefixes, never a client error string.
pub(crate) fn should_remember(error: &PluginError) -> bool {
    match error.kind() {
        PluginErrorKind::Timeout => error.message().starts_with(HEADERS_TIMEOUT),
        PluginErrorKind::Transport => {
            !error.message().starts_with(STREAM_REFUSED)
                && error.message().starts_with(HTTP2_UNAVAILABLE)
        }
        _ => false,
    }
}

#[cfg(test)]
pub(crate) fn reset_for_tests() {
    clear();
}

#[cfg(test)]
pub(crate) fn test_lock() -> MutexGuard<'static, ()> {
    TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use hofund_kit_subscription::{TestHost, advance_time, block_on_paused};
    use hofund_plugin_api::{PluginError, PluginId, PluginKind};

    use super::{blocked, clear, remember, reset_for_tests, should_remember, test_lock};

    fn host() -> TestHost {
        TestHost::open_for_with_tokio_monotonic(
            PluginKind::Subscription,
            PluginId::from_static("cursor"),
            tempfile::tempdir().expect("tempdir").keep(),
        )
    }

    #[test]
    fn only_the_stable_http2_signals_set_memory() {
        let _guard = test_lock();
        reset_for_tests();
        let cases = [
            (PluginError::timeout("duplex headers timed out: test"), true),
            (PluginError::timeout("another timeout"), false),
            (PluginError::connect("connection refused"), false),
            (PluginError::transport("stream refused: test"), false),
            (PluginError::transport("http2 unavailable: ALPN"), true),
            (
                PluginError::transport("http2 unavailable: h2 is not supported"),
                true,
            ),
            (PluginError::transport("http2 unavailable: nghttp2"), true),
            (
                PluginError::transport("http2 unavailable: protocol error"),
                true,
            ),
            (PluginError::transport("h2 is not supported"), false),
            (PluginError::cancelled("caller left"), false),
            (PluginError::interrupted("body failed"), false),
        ];
        for (error, expected) in cases {
            assert_eq!(should_remember(&error), expected, "{}", error.message());
        }
    }

    #[test]
    fn memory_expires_after_ten_minutes_and_clear_forgets_it() {
        let _guard = test_lock();
        reset_for_tests();
        block_on_paused(async {
            let host = host();
            remember(host.context());
            assert!(blocked(host.context(), Duration::from_secs(600)));
            advance_time(Duration::from_secs(599)).await;
            assert!(blocked(host.context(), Duration::from_secs(600)));
            advance_time(Duration::from_secs(2)).await;
            assert!(!blocked(host.context(), Duration::from_secs(600)));

            remember(host.context());
            clear();
            assert!(!blocked(host.context(), Duration::from_secs(600)));
        });
    }
}
