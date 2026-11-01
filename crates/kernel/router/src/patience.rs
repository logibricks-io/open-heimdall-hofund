//! Direct-provider retry budgets and busy-failure planning.

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use hofund_plugin_api::{PluginError, PluginErrorKind};

const FIRST_PAUSE: Duration = Duration::from_secs(2);
const LONGEST_PAUSE: Duration = Duration::from_secs(30);
const SHORT_RATE_LIMIT: Duration = Duration::from_secs(240);
const VENDOR_TIME_CLAMP: Duration = Duration::from_secs(1);
const REPLAN_ROUNDS: u32 = 12;

/// Supplies runtime-specific sleeping for a direct-provider patience pause.
pub trait WaitHook: Send + Sync {
    /// The router is about to wait before another attempt.
    fn waiting(&self) {}

    /// Waits for `duration` before the router makes another attempt.
    fn wait(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;

    /// Reads the monotonic clock used for vendor readiness and the budget.
    fn now(&self) -> Instant {
        Instant::now()
    }
}

pub(crate) struct Busy {
    pub(crate) ready: Instant,
}

pub(crate) fn busy(error: &PluginError, now: Instant) -> Option<Busy> {
    if error.kind() != PluginErrorKind::Status
        || error.is_credit()
        || !crate::decision::rests(error)
        || crate::decision::outgrown(error)
        || token_wall(error.message())
    {
        return None;
    }
    let status = error.status()?;
    let eligible = matches!(status, 408 | 500 | 502 | 503 | 504 | 529)
        || status == 429
            && error
                .retry_after()
                .is_none_or(|after| after <= SHORT_RATE_LIMIT);
    if !eligible {
        return None;
    }
    let ready = error.retry_after().map_or(now, |after| now + after);
    Some(Busy { ready })
}

pub(crate) fn pause(
    failures: &mut [Busy],
    round: u32,
    wait_from: Instant,
    budget: Duration,
    now: Instant,
) -> Option<Duration> {
    if failures.is_empty() || budget.is_zero() || round >= REPLAN_ROUNDS {
        return None;
    }
    let end = wait_from.checked_add(budget)?;
    let clamp_end = end.checked_add(VENDOR_TIME_CLAMP)?;
    let mut soonest = None;
    for failure in failures {
        if failure.ready > end && failure.ready <= clamp_end {
            failure.ready = end;
        }
        soonest =
            Some(soonest.map_or(failure.ready, |current: Instant| current.min(failure.ready)));
    }
    let multiplier = 1_u32.checked_shl(round).unwrap_or(u32::MAX);
    let exponential = FIRST_PAUSE.saturating_mul(multiplier).min(LONGEST_PAUSE);
    let vendor = soonest?.saturating_duration_since(now);
    let wait = exponential.max(vendor);
    let at = now.checked_add(wait)?;
    (at <= end).then_some(wait)
}

fn token_wall(message: &str) -> bool {
    let Ok(serde_json::Value::Object(body)) = serde_json::from_str(message) else {
        return false;
    };
    body.get("error")
        .and_then(serde_json::Value::as_object)
        .and_then(|error| error.get("code"))
        .and_then(serde_json::Value::as_str)
        == Some("user_model_rate_limited")
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Busy, pause};

    #[test]
    fn b6e_patience_budget_is_inclusive_with_one_second_clamp() {
        let start = std::time::Instant::now();
        let budget = Duration::from_secs(60);
        let mut exact = [Busy {
            ready: start + budget,
        }];
        assert_eq!(
            pause(&mut exact, 0, start, budget, start),
            Some(budget),
            "a retry exactly at the budget end is allowed"
        );

        let mut clamped = [Busy {
            ready: start + budget + Duration::from_millis(900),
        }];
        assert_eq!(
            pause(&mut clamped, 0, start, budget, start),
            Some(budget),
            "a vendor time within one second is clamped"
        );
        assert_eq!(clamped[0].ready, start + budget);

        let mut late = [Busy {
            ready: start + budget + Duration::from_millis(1001),
        }];
        assert_eq!(pause(&mut late, 0, start, budget, start), None);
    }

    #[test]
    fn b6i_planner_stops_after_twelve_rounds() {
        let start = std::time::Instant::now();
        // A budget no pause can spend isolates the round backstop.
        let budget = Duration::from_secs(24 * 60 * 60);
        let mut ready = [Busy { ready: start }];
        assert_eq!(
            pause(&mut ready, 11, start, budget, start),
            Some(Duration::from_secs(30)),
            "the twelfth pause is still taken"
        );
        assert_eq!(pause(&mut ready, 12, start, budget, start), None);
        assert_eq!(pause(&mut ready, u32::MAX, start, budget, start), None);
        assert_eq!(
            pause(&mut ready, 4, start, budget, start),
            Some(Duration::from_secs(30)),
            "pauses stop doubling at thirty seconds"
        );
        assert_eq!(
            pause(&mut ready, 3, start, budget, start),
            Some(Duration::from_secs(16))
        );
        assert_eq!(pause(&mut [], 0, start, budget, start), None);
        assert_eq!(pause(&mut ready, 0, start, Duration::ZERO, start), None);
    }
}
