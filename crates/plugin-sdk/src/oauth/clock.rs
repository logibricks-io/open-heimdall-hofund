//! The two clock readings the OAuth helpers need.

use std::time::{Duration, SystemTime};

use hofund_plugin_api::ClockHandle;

/// Wall time and monotonic time.
///
/// [`ClockHandle`] implements this, so a plugin passes `ctx.clock()`. Tests
/// can pass their own clock.
pub trait Clock: Send + Sync {
    /// The current wall time.
    fn now(&self) -> SystemTime;

    /// Time since an arbitrary fixed point. It never goes backwards.
    fn monotonic(&self) -> Duration;
}

impl Clock for ClockHandle {
    fn now(&self) -> SystemTime {
        ClockHandle::now(self)
    }

    fn monotonic(&self) -> Duration {
        ClockHandle::monotonic(self)
    }
}

#[cfg(test)]
pub(crate) mod test_clock {
    use std::time::{Duration, SystemTime};

    use super::Clock;

    /// Follows tokio's clock, so paused tests advance it by sleeping.
    pub(crate) struct TokioClock {
        start: tokio::time::Instant,
    }

    impl TokioClock {
        pub(crate) fn new() -> Self {
            Self {
                start: tokio::time::Instant::now(),
            }
        }
    }

    impl Clock for TokioClock {
        fn now(&self) -> SystemTime {
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000) + self.monotonic()
        }

        fn monotonic(&self) -> Duration {
            self.start.elapsed()
        }
    }

    /// Never moves.
    pub(crate) struct StoppedClock;

    impl Clock for StoppedClock {
        fn now(&self) -> SystemTime {
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000)
        }

        fn monotonic(&self) -> Duration {
            Duration::ZERO
        }
    }
}
