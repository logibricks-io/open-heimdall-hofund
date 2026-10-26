//! Member cooldown state shared across immutable router snapshots.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use hofund_plugin_api::{MemberId, PluginError, PluginErrorKind};

/// Timing limits for member cooldowns.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct CooldownPolicy {
    /// Initial cooldown for a generic failure.
    pub base: Duration,
    /// Maximum cooldown for consecutive generic failures.
    pub generic_max: Duration,
    /// Maximum ladder cooldown for rate-limit failures.
    pub rate_max: Duration,
    /// Time after a rate-limit rest when its ladder is forgotten.
    pub rate_forget: Duration,
    /// Maximum retry hint for rate-limit and credit failures.
    pub retry_after_max: Duration,
    /// Cooldown for an exhausted quota without a retry hint.
    pub quota_default: Duration,
    /// Maximum retry hint for an exhausted quota.
    pub quota_max: Duration,
    /// Minimum cooldown for insufficient credit.
    pub credit: Duration,
    /// Maximum cooldown for a passive observation that is not exhausted.
    pub observed_max: Duration,
}

impl Default for CooldownPolicy {
    fn default() -> Self {
        Self {
            base: Duration::from_secs(60),
            generic_max: Duration::from_secs(10 * 60),
            rate_max: Duration::from_secs(30 * 60),
            rate_forget: Duration::from_secs(30 * 60),
            retry_after_max: Duration::from_secs(60 * 60),
            quota_default: Duration::from_secs(15 * 60),
            quota_max: Duration::from_secs(8 * 24 * 60 * 60),
            credit: Duration::from_secs(30 * 60),
            observed_max: Duration::from_secs(60 * 60),
        }
    }
}

/// The event that caused a member to cool down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CooldownCause {
    /// A failure using the generic consecutive-failure ladder.
    Failure,
    /// A rate-limit response.
    RateLimit,
    /// An exhausted quota.
    Quota,
    /// Insufficient credit.
    Credit,
    /// Quota headers observed on a successful call.
    Observed,
    /// A contained member panic.
    Panic,
}

impl CooldownCause {
    /// Stable label used by metrics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Failure => "failure",
            Self::RateLimit => "rate_limit",
            Self::Quota => "quota",
            Self::Credit => "credit",
            Self::Observed => "observed",
            Self::Panic => "panic",
        }
    }
}

/// An active member cooldown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Cooling {
    until: Instant,
    cause: CooldownCause,
    failures: u32,
}

impl Cooling {
    /// When the member may be tried normally again.
    #[must_use]
    pub const fn until(&self) -> Instant {
        self.until
    }

    /// What caused the cooldown.
    #[must_use]
    pub const fn cause(&self) -> CooldownCause {
        self.cause
    }

    /// Consecutive failures on the ladder that caused the cooldown.
    #[must_use]
    pub const fn failures(&self) -> u32 {
        self.failures
    }
}

#[derive(Default)]
struct State {
    cooling: Option<Cooling>,
    failures: u32,
    rate_failures: u32,
    rate_until: Option<Instant>,
}

/// Thread-safe cooldown state keyed by stable member id.
///
/// Sharing this book in an [`Arc`] keeps breaker state when a router snapshot
/// replaces every member instance. The reference gateway's ladders are kept
/// here rather than in a plugin, so plugins only report errors and observations.
pub struct Cooldowns {
    policy: CooldownPolicy,
    clock: Arc<dyn Fn() -> Instant + Send + Sync>,
    states: Mutex<HashMap<MemberId, State>>,
}

impl Cooldowns {
    /// Creates a book using the monotonic system clock.
    #[must_use]
    pub fn new(policy: CooldownPolicy) -> Self {
        Self::with_clock(policy, Arc::new(Instant::now))
    }

    /// Creates a book with an injected monotonic clock.
    #[must_use]
    pub fn with_clock(
        policy: CooldownPolicy,
        clock: Arc<dyn Fn() -> Instant + Send + Sync>,
    ) -> Self {
        Self {
            policy,
            clock,
            states: Mutex::new(HashMap::new()),
        }
    }

