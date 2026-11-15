//! Retry budgets for busy failures, of one provider or a route pool, and
//! the planning of the rounds that ask the busy members again.

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use hofund_plugin_api::{PluginError, PluginErrorKind};

const FIRST_PAUSE: Duration = Duration::from_secs(2);
const LONGEST_PAUSE: Duration = Duration::from_secs(30);
const SHORT_RATE_LIMIT: Duration = Duration::from_secs(240);
const VENDOR_TIME_CLAMP: Duration = Duration::from_secs(1);
const REPLAN_ROUNDS: u32 = 12;
/// Pause before the same account is asked again after a regional refusal.
pub(crate) const REGION_RETRY_PAUSE: Duration = Duration::from_secs(1);

/// Supplies runtime-specific sleeping for a patience pause.
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

/// How a pool member inherits patience from nested groups.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PoolMemberPatience {
    /// Replan is off for this member.
    Off,
    /// Use the asked route pool's patience.
    Inherited,
    /// This member's own budget in seconds.
    Wait(Duration),
}

/// A member that failed in a way that passes, and when it may be asked
/// again: now, or when its vendor's `Retry-After` says.
pub(crate) struct Busy {
    /// Its place in the ordered hops.
    pub(crate) index: usize,
    pub(crate) ready: Instant,
    pub(crate) member_patience: PoolMemberPatience,
}

impl Busy {
    fn effective_budget(&self, pool_fallback: Option<Duration>) -> Option<Duration> {
        match self.member_patience {
            PoolMemberPatience::Off => None,
            PoolMemberPatience::Inherited => pool_fallback,
            PoolMemberPatience::Wait(duration) => Some(duration),
        }
    }
}

/// Whether a failure says the member is out of its allowance: quota or
/// credit. Such a member is not asked again within the request.
pub(crate) fn spent(error: &PluginError) -> bool {
    error.is_credit()
        || error.kind() == PluginErrorKind::Quota
        || error.kind() == PluginErrorKind::Status && error.status() == Some(402)
}

pub(crate) fn busy(
    error: &PluginError,
    index: usize,
    member_patience: PoolMemberPatience,
    now: Instant,
) -> Option<Busy> {
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
        || (520..=524).contains(&status)
        || status == 429
            && error
                .retry_after()
                .is_none_or(|after| after <= SHORT_RATE_LIMIT);
    if !eligible {
        return None;
    }
    let ready = error.retry_after().map_or(now, |after| now + after);
    Some(Busy {
        index,
        ready,
        member_patience,
    })
}

/// Whether any busy member may be kept for another pool round.
pub(crate) fn any_member_may_replan(busy: &[Busy], pool_fallback: Option<Duration>) -> bool {
    busy.iter().any(|entry| {
        entry
            .effective_budget(pool_fallback)
            .is_some_and(|budget| !budget.is_zero())
    })
}

/// The members to ask again and the pause before (`reference:
/// internal/gateway/patience.go:113-175`, `replan`): the pause doubles from
/// two seconds each round up to thirty, and is never shorter than the
/// soonest vendor time. Those ready by then go first, in the order they
/// failed, those still asking for time after them. `None` once the rounds
/// are spent or the pause would end past the budget, counted from
/// `wait_from`.
pub(crate) fn replan(
    failures: &mut [Busy],
    round: u32,
    wait_from: Instant,
    pool_fallback: Option<Duration>,
    now: Instant,
) -> Option<(Vec<usize>, Duration)> {
    let wait = pause(failures, round, wait_from, pool_fallback, now)?;
    let at = now.checked_add(wait)?;
    let alive: Vec<&Busy> = failures
        .iter()
        .filter(|failure| {
            let Some(budget) = failure.effective_budget(pool_fallback) else {
                return false;
            };
            at <= wait_from.checked_add(budget).unwrap_or(wait_from)
        })
        .collect();
    if alive.is_empty() {
        return None;
    }
    let (next, later): (Vec<&Busy>, Vec<&Busy>) =
        alive.into_iter().partition(|failure| failure.ready <= at);
    let order = next
        .into_iter()
        .chain(later)
        .map(|failure| failure.index)
        .collect();
    Some((order, wait))
}

