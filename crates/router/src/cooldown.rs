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
//! | Passive observation | Zero remaining uses the window reset capped at 8 days, the retry hint capped at 1 hour, or 15 minutes without either. Positive remaining at least 98% used with a known positive limit rests only for a positive window reset capped at 8 days. Accounts spending credits and all other observations record nothing. Observations do not advance either failure ladder. |
//! | Contained panic | Uses the generic failure ladder with [`CooldownCause::Panic`]. |
//!
//! Cancelled, unsupported, configuration, internal, unknown future, and other
//! request-shaped HTTP 4xx errors do not cool a member. A new failure,
//! observation, or panic never shortens an existing longer cooldown. Idle
//! ladder history is forgotten after `rate_forget`; active rests never expire
//! as idle state. The book holds at most `max_entries` members and evicts
//! expired entries, then the least-recently-touched idle entry, on insertion.
//!
//! Routing keeps ready members first in configured order, then keeps only the
//! cooling member whose rest ends soonest as a probe. Configured order breaks a
//! tie, other cooling members are skipped for that request, and a route with one
//! member always tries that member. If every member is cooling, only the
//! soonest probe is tried.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use hofund_plugin_api::{MemberId, PluginError, PluginErrorKind, QuotaObservation};

const RECOVERY_CAPACITY: usize = 1_024;

/// Timing and capacity limits for member cooldowns.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct CooldownPolicy {
    /// Initial cooldown for a generic failure.
    pub base: Duration,
    /// Maximum cooldown for consecutive generic failures.
    pub generic_max: Duration,
    /// Maximum ladder cooldown for rate-limit failures.
    pub rate_max: Duration,
    /// Time after a rest when its idle ladder history is forgotten.
    pub rate_forget: Duration,
    /// Maximum HTTP retry hint for every failure cause.
    pub retry_after_max: Duration,
    /// Cooldown for an exhausted quota without a positive retry hint.
    pub quota_default: Duration,
    /// Maximum vendor-stated reset for an exhausted quota window.
    pub quota_max: Duration,
    /// Minimum cooldown for insufficient credit.
    pub credit: Duration,
    /// Maximum number of member states retained by the book.
    pub max_entries: usize,
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
            max_entries: 10_000,
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
    /// Every cooldown cause. A new variant must be added to this list.
    pub const ALL: [CooldownCause; 6] = [
        Self::Failure,
        Self::RateLimit,
        Self::Quota,
        Self::Credit,
        Self::Observed,
        Self::Panic,
    ];

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

struct State {
    cooling: Option<Cooling>,
    failures: u32,
    rate_failures: u32,
    rate_until: Option<Instant>,
    idle_since: Option<Instant>,
    touched: Instant,
    revision: u64,
}

impl State {
    fn new(now: Instant, revision: u64) -> Self {
        Self {
            cooling: None,
            failures: 0,
            rate_failures: 0,
            rate_until: None,
            idle_since: None,
            touched: now,
            revision,
        }
    }

    fn active(&self, now: Instant) -> bool {
        self.cooling.is_some_and(|cooling| now < cooling.until)
    }
}

struct Book {
    states: HashMap<MemberId, State>,
    next_revision: u64,
    recoveries: VecDeque<(MemberId, CooldownCause)>,
}