    /// Records a failed call and returns its cooldown, if the error cools.
    pub fn record_failure(&self, id: &MemberId, error: &PluginError) -> Option<Cooling> {
        let failure = match error.kind() {
            PluginErrorKind::Connect
            | PluginErrorKind::Timeout
            | PluginErrorKind::Transport
            | PluginErrorKind::Decode
            | PluginErrorKind::Io
            | PluginErrorKind::Auth => Failure::Generic(CooldownCause::Failure),
            PluginErrorKind::Status => match error.status() {
                Some(429) => Failure::RateLimit,
                Some(402) => Failure::Credit,
                Some(401 | 403 | 408) | Some(500..=u16::MAX) => {
                    Failure::Generic(CooldownCause::Failure)
                }
                _ => return None,
            },
            PluginErrorKind::Quota => Failure::Quota,
            PluginErrorKind::Unsupported
            | PluginErrorKind::Config
            | PluginErrorKind::Cancelled
            | PluginErrorKind::Internal => return None,
            _ => return None,
        };
        let now = (self.clock)();
        let mut states = self.lock();
        let state = states.entry(id.clone()).or_default();
        let candidate = match failure {
            Failure::Generic(cause) => Self::generic(state, now, &self.policy, cause),
            Failure::RateLimit => self.rate_limit(state, now, error.retry_after()),
            Failure::Quota => {
                let duration = error
                    .retry_after()
                    .map_or(self.policy.quota_default, |after| {
                        after.min(self.policy.quota_max)
                    });
                cooling(now, duration, CooldownCause::Quota, state.failures)
            }
            Failure::Credit => {
                let retry = error
                    .retry_after()
                    .unwrap_or_default()
                    .min(self.policy.retry_after_max);
                cooling(
                    now,
                    self.policy.credit.max(retry),
                    CooldownCause::Credit,
                    state.failures,
                )
            }
        };
        let applied = apply(state, candidate);
        if matches!(failure, Failure::RateLimit) {
            state.rate_until = Some(applied.until);
        }
        drop(states);
        self.emit(id, applied, now);
        Some(applied)
    }

    /// Clears all cooldown and ladder state after a successful call.
    pub fn record_success(&self, id: &MemberId) {
        self.lock().remove(id);
    }

    /// Records a contained panic on the generic failure ladder.
    pub fn record_panic(&self, id: &MemberId) -> Cooling {
        self.record_generic(id, CooldownCause::Panic)
    }

    /// Applies quota headers observed on a successful call.
    pub fn observe(
        &self,
        id: &MemberId,
        retry_after: Option<Duration>,
        remaining: Option<u64>,
    ) -> Option<Cooling> {
        let duration = match (remaining, retry_after) {
            (Some(0), retry_after) => retry_after.map_or(self.policy.quota_default, |after| {
                after.min(self.policy.quota_max)
            }),
            (_, Some(retry_after)) => retry_after.min(self.policy.observed_max),
            _ => return None,
        };
        let now = (self.clock)();
        let mut states = self.lock();
        let state = states.entry(id.clone()).or_default();
        let applied = apply(
            state,
            cooling(now, duration, CooldownCause::Observed, state.failures),
        );
        drop(states);
        self.emit(id, applied, now);
        Some(applied)
    }

    /// Returns the active cooldown, or `None` when it has expired.
    #[must_use]
    pub fn cooling(&self, id: &MemberId) -> Option<Cooling> {
        let now = (self.clock)();
        self.lock()
            .get(id)
            .and_then(|state| state.cooling)
            .filter(|cooling| now < cooling.until)
    }

