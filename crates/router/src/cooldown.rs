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
        state.revision = state.revision.saturating_add(1);
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
        let mut states = self.lock();
        let state = states.entry(id.clone()).or_default();
        clear(state);
    }

    /// Returns the member's state revision for conditional success recording.
    #[doc(hidden)]
    #[must_use]
    pub fn revision(&self, id: &MemberId) -> u64 {
        self.lock().get(id).map_or(0, |state| state.revision)
    }

    /// Clears state only when no failure or observation arrived during a call.
    #[doc(hidden)]
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