/// Thread-safe cooldown state keyed by stable member id.
///
/// Sharing this book in an [`Arc`] keeps breaker state when a router snapshot
/// replaces every member instance. The reference gateway's ladders are kept
/// here rather than in a plugin, so plugins only report errors and observations.
pub struct Cooldowns {
    policy: CooldownPolicy,
    clock: Arc<dyn Fn() -> Instant + Send + Sync>,
    book: Mutex<Book>,
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
            book: Mutex::new(Book {
                states: HashMap::new(),
                next_revision: 1,
                recoveries: VecDeque::new(),
            }),
        }
    }

    /// Records a failed call and returns its cooldown, if the error cools.
    ///
    /// Interrupted bodies use the generic failure policy
    /// (`reference: internal/gateway/fallback.go:1347-1355`). At the capacity
    /// bound, a failure that cannot evict an idle entry returns its untracked
    /// cooling decision without inserting it; active rests are never evicted.
    pub fn record_failure(&self, id: &MemberId, error: &PluginError) -> Option<Cooling> {
        let failure = failure_kind(error);
        let now = (self.clock)();
        let mut book = self.lock();
        self.maintain_locked(&mut book, now);
        let failure = failure?;
        let revision = next_revision(&mut book);
        let Some(state) = self.entry_for_insert(&mut book, id, now, revision) else {
            // A full book of active rests can consume a revision without inserting.
            let mut state = State::new(now, 0);
            return Some(self.apply_failure(&mut state, failure, now, error));
        };
        let applied = self.apply_failure(state, failure, now, error);
        drop(book);
        self.emit(id, applied, now);
        Some(applied)
    }

    /// Clears all cooldown and ladder state after a successful call.
    ///
    /// A missing member is never inserted. Clearing an existing member removes
    /// its entry, and a later call cannot inherit its revision.
    pub fn record_success(&self, id: &MemberId) {
        let now = (self.clock)();
        let mut book = self.lock();
        self.maintain_locked(&mut book, now);
        if let Some(state) = book.states.remove(id)
            && let Some(cooling) = state.cooling
        {
            recover(&mut book, id.clone(), cooling.cause);
        }
    }

    /// Captures the member's state revision before starting a call.
    ///
    /// Pass the returned revision to [`Self::record_success_if_unchanged`] when
    /// the call succeeds so it cannot erase a newer failure, panic, or
    /// observation. Zero means the member has no entry.
    #[must_use]
    pub fn revision(&self, id: &MemberId) -> u64 {
        let now = (self.clock)();
        let mut book = self.lock();
        self.maintain_locked(&mut book, now);
        let revision = book.states.get(id).map_or(0, |state| state.revision);
        if let Some(state) = book.states.get_mut(id) {
            state.touched = now;
        }
        revision
    }

    /// Records success only if no failure, panic, or observation landed during
    /// the call that captured `revision`.
    ///
    /// The book-wide revision guard prevents an older successful call from
    /// clearing newer state, including state recreated after a missing capture.
    /// Success never inserts a missing entry.
    pub fn record_success_if_unchanged(&self, id: &MemberId, revision: u64) {
        let now = (self.clock)();
        let mut book = self.lock();
        self.maintain_locked(&mut book, now);
        if revision != 0
            && book
                .states
                .get(id)
                .is_some_and(|state| state.revision == revision)
            && let Some(state) = book.states.remove(id)
            && let Some(cooling) = state.cooling
        {
            recover(&mut book, id.clone(), cooling.cause);
        }
    }

    /// Records a contained panic on the generic failure ladder.
    pub fn record_panic(&self, id: &MemberId) -> Cooling {
        let now = (self.clock)();
        let mut book = self.lock();
        self.maintain_locked(&mut book, now);
        let revision = next_revision(&mut book);
        let Some(state) = self.entry_for_insert(&mut book, id, now, revision) else {
            let mut state = State::new(now, 0);
            return Self::generic(&mut state, now, &self.policy, CooldownCause::Panic);
        };
        let candidate = Self::generic(state, now, &self.policy, CooldownCause::Panic);
        let applied = apply(state, candidate);
        drop(book);
        self.emit(id, applied, now);
        applied
    }

    /// Applies an exhausted quota window observed on a successful call.
    ///
    /// Zero remaining uses reset, retry, then default cooldowns. Positive
    /// remaining at or above 98% used rests only for a known positive window
    /// reset (reference: internal/provider/routing.go:365-375). Accounts
    /// spending credits and all other observations record nothing and do not
    /// advance the state revision.
    pub fn observe(&self, id: &MemberId, observation: &QuotaObservation) -> Option<Cooling> {
        let now = (self.clock)();
        let mut book = self.lock();
        self.maintain_locked(&mut book, now);
        let duration = observation_rest(&self.policy, observation)?;
        let revision = next_revision(&mut book);
        let Some(state) = self.entry_for_insert(&mut book, id, now, revision) else {
            return Some(cooling(now, duration, CooldownCause::Observed, 0));
        };
        let applied = apply(
            state,
            cooling(now, duration, CooldownCause::Observed, state.failures),
        );
        drop(book);
        self.emit(id, applied, now);
        Some(applied)
    }

    /// Returns the active cooldown, or `None` when it has expired.
    #[must_use]
    pub fn cooling(&self, id: &MemberId) -> Option<Cooling> {
        let now = (self.clock)();
        let mut book = self.lock();
        self.maintain_locked(&mut book, now);
        let cooling = book
            .states
            .get(id)
            .and_then(|state| state.cooling)
            .filter(|cooling| now < cooling.until);
        if let Some(state) = book.states.get_mut(id) {
            state.touched = now;
        }
        cooling
    }

    /// Returns active cooldowns for a future routing view.
    #[must_use]
    pub fn entries(&self) -> Vec<(MemberId, Cooling)> {
        let now = (self.clock)();
        let mut book = self.lock();
        self.maintain_locked(&mut book, now);
        for state in book.states.values_mut() {
            state.touched = now;
        }
        let mut entries = book
            .states
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

    /// Expires completed rests and prunes idle history beyond `rate_forget`.
    pub fn maintain(&self) {
        let now = (self.clock)();
        let mut book = self.lock();
        self.maintain_locked(&mut book, now);
    }

    /// Removes states for members absent from a successfully built candidate.
    ///
    /// Removed members drop active rests without a recovery event.
    pub fn retain_members(&self, members: &HashSet<MemberId>) {
        let now = (self.clock)();
        let mut book = self.lock();
        self.maintain_locked(&mut book, now);
        book.states.retain(|id, _| members.contains(id));
    }

    /// Removes one member after its provider identity changes.
    ///
    /// The changed identity drops an active rest without a recovery event.
    pub fn remove(&self, id: &MemberId) {
        let now = (self.clock)();
        let mut book = self.lock();
        self.maintain_locked(&mut book, now);
        book.states.remove(id);
    }

    /// Drains pending one-per-rest recovery events, oldest first.
    ///
    /// The queue retains at most 1,024 events and drops the oldest on overflow.
    pub fn drain_recoveries(&self) -> Vec<(MemberId, CooldownCause)> {
        self.lock().recoveries.drain(..).collect()
    }

    fn apply_failure(
        &self,
        state: &mut State,
        failure: Failure,
        now: Instant,
        error: &PluginError,
    ) -> Cooling {
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
        state.idle_since = None;
        if matches!(failure, Failure::RateLimit) {
            state.rate_until = Some(applied.until);
        }
        applied
    }

    fn entry_for_insert<'a>(
        &self,
        book: &'a mut Book,
        id: &MemberId,
        now: Instant,
        revision: u64,
    ) -> Option<&'a mut State> {
        if book.states.contains_key(id) {
            let state = book.states.get_mut(id).expect("existing state");
            state.revision = revision;
            state.touched = now;
            return Some(state);
        }
        if book.states.len() >= self.policy.max_entries {
            let oldest = book
                .states
                .iter()
                .filter(|(_, state)| !state.active(now))
                .min_by_key(|(_, state)| state.touched)
                .map(|(id, _)| id.clone());
            let oldest = oldest?;
            book.states.remove(&oldest);
        }
        book.states.insert(id.clone(), State::new(now, revision));
        book.states.get_mut(id)
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
        let resting = state.active(now);
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

    fn maintain_locked(&self, book: &mut Book, now: Instant) {
        let mut recovered = Vec::new();
        for (id, state) in &mut book.states {
            if let Some(cooling) = state.cooling
                && now >= cooling.until
            {
                state.cooling = None;
                state.idle_since = Some(cooling.until);
                recovered.push((id.clone(), cooling.cause));
            }
            if state.rate_until.is_some_and(|until| {
                now.saturating_duration_since(until) >= self.policy.rate_forget
            }) {
                state.rate_until = None;
                state.rate_failures = 0;
            }
        }
        for (id, cause) in recovered {
            recover(book, id, cause);
        }
        let forget = self.policy.rate_forget;
        book.states.retain(|_, state| {
            if state.active(now) {
                return true;
            }
            let history =
                state.failures > 0 || state.rate_failures > 0 || state.rate_until.is_some();
            history
                && state
                    .idle_since
                    .is_none_or(|since| now.saturating_duration_since(since) < forget)
        });
    }

    fn lock(&self) -> MutexGuard<'_, Book> {
        self.book
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