    /// Returns active cooldowns for a future routing view.
    #[must_use]
    pub fn entries(&self) -> Vec<(MemberId, Cooling)> {
        let now = (self.clock)();
        let mut entries = self
            .lock()
            .iter()
            .filter_map(|(id, state)| {
                state
                    .cooling
                    .filter(|cooling| now < cooling.until)
                    .map(|cooling| (id.clone(), cooling))
            })
            .collect::<Vec<_>>();
        entries.sort_by_key(|(id, _)| id.to_string());
        entries
    }

    fn record_generic(&self, id: &MemberId, cause: CooldownCause) -> Cooling {
        let now = (self.clock)();
        let mut states = self.lock();
        let state = states.entry(id.clone()).or_default();
        let candidate = Self::generic(state, now, &self.policy, cause);
        let applied = apply(state, candidate);
        drop(states);
        self.emit(id, applied, now);
        applied
    }

    fn generic(
        state: &mut State,
        now: Instant,
        policy: &CooldownPolicy,
        cause: CooldownCause,
    ) -> Cooling {
        state.failures = state.failures.saturating_add(1);
        let duration = ladder(policy.base, policy.generic_max, state.failures);
        cooling(now, duration, cause, state.failures)
    }

    fn rate_limit(
        &self,
        state: &mut State,
        now: Instant,
        retry_after: Option<Duration>,
    ) -> Cooling {
        let resting = state.cooling.is_some_and(|cooling| now < cooling.until);
        let within_forget = state
            .rate_until
            .is_some_and(|until| now.saturating_duration_since(until) < self.policy.rate_forget);
        state.rate_failures = if resting && state.rate_failures > 0 {
            state.rate_failures
        } else if within_forget {
            state.rate_failures.saturating_add(1)
        } else {
            1
        };
        let rest = ladder(self.policy.base, self.policy.rate_max, state.rate_failures);
        let retry = retry_after
            .unwrap_or_default()
            .min(self.policy.retry_after_max);
        cooling(
            now,
            rest.max(retry),
            CooldownCause::RateLimit,
            state.rate_failures,
        )
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<MemberId, State>> {
        self.states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn emit(&self, id: &MemberId, cooling: Cooling, now: Instant) {
        let duration_ms = u128_as_u64(cooling.until.saturating_duration_since(now).as_millis());
        let cause = cooling.cause.as_str();
        tracing::info!(
            "member.id" = %id,
            cause,
            duration_ms,
            failures = cooling.failures,
            "member_cooldown"
        );
        metrics::counter!("hofund_member_cooldowns", "cause" => cause).increment(1);
    }
}

#[derive(Clone, Copy)]
enum Failure {
    Generic(CooldownCause),
    RateLimit,
    Quota,
    Credit,
}

fn ladder(base: Duration, cap: Duration, failures: u32) -> Duration {
    let multiplier = 1_u32
        .checked_shl(failures.saturating_sub(1))
        .unwrap_or(u32::MAX);
    base.saturating_mul(multiplier).min(cap)
}

fn cooling(now: Instant, duration: Duration, cause: CooldownCause, failures: u32) -> Cooling {
    Cooling {
        until: now.checked_add(duration).unwrap_or(now),
        cause,
        failures,
    }
}

fn apply(state: &mut State, candidate: Cooling) -> Cooling {
    let until = match state.cooling {
        Some(current) => current.until.max(candidate.until),
        None => candidate.until,
    };
    let applied = Cooling { until, ..candidate };
    state.cooling = Some(applied);
    applied
}

fn u128_as_u64(value: u128) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use hofund_plugin_api::{MemberKey, PresetId};

    use super::{CooldownCause, CooldownPolicy, Cooldowns};
    use crate::{MemberId, PluginError};

    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;

    fn fixture() -> (Cooldowns, MemberId, Arc<Mutex<Instant>>) {
        let now = Arc::new(Mutex::new(Instant::now()));
        let clock = Arc::clone(&now);
        let book = Cooldowns::with_clock(
            CooldownPolicy::default(),
            Arc::new(move || *clock.lock().expect("clock")),
        );
        let id = MemberId::from_preset(
            PresetId::from_static("member"),
            MemberKey::from_static("row"),
        );
        (book, id, now)
    }

    fn advance(clock: &Mutex<Instant>, seconds: u64) {
        let mut now = clock.lock().expect("clock");
        *now += Duration::from_secs(seconds);
    }

    fn remaining(book: &Cooldowns, id: &MemberId, clock: &Mutex<Instant>) -> Duration {
        book.cooling(id)
            .expect("cooling")
            .until()
            .duration_since(*clock.lock().expect("clock"))
    }

    #[test]
    fn generic_ladder_doubles_caps_and_success_clears_it() {
        let (book, id, clock) = fixture();
        for (index, seconds) in [60, 120, 240, 480, 600, 600].into_iter().enumerate() {
            let cooling = book
                .record_failure(&id, &PluginError::connect("down"))
                .expect("cooling");
            assert_eq!(cooling.cause(), CooldownCause::Failure);
            assert_eq!(cooling.failures(), u32::try_from(index).unwrap() + 1);
            assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(seconds));
        }
        book.record_success(&id);
        assert_eq!(book.cooling(&id), None);
        let cooling = book.record_panic(&id);
        assert_eq!(cooling.cause(), CooldownCause::Panic);
        assert_eq!(cooling.failures(), 1);
        assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(60));
    }

    #[test]
    fn every_generic_failure_kind_and_status_cools() {
        let (book, id, _) = fixture();
        for error in [
            PluginError::timeout("late"),
            PluginError::transport("reset"),
            PluginError::decode("bad"),
            PluginError::io("io"),
            PluginError::auth("denied"),
            PluginError::status_error(401, "denied"),
            PluginError::status_error(403, "denied"),
            PluginError::status_error(408, "late"),
            PluginError::status_error(500, "down"),
            PluginError::status_error(599, "down"),
        ] {
            book.record_success(&id);
            assert!(book.record_failure(&id, &error).is_some(), "{error}");
        }
    }

    #[test]
    fn request_shaped_errors_and_statuses_do_not_cool() {
        let (book, id, _) = fixture();
        for error in [
            PluginError::cancelled("cancelled"),
            PluginError::unsupported("unsupported"),
            PluginError::config("config"),
            PluginError::internal("bug"),
            PluginError::status_error(400, "bad"),
            PluginError::status_error(404, "missing"),
            PluginError::status_error(413, "large"),
            PluginError::status_error(422, "invalid"),
        ] {
            assert_eq!(book.record_failure(&id, &error), None, "{error}");
        }
        assert!(book.entries().is_empty());
    }

    #[test]
    fn rate_limit_uses_retry_hint_and_does_not_stretch_its_ladder() {
        let (book, id, clock) = fixture();
        let first = PluginError::status_error(429, "slow")
            .with_retry_after(Duration::from_secs(2 * MINUTE));
        let cooling = book.record_failure(&id, &first).expect("cooling");
        assert_eq!(cooling.failures(), 1);
        assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(120));
        let repeated = book
            .record_failure(&id, &PluginError::status_error(429, "slow"))
            .expect("cooling");
        assert_eq!(repeated.failures(), 1);
        assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(120));
        advance(&clock, 121);
        let next = book
            .record_failure(&id, &PluginError::status_error(429, "slow"))
            .expect("cooling");
        assert_eq!(next.failures(), 2);
        assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(120));
    }

    #[test]
    fn rate_limit_ladder_caps_resets_and_clamps_retry_hint() {
        let (book, id, clock) = fixture();
        let mut previous = 0;
        for (seconds, failures) in [60, 120, 240, 480, 960, 1800, 1800]
            .into_iter()
            .zip([1, 2, 3, 4, 5, 6, 7])
        {
            if previous > 0 {
                advance(&clock, previous + 1);
            }
            let cooling = book
                .record_failure(&id, &PluginError::status_error(429, "slow"))
                .expect("cooling");
            assert_eq!(cooling.failures(), failures);
            assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(seconds));
            previous = seconds;
        }
        advance(&clock, 1800 + 1800);
        let reset = book
            .record_failure(&id, &PluginError::status_error(429, "slow"))
            .expect("cooling");
        assert_eq!(reset.failures(), 1);
        assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(60));
        book.record_success(&id);
        let clamped =
            PluginError::status_error(429, "slow").with_retry_after(Duration::from_secs(2 * HOUR));
        assert_eq!(
            book.record_failure(&id, &clamped)
                .expect("cooling")
                .until()
                .duration_since(*clock.lock().expect("clock")),
            Duration::from_secs(HOUR)
        );
    }

    #[test]
    fn quota_and_credit_use_their_own_limits() {
        let (book, id, clock) = fixture();
        let quota = book
            .record_failure(&id, &PluginError::quota("empty"))
            .expect("quota");
        assert_eq!(quota.cause(), CooldownCause::Quota);
        assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(900));
        book.record_success(&id);
        let quota = PluginError::quota("empty").with_retry_after(Duration::from_secs(9 * DAY));
        book.record_failure(&id, &quota).expect("quota");
        assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(8 * DAY));
        book.record_success(&id);
        let credit = book
            .record_failure(&id, &PluginError::status_error(402, "credit"))
            .expect("credit");
        assert_eq!(credit.cause(), CooldownCause::Credit);
        assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(1800));
        book.record_success(&id);
        let credit = PluginError::status_error(402, "credit")
            .with_retry_after(Duration::from_secs(2 * HOUR));
        book.record_failure(&id, &credit).expect("credit");
        assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(HOUR));
    }

    #[test]
    fn a_new_failure_never_shortens_a_longer_cooldown() {
        let (book, id, clock) = fixture();
        let quota = PluginError::quota("empty").with_retry_after(Duration::from_secs(HOUR));
        let expected = book.record_failure(&id, &quota).expect("quota");
        let generic = book
            .record_failure(&id, &PluginError::connect("down"))
            .expect("generic");
        assert_eq!(generic.until(), expected.until());
        assert_eq!(generic.cause(), CooldownCause::Failure);
        assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(HOUR));
    }

    #[test]
    fn observations_follow_exhaustion_and_retry_hint_rules() {
        let (book, id, clock) = fixture();
        assert_eq!(
            book.observe(&id, None, Some(0)).expect("empty").cause(),
            CooldownCause::Observed
        );
        assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(900));
        book.record_success(&id);
        book.observe(&id, Some(Duration::from_secs(9 * DAY)), Some(0));
        assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(8 * DAY));
        book.record_success(&id);
        book.observe(&id, Some(Duration::from_secs(2 * HOUR)), None);
        assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(HOUR));
        book.record_success(&id);
        book.observe(&id, Some(Duration::from_secs(120)), Some(5));
        assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(120));
        book.record_success(&id);
        assert_eq!(book.observe(&id, None, Some(5)), None);
        assert_eq!(book.observe(&id, None, None), None);
    }

    #[test]
    fn observation_does_not_advance_ladders_or_shorten_a_cooldown() {
        let (book, id, clock) = fixture();
        let failure = book
            .record_failure(&id, &PluginError::connect("down"))
            .expect("failure");
        let observed = book
            .observe(&id, Some(Duration::from_secs(1)), Some(0))
            .expect("observation");
        assert_eq!(observed.until(), failure.until());
        assert_eq!(observed.failures(), 1);
        advance(&clock, 61);
        let next = book
            .record_failure(&id, &PluginError::connect("down"))
            .expect("failure");
        assert_eq!(next.failures(), 2);
    }

    #[test]
    fn expiry_hides_cooldowns_and_entries() {
        let (book, id, clock) = fixture();
        book.record_failure(&id, &PluginError::connect("down"));
        assert_eq!(book.entries().len(), 1);
        advance(&clock, 60);
        assert_eq!(book.cooling(&id), None);
        assert!(book.entries().is_empty());
    }
}
