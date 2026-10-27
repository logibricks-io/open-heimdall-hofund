//! Member cooldown state shared across immutable router snapshots.
//!
//! # Default policy
//!
//! | Event | Cooldown and ladder |
//! | --- | --- |
//! | Generic failure | Starts at 1 minute and doubles for each consecutive failure since the last success, capped at 10 minutes. Connection, timeout, transport, interrupted-body, decode, I/O, and authentication errors use this ladder, as do HTTP 401, 403, 408, and 5xx statuses. |
//! | HTTP 429 | Uses the larger of the rate ladder and the retry hint, with the hint capped at 1 hour. The rate ladder starts at 1 minute, doubles for a new 429 within 30 minutes after the previous rate rest ended, and is capped at 30 minutes. A 429 during an active rest does not advance the ladder. |
//! | Exhausted quota | Uses the header retry hint capped at 1 hour, or 15 minutes without a positive hint. |
//! | HTTP 402 | Uses 30 minutes, or the retry hint capped at 1 hour when that is longer. |
//! | Passive observation | An exhausted window uses its stated reset capped at 8 days, or 15 minutes without a positive reset. Any other observation records nothing. Observations do not advance either failure ladder. |
//! | Contained panic | Uses the generic failure ladder with [`CooldownCause::Panic`]. |
//!
//! Cancelled, unsupported, configuration, internal, unknown future, and other
//! request-shaped HTTP 4xx errors do not cool a member. A new failure,
//! observation, or panic never shortens an existing longer cooldown.
//!
//! Routing keeps ready members first in configured order, then keeps only the
//! cooling member whose rest ends soonest as a probe. Configured order breaks a
//! tie, other cooling members are skipped for that request, and a route with one
//! member always tries that member. If every member is cooling, only the
//! soonest probe is tried.

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
    /// Maximum HTTP retry hint for every failure cause.
    pub retry_after_max: Duration,
    /// Cooldown for an exhausted quota without a positive retry hint.
    pub quota_default: Duration,
    /// Maximum vendor-stated reset for an exhausted quota window.
    pub quota_max: Duration,
    /// Minimum cooldown for insufficient credit.
    pub credit: Duration,
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
    revision: u64,
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
    /// Interrupted bodies use the generic failure policy
    /// (`reference: internal/gateway/fallback.go:1406-1413`).
    pub fn record_failure(&self, id: &MemberId, error: &PluginError) -> Option<Cooling> {
        let failure = match error.kind() {
            PluginErrorKind::Connect
            | PluginErrorKind::Timeout
            | PluginErrorKind::Transport
            | PluginErrorKind::Interrupted
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
        state.revision = state.revision.saturating_add(1);
        let candidate = match failure {
            Failure::Generic(cause) => Self::generic(state, now, &self.policy, cause),
            Failure::RateLimit => self.rate_limit(state, now, error.retry_after()),
            Failure::Quota => {
                let duration = positive_hint(error.retry_after())
                    .map_or(self.policy.quota_default, |after| {
                        after.min(self.policy.retry_after_max)
                    });
                cooling(now, duration, CooldownCause::Quota, state.failures)
            }
            Failure::Credit => {
                let retry = positive_hint(error.retry_after())
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
    ///
    /// This unconditional clear also advances the revision, preventing an older
    /// in-flight call from clearing state recorded after this success.
    pub fn record_success(&self, id: &MemberId) {
        let mut states = self.lock();
        let state = states.entry(id.clone()).or_default();
        clear(state);
    }

    /// Captures the member's state revision before starting a call.
    ///
    /// Pass the returned revision to [`Self::record_success_if_unchanged`] when
    /// the call succeeds so it cannot erase a newer failure, panic, or
    /// observation.
    #[must_use]
    pub fn revision(&self, id: &MemberId) -> u64 {
        self.lock().get(id).map_or(0, |state| state.revision)
    }

    /// Records success only if no failure, panic, or observation landed during
    /// the call that captured `revision`.
    ///
    /// The revision guard prevents an older successful call from clearing newer
    /// cooldown or ladder state. When the revision still matches, this clears
    /// the same state as [`Self::record_success`].
    pub fn record_success_if_unchanged(&self, id: &MemberId, revision: u64) {
        let mut states = self.lock();
        let state = states.entry(id.clone()).or_default();
        if state.revision == revision {
            clear(state);
        }
    }

    /// Records a contained panic on the generic failure ladder.
    pub fn record_panic(&self, id: &MemberId) -> Cooling {
        self.record_generic(id, CooldownCause::Panic)
    }

    /// Applies an exhausted quota window observed on a successful call.
    ///
    /// Other observations record nothing and do not advance the state revision.
    pub fn observe(
        &self,
        id: &MemberId,
        retry_after: Option<Duration>,
        remaining: Option<u64>,
    ) -> Option<Cooling> {
        if remaining != Some(0) {
            return None;
        }
        let duration = positive_hint(retry_after).map_or(self.policy.quota_default, |after| {
            after.min(self.policy.quota_max)
        });
        let now = (self.clock)();
        let mut states = self.lock();
        let state = states.entry(id.clone()).or_default();
        state.revision = state.revision.saturating_add(1);
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
        state.revision = state.revision.saturating_add(1);
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
        let retry = positive_hint(retry_after)
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

fn positive_hint(duration: Option<Duration>) -> Option<Duration> {
    duration.filter(|duration| !duration.is_zero())
}

/// Keeps an overflowing rest active at a far deadline rather than failing open.
fn saturating_deadline(now: Instant, rest: Duration) -> Instant {
    const CENTURY: Duration = Duration::from_secs(100 * 365 * 24 * 60 * 60);
    const YEAR: Duration = Duration::from_secs(365 * 24 * 60 * 60);
    now.checked_add(rest)
        .or_else(|| now.checked_add(CENTURY))
        .or_else(|| now.checked_add(YEAR))
        .unwrap_or(now)
}

fn cooling(now: Instant, duration: Duration, cause: CooldownCause, failures: u32) -> Cooling {
    Cooling {
        until: saturating_deadline(now, duration),
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

fn clear(state: &mut State) {
    let revision = state.revision.saturating_add(1);
    *state = State {
        revision,
        ..State::default()
    };
}

fn u128_as_u64(value: u128) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}