fn failure_kind(error: &PluginError) -> Option<Failure> {
    match error.kind() {
        PluginErrorKind::Connect
        | PluginErrorKind::Timeout
        | PluginErrorKind::Transport
        | PluginErrorKind::Interrupted
        | PluginErrorKind::Decode
        | PluginErrorKind::Io
        | PluginErrorKind::Auth => Some(Failure::Generic(CooldownCause::Failure)),
        PluginErrorKind::Status => match error.status() {
            Some(429) => Some(Failure::RateLimit),
            Some(402) => Some(Failure::Credit),
            Some(401 | 403 | 408) | Some(500..=u16::MAX) => {
                Some(Failure::Generic(CooldownCause::Failure))
            }
            _ => None,
        },
        PluginErrorKind::Quota => Some(Failure::Quota),
        PluginErrorKind::Unsupported
        | PluginErrorKind::Config
        | PluginErrorKind::Cancelled
        | PluginErrorKind::Internal => None,
        _ => None,
    }
}

fn next_revision(book: &mut Book) -> u64 {
    let revision = book.next_revision;
    book.next_revision = book
        .next_revision
        .checked_add(1)
        .expect("cooldown revision space exhausted");
    revision
}

fn recover(book: &mut Book, id: MemberId, cause: CooldownCause) {
    if book.recoveries.len() == RECOVERY_CAPACITY {
        book.recoveries.pop_front();
    }
    book.recoveries.push_back((id, cause));
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

fn observation_rest(policy: &CooldownPolicy, observation: &QuotaObservation) -> Option<Duration> {
    if observation.spending_credits() {
        return None;
    }
    let remaining = observation.remaining()?;
    if remaining == 0 {
        return Some(
            if let Some(after) = positive_hint(observation.window_reset()) {
                after.min(policy.quota_max)
            } else if let Some(after) = positive_hint(observation.retry_after()) {
                after.min(policy.retry_after_max)
            } else {
                policy.quota_default
            },
        );
    }
    let limit = observation.limit()?;
    if limit == 0 || remaining > limit {
        return None;
    }
    let used = u128::from(limit) - u128::from(remaining);
    if used * 100 < u128::from(limit) * 98 {
        return None;
    }
    positive_hint(observation.window_reset()).map(|after| after.min(policy.quota_max))
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

fn u128_as_u64(value: u128) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::CooldownCause;

    #[test]
    fn all_lists_every_cooldown_cause() {
        let indices = CooldownCause::ALL
            .into_iter()
            .map(|cause| match cause {
                CooldownCause::Failure => 0,
                CooldownCause::RateLimit => 1,
                CooldownCause::Quota => 2,
                CooldownCause::Credit => 3,
                CooldownCause::Observed => 4,
                CooldownCause::Panic => 5,
            })
            .collect::<Vec<_>>();
        assert_eq!(indices, (0..CooldownCause::ALL.len()).collect::<Vec<_>>());
    }
}