pub(crate) fn pause(
    failures: &mut [Busy],
    round: u32,
    wait_from: Instant,
    pool_fallback: Option<Duration>,
    now: Instant,
) -> Option<Duration> {
    if failures.is_empty() || round >= REPLAN_ROUNDS {
        return None;
    }
    let mut candidates = Vec::new();
    for failure in failures.iter_mut() {
        let Some(budget) = failure.effective_budget(pool_fallback) else {
            continue;
        };
        if budget.is_zero() {
            continue;
        }
        let end = wait_from.checked_add(budget)?;
        let clamp_end = end.checked_add(VENDOR_TIME_CLAMP)?;
        if failure.ready > end && failure.ready <= clamp_end {
            failure.ready = end;
        }
        candidates.push(failure);
    }
    if candidates.is_empty() {
        return None;
    }
    let soonest = candidates.iter().map(|failure| failure.ready).min()?;
    let multiplier = 1_u32.checked_shl(round).unwrap_or(u32::MAX);
    let exponential = FIRST_PAUSE.saturating_mul(multiplier).min(LONGEST_PAUSE);
    let vendor = soonest.saturating_duration_since(now);
    let wait = exponential.max(vendor);
    let at = now.checked_add(wait)?;
    let still_alive = candidates.iter().any(|failure| {
        let Some(budget) = failure.effective_budget(pool_fallback) else {
            return false;
        };
        at <= wait_from.checked_add(budget).unwrap_or(wait_from)
    });
    still_alive.then_some(wait)
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

    use hofund_plugin_api::PluginError;

    use super::{Busy, PoolMemberPatience, busy, pause, replan, spent};

    #[test]
    fn b6d_rate_limit_hint_over_240_seconds_is_not_busy() {
        let now = std::time::Instant::now();
        let short =
            PluginError::status_error(429, "slow").with_retry_after(Duration::from_secs(240));
        assert!(
            busy(&short, 0, PoolMemberPatience::Inherited, now).is_some(),
            "a 240-second hint is still short"
        );
        let long =
            PluginError::status_error(429, "slow").with_retry_after(Duration::from_millis(240_500));
        assert!(
            busy(&long, 0, PoolMemberPatience::Inherited, now).is_none(),
            "a hint past 240 seconds is terminal even inside the one-second clamp"
        );
    }

    #[test]
    fn b6e_patience_budget_is_inclusive_with_one_second_clamp() {
        let start = std::time::Instant::now();
        let budget = Duration::from_secs(60);
        let mut exact = [Busy {
            index: 0,
            ready: start + budget,
            member_patience: PoolMemberPatience::Wait(budget),
        }];
        assert_eq!(
            pause(&mut exact, 0, start, Some(budget), start),
            Some(budget),
            "a retry exactly at the budget end is allowed"
        );

        let mut clamped = [Busy {
            index: 0,
            ready: start + budget + Duration::from_millis(900),
            member_patience: PoolMemberPatience::Wait(budget),
        }];
        assert_eq!(
            pause(&mut clamped, 0, start, Some(budget), start),
            Some(budget),
            "a vendor time within one second is clamped"
        );
        assert_eq!(clamped[0].ready, start + budget);

        let mut late = [Busy {
            index: 0,
            ready: start + budget + Duration::from_millis(1001),
            member_patience: PoolMemberPatience::Wait(budget),
        }];
        assert_eq!(pause(&mut late, 0, start, Some(budget), start), None);
    }

    #[test]
    fn b6i_planner_stops_after_twelve_rounds() {
        let start = std::time::Instant::now();
        // A budget no pause can spend isolates the round backstop.
        let budget = Duration::from_secs(24 * 60 * 60);
        let mut ready = [Busy {
            index: 0,
            ready: start,
            member_patience: PoolMemberPatience::Wait(budget),
        }];
        assert_eq!(
            pause(&mut ready, 11, start, Some(budget), start),
            Some(Duration::from_secs(30)),
            "the twelfth pause is still taken"
        );
        assert_eq!(pause(&mut ready, 12, start, Some(budget), start), None);
        assert_eq!(
            pause(&mut ready, u32::MAX, start, Some(budget), start),
            None
        );
        assert_eq!(
            pause(&mut ready, 4, start, Some(budget), start),
            Some(Duration::from_secs(30)),
            "pauses stop doubling at thirty seconds"
        );
        assert_eq!(
            pause(&mut ready, 3, start, Some(budget), start),
            Some(Duration::from_secs(16))
        );
        assert_eq!(pause(&mut [], 0, start, Some(budget), start), None);
        assert_eq!(
            pause(&mut ready, 0, start, Some(Duration::ZERO), start),
            None
        );
    }

    #[test]
    fn b11_replan_asks_ready_members_before_those_still_asking_for_time() {
        let now = std::time::Instant::now();
        let mut failures = [
            Busy {
                index: 0,
                ready: now + Duration::from_secs(20),
                member_patience: PoolMemberPatience::Inherited,
            },
            Busy {
                index: 2,
                ready: now,
                member_patience: PoolMemberPatience::Inherited,
            },
            Busy {
                index: 3,
                ready: now + Duration::from_secs(1),
                member_patience: PoolMemberPatience::Inherited,
            },
        ];
        let (order, wait) =
            replan(&mut failures, 0, now, Some(Duration::from_secs(60)), now).expect("a pause");
        assert_eq!(wait, Duration::from_secs(2), "first pause is two seconds");
        assert_eq!(order, vec![2, 3, 0], "ready first, in failure order");
        assert!(
            replan(&mut failures, 12, now, Some(Duration::from_secs(60)), now).is_none(),
            "twelve rounds at most"
        );
    }

    #[test]
    fn r4_75_cloudflare_520_through_524_count_as_busy() {
        let now = std::time::Instant::now();
        for status in [520, 521, 522, 523, 524] {
            let error = PluginError::status_error(status, "cloudflare");
            assert!(
                busy(&error, 0, PoolMemberPatience::Inherited, now).is_some(),
                "status {status} should pass"
            );
        }
    }

    #[test]
    fn b11_quota_and_credit_failures_are_spent_not_busy() {
        let now = std::time::Instant::now();
        let quota = PluginError::quota("out of quota");
        assert!(spent(&quota) && busy(&quota, 0, PoolMemberPatience::Inherited, now).is_none());
        let credit = PluginError::status_error(402, "pay up");
        assert!(spent(&credit) && busy(&credit, 0, PoolMemberPatience::Inherited, now).is_none());
        let overloaded = PluginError::status_error(529, "overloaded");
        assert!(
            !spent(&overloaded)
                && busy(&overloaded, 1, PoolMemberPatience::Inherited, now)
                    .is_some_and(|b| b.index == 1)
        );
    }
}
