//! Member cooldown state shared across immutable router snapshots.
//!
//! # Default policy
//!
//! | Event | Cooldown and ladder |
//! | --- | --- |
//! | Generic failure | Starts at 1 minute and doubles after an ended generic rest, capped at 10 minutes. A failure during an active generic rest keeps its rung. Connection, timeout, transport, interrupted-body, decode, I/O, and authentication errors use this ladder, as do HTTP 401, 403, 408, and 5xx statuses. An unserved model uses this ladder on its own key. Other model failures also count on both keys and move to the model ladder when its count passes the member's, so sibling answers do not restart it. |
//! | HTTP 429 | Uses the larger of the rate ladder and the retry hint, with the hint capped at 1 hour. The rate ladder starts at 1 minute, doubles for a new 429 within 30 minutes after the previous rate rest ended, and is capped at 30 minutes. A 429 during an active rest does not advance the ladder. A refusal saying the requested model's credentials are cooling uses a model-local ladder. |
//! | Exhausted quota | Uses the header retry hint capped at 1 hour, or 15 minutes without a positive hint. Model-cooling wording scopes the rest to that model. |
//! | API-key credit refusal | Climbs 30 minutes, 2 hours, 8 hours, 24 hours, 3 days, then 7 days after each ended credit rest, repeating 7 days. An active rest keeps its rung. A named reset wins and is capped at 35 days. A retry hint or soon/smaller-request wording stays flat at 30 minutes and off the ladder. Model-cooling wording keys the ladder by model. Success and manual removal restart the ladder. |
//! | Windowed-account credit refusal | Uses a named body or header reset, the latest full-window reset, a retry hint longer than 5 hours, then 5 hours, capped at 35 days. A renewed window lifts a rest whose deadline was not vendor-named; a vendor-timed rest lifts only when a matching window's reset elapsed or moved to a later cycle. Failed and empty readings lift nothing. Other subscription accounts stay flat at 30 minutes. |
//! | Passive observation | Zero remaining uses the window reset capped at 8 days, the retry hint capped at 1 hour, or 15 minutes without either. Positive remaining at least 98% used with a known positive limit rests only for a positive window reset capped at 8 days. Accounts spending credits and all other observations record nothing. Observations do not advance either failure ladder. |
//! | Contained panic | Uses the generic failure ladder with [`CooldownCause::Panic`]. |
//!
//! Cancelled, unsupported, configuration, internal, unknown future, and other
//! request-shaped HTTP 4xx errors do not cool a member. A new failure, panic,
//! or passive observation keeps whichever rest ends later, so a shorter one
//! never cuts a longer rest short; a deadline tie replaces it. A quota or
//! credit rest guessed without a vendor time gives way to a same-cause
//! failure whose time the vendor named, though it is sooner. Idle
//! ladder history is forgotten after `rate_forget`; active rests never expire
//! as idle state. The book holds at most `max_entries` member/model states and
//! evicts expired entries, then the least-recently-touched idle entry, on
//! insertion.
//!
//! Routing keeps ready members first in configured order, then keeps only the
//! cooling member/model pair whose rest ends soonest as a probe. Configured
//! order breaks a tie, other cooling pairs are skipped for that request, and a
//! route with one member always tries that member. If every member is cooling
//! for the requested model, only the soonest probe is tried.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use hofund_plugin_api::{MemberId, PluginError, PluginErrorKind, QuotaObservation, QuotaWindow};
use serde::{Deserialize, Serialize};

const RECOVERY_CAPACITY: usize = 1_024;
const CREDIT_RESET_MAX: Duration = Duration::from_secs(35 * 24 * 60 * 60);
const WINDOWED_CREDIT_DEFAULT: Duration = Duration::from_secs(5 * 60 * 60);
const CREDIT_LADDER: [Duration; 6] = [
    Duration::from_secs(30 * 60),
    Duration::from_secs(2 * 60 * 60),
    Duration::from_secs(8 * 60 * 60),
    Duration::from_secs(24 * 60 * 60),
    Duration::from_secs(3 * 24 * 60 * 60),
    Duration::from_secs(7 * 24 * 60 * 60),
];

/// One API-key credit-ladder rest stored by the daemon.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedRest {
    /// Stable member id in its display form.
    pub member: String,
    /// Model whose credit ladder rests alone, or `None` for the whole member.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Wall-clock end of the rest, including an already ended rest.
    pub until: SystemTime,
    /// One-based credit-ladder rung.
    pub rung: u32,
}

impl PersistedRest {
    /// Creates one member-wide rest for persistence or restoration.
    #[must_use]
    pub fn new(member: String, until: SystemTime, rung: u32) -> Self {
        Self {
            member,
            model: None,
            until,
            rung,
        }
    }

    /// Creates one model-scoped rest for persistence or restoration.
    #[must_use]
    pub fn for_model(member: String, model: String, until: SystemTime, rung: u32) -> Self {
        Self {
            member,
            model: Some(model),
            until,
            rung,
        }
    }
}

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
    /// Maximum HTTP retry hint for rate-limit and quota failures.
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
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Cooling {
    until: Instant,
    cause: CooldownCause,
    failures: u32,
    status: Option<u16>,
    error: String,
    background_probe: bool,
    /// Whether this quota or credit rest is a guess: the refusal named no
    /// time (reference: internal/gateway/routing.go:1506-1515, `askAt`).
    guessed: bool,
    /// Whether the refusal itself named this rest's deadline.
    vendor_timed: bool,
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

    /// Upstream status that caused the cooldown, when there was one.
    #[must_use]
    pub const fn status(&self) -> Option<u16> {
        self.status
    }

    /// Upstream words, capped at 300 characters.
    #[must_use]
    pub fn error(&self) -> &str {
        &self.error
    }

    /// Whether a runtime model-list probe may lift this member-wide rest.
    #[must_use]
    pub const fn background_probe(&self) -> bool {
        self.background_probe
    }
}

struct State {
    cooling: Option<Cooling>,
    failures: u32,
    rate_failures: u32,
    rate_until: Option<Instant>,
    credit_rung: u32,
    credit_until: Option<Instant>,
    credit_ladder: bool,
    /// Cause of the most recent rest kept; it outlives the rest's expiry.
    last_cause: Option<CooldownCause>,
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
            credit_rung: 0,
            credit_until: None,
            credit_ladder: false,
            last_cause: None,
            idle_since: None,
            touched: now,
            revision,
        }
    }

    fn active(&self, now: Instant) -> bool {
        self.cooling
            .as_ref()
            .is_some_and(|cooling| now < cooling.until)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CooldownKey {
    member: MemberId,
    model: Option<String>,
}

impl CooldownKey {
    fn new(member: &MemberId, model: Option<&str>) -> Self {
        Self {
            member: member.clone(),
            model: model.map(str::to_owned),
        }
    }
}

struct Book {
    states: HashMap<CooldownKey, State>,
    windowed_credit: HashSet<MemberId>,
    credit_windows: HashMap<MemberId, CreditWindow>,
    probe_lifted: HashMap<MemberId, Instant>,
    probe_missed: HashSet<MemberId>,
    next_revision: u64,
    recoveries: VecDeque<(CooldownKey, CooldownCause)>,
}

/// Thread-safe cooldown state keyed by stable member id and optional model.
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
                windowed_credit: HashSet::new(),
                credit_windows: HashMap::new(),
                probe_lifted: HashMap::new(),
                probe_missed: HashSet::new(),
                next_revision: 1,
                recoveries: VecDeque::new(),
            }),
        }
    }

    /// Enables full-window credit handling for one subscription account.
    pub fn configure_credit_window(&self, id: &MemberId) {
        self.lock().windowed_credit.insert(id.clone());
    }

    /// Records a successful account-wide window reading. A renewed full
    /// window lifts a rest whose deadline was not named by the vendor. A
    /// vendor-timed rest lifts only when a matching window's reset elapsed
    /// or moved to a later cycle. A rate or generic rest stays.
    pub fn observe_credit_window(&self, id: &MemberId, windows: &[QuotaWindow]) {
        let now = (self.clock)();
        let wall = SystemTime::now();
        let mut book = self.lock();
        self.maintain_locked(&mut book, now);
        if !book.windowed_credit.contains(id) || windows.is_empty() {
            return;
        }
        let mut read = HashMap::new();
        let mut full = HashMap::new();
        let mut reset = None;
        let mut unknown = false;
        for window in windows.iter().filter(|window| {
            window.blocking() && window.scope_model().is_none() && window.limit().is_some()
        }) {
            let until = window
                .resets_at()
                .and_then(|reset| reset.duration_since(wall).ok())
                .filter(|after| !after.is_zero())
                .map(|after| saturating_deadline(now, after));
            read.insert(window.name().to_owned(), until);
            if window.limit().is_some_and(|limit| window.used() >= limit) {
                if let Some(until) = until {
                    full.insert(window.name().to_owned(), until);
                    reset = Some(reset.map_or(until, |current: Instant| current.max(until)));
                }
                unknown |= window.resets_at().is_none();
            }
        }
        let current = CreditWindow {
            until: reset,
            unknown,
            full,
        };
        let (renewed, started_again) =
            book.credit_windows
                .get(id)
                .map_or((false, false), |previous| {
                    (
                        !read.is_empty() && previous.renews_later_than(&current),
                        previous.started_again(&read, now),
                    )
                });
        book.credit_windows.insert(id.clone(), current);
        let key = CooldownKey::new(id, None);
        let lifting = book
            .states
            .get(&key)
            .and_then(|state| state.cooling.as_ref())
            .filter(|cooling| {
                (cooling.vendor_timed && started_again || !cooling.vendor_timed && renewed)
                    && matches!(
                        cooling.cause,
                        CooldownCause::Credit | CooldownCause::Quota | CooldownCause::Observed
                    )
                    && now < cooling.until
            })
            .map(|cooling| cooling.cause);
        if let Some(cause) = lifting {
            let revision = next_revision(&mut book);
            let state = book.states.get_mut(&key).expect("existing credit state");
            state.cooling = None;
            state.credit_rung = 0;
            state.credit_until = None;
            state.credit_ladder = false;
            state.last_cause = None;
            state.idle_since = Some(now);
            state.touched = now;
            state.revision = revision;
            recover(&mut book, key, cause);
        }
    }

    /// Returns API-key credit-ladder rests, including ended rests retained
    /// for their rung, with wall-clock deadlines and optional model scopes.
    #[must_use]
    pub fn credit_snapshot(&self, now_wall: SystemTime) -> Vec<PersistedRest> {
        let now = (self.clock)();
        let mut book = self.lock();
        self.maintain_locked(&mut book, now);
        let mut rests = book
            .states
            .iter()
            .filter(|(key, state)| {
                key.member.subscription().is_none()
                    && state.last_cause == Some(CooldownCause::Credit)
                    && state.credit_ladder
                    && state.credit_rung > 0
            })
            .filter_map(|(key, state)| {
                let until = state.credit_until?;
                if now.saturating_duration_since(until) > CREDIT_RESET_MAX {
                    return None;
                }
                let until = if until >= now {
                    now_wall.checked_add(until.duration_since(now))
                } else {
                    now_wall.checked_sub(now.duration_since(until))
                }?;
                Some(match &key.model {
                    Some(model) => PersistedRest::for_model(
                        key.member.to_string(),
                        model.clone(),
                        until,
                        state.credit_rung,
                    ),
                    None => PersistedRest::new(key.member.to_string(), until, state.credit_rung),
                })
            })
            .collect::<Vec<_>>();
        rests.sort_by(|left, right| {
            left.member
                .cmp(&right.member)
                .then_with(|| left.model.cmp(&right.model))
        });
        rests
    }

    /// Restores persisted API-key credit rests for configured members.
    /// Existing in-process state for the same member/model key wins.
    pub fn restore_credit(
        &self,
        rests: Vec<PersistedRest>,
        configured: &HashSet<MemberId>,
        now_wall: SystemTime,
    ) {
        let now = (self.clock)();
        let mut book = self.lock();
        self.maintain_locked(&mut book, now);
        let configured = configured
            .iter()
            .map(|id| (id.to_string(), id))
            .collect::<HashMap<_, _>>();
        for rest in rests {
            if rest.rung == 0 {
                continue;
            }
            let rung = rest
                .rung
                .min(u32::try_from(CREDIT_LADDER.len()).unwrap_or(u32::MAX));
            let Some(id) = configured.get(&rest.member) else {
                continue;
            };
            let key = CooldownKey::new(id, rest.model.as_deref());
            if id.subscription().is_some()
                || book.states.contains_key(&key)
                || book.states.len() >= self.policy.max_entries
            {
                continue;
            }
            let (until, active) = match rest.until.duration_since(now_wall) {
                Ok(left) => (saturating_deadline(now, left.min(CREDIT_LADDER[5])), true),
                Err(error) if error.duration() <= CREDIT_RESET_MAX => {
                    (now.checked_sub(error.duration()).unwrap_or(now), false)
                }
                Err(_) => continue,
            };
            let revision = next_revision(&mut book);
            let mut state = State::new(now, revision);
            state.credit_rung = rung;
            state.credit_until = Some(until);
            state.credit_ladder = true;
            state.last_cause = Some(CooldownCause::Credit);
            if active {
                state.cooling = Some(cooling(
                    now,
                    until.saturating_duration_since(now),
                    CooldownCause::Credit,
                    rung,
                ));
            } else {
                state.idle_since = Some(until);
            }
            book.states.insert(key, state);
        }
    }

    /// Records a failed call without a model scope and returns its cooldown.
    ///
    /// Interrupted bodies use the generic failure policy
    /// (`reference: internal/gateway/fallback.go:1347-1355`). At the capacity
    /// bound, a failure that cannot evict an idle entry returns its untracked
    /// cooling decision without inserting it; active rests are never evicted.
    pub fn record_failure(&self, id: &MemberId, error: &PluginError) -> Option<Cooling> {
        self.record_failure_at(id, None, error)
    }

    /// Records a failed call for `model`, narrowing only errors whose wording
    /// or generic-failure ladder belongs to that model.
    pub fn record_failure_for_model(
        &self,
        id: &MemberId,
        model: &str,
        error: &PluginError,
    ) -> Option<Cooling> {
        self.record_failure_at(id, Some(model), error)
    }

    fn record_failure_at(
        &self,
        id: &MemberId,
        model: Option<&str>,
        error: &PluginError,
    ) -> Option<Cooling> {
        let failure = failure_kind(error);
        let now = (self.clock)();
        let mut book = self.lock();
        self.maintain_locked(&mut book, now);
        let failure = failure?;
        note_probe_failure(&mut book, id, now);
        let model = model.filter(|model| !model.is_empty());
        let model_only = model.is_some()
            && (matches!(
                failure,
                Failure::RateLimit | Failure::Quota | Failure::Credit
            ) && crate::decision::model_cooling(error)
                || crate::decision::model_unserved(error));
        if let Some(model) = model
            && matches!(failure, Failure::Generic(_))
            && !model_only
        {
            let applied = self.generic_model(&mut book, id, model, failure, now, error);
            drop(book);
            self.emit_for_model(id, Some(model), &applied, now);
            return Some(applied);
        }
        let key = CooldownKey::new(id, model_only.then_some(model).flatten());
        let background_probe =
            key.model.is_none() && id.subscription().is_none() && probe_error(error);
        let credit = credit_context(&book, id, now);
        let revision = next_revision(&mut book);
        let Some(state) = self.entry_for_insert(&mut book, &key, now, revision) else {
            // A full book of active rests can consume a revision without inserting.
            let mut state = State::new(now, 0);
            return Some(self.apply_failure(
                &mut state,
                failure,
                now,
                error,
                credit,
                background_probe,
            ));
        };
        let applied = self.apply_failure(state, failure, now, error, credit, background_probe);
        drop(book);
        self.emit(&key, &applied, now);
        Some(applied)
    }

    /// Clears every cooldown and ladder state for the member. This is also the
    /// manual-clear operation for all of its model rests.
    pub fn record_success(&self, id: &MemberId) {
        let now = (self.clock)();
        let mut book = self.lock();
        self.maintain_locked(&mut book, now);
        forget_probe_guard(&mut book, id);
        let keys = book
            .states
            .keys()
            .filter(|key| key.member == *id)
            .cloned()
            .collect::<Vec<_>>();
        for key in keys {
            if let Some(state) = book.states.remove(&key)
                && let Some(cooling) = state.cooling
            {
                recover(&mut book, key, cooling.cause);
            }
        }
    }

    /// Whether a real failure has disabled background probes for this member.
    #[must_use]
    pub fn probe_allowed(&self, id: &MemberId) -> bool {
        !self.lock().probe_missed.contains(id)
    }

    /// Lifts the matching generic rest after a model-list answer.
    ///
    /// A later rest with another deadline wins. This is not a real request
    /// success, so it starts the 30-minute miss guard instead of clearing it.
    pub fn lift_probe(&self, id: &MemberId, until: Instant) -> bool {
        let now = (self.clock)();
        let mut book = self.lock();
        self.maintain_locked(&mut book, now);
        let key = CooldownKey::new(id, None);
        let matches = book.states.get(&key).is_some_and(|state| {
            state
                .cooling
                .as_ref()
                .is_some_and(|cooling| cooling.until == until && cooling.background_probe)
        });
        if !matches {
            return false;
        }
        if let Some(state) = book.states.remove(&key)
            && let Some(cooling) = state.cooling
        {
            recover(&mut book, key, cooling.cause);
        }
        book.probe_lifted.insert(id.clone(), now);
        true
    }

    /// Clears only one model's cooldown and ladder state after its success.
    /// A sibling answer also restarts the member's generic-failure count, but
    /// does not lift its active rest or touch its rate and credit ladders.
    pub fn record_success_for_model(&self, id: &MemberId, model: &str) {
        self.clear_scoped(id, Some(model), None);
    }

    /// Captures the member-wide state revision before starting a call.
    ///
    /// Pass the returned revision to [`Self::record_success_if_unchanged`] when
    /// the call succeeds so it cannot erase a newer failure, panic, or
    /// observation. Zero means the member has no member-wide entry.
    #[must_use]
    pub fn revision(&self, id: &MemberId) -> u64 {
        self.scoped_revision(id, None)
    }

    /// Captures the newest member/model revision before starting its call.
    #[must_use]
    pub fn revision_for_model(&self, id: &MemberId, model: &str) -> u64 {
        self.scoped_revision(id, Some(model))
    }

    fn scoped_revision(&self, id: &MemberId, model: Option<&str>) -> u64 {
        let now = (self.clock)();
        let mut book = self.lock();
        self.maintain_locked(&mut book, now);
        let key = CooldownKey::new(id, model);
        let member = CooldownKey::new(id, None);
        let revision = if model.is_some() {
            [&key, &member].into_iter().fold(0, |newest, key| {
                newest.max(book.states.get(key).map_or(0, |state| state.revision))
            })
        } else {
            book.states.get(&key).map_or(0, |state| state.revision)
        };
        for key in std::iter::once(&key).chain(model.is_some().then_some(&member)) {
            if let Some(state) = book.states.get_mut(key) {
                state.touched = now;
            }
        }
        revision
    }

    /// Records member-wide success only if its entry has not changed.
    pub fn record_success_if_unchanged(&self, id: &MemberId, revision: u64) {
        self.clear_scoped(id, None, Some(revision));
    }

    /// Records model success only if that model's entry has not changed.
    pub fn record_success_for_model_if_unchanged(&self, id: &MemberId, model: &str, revision: u64) {
        self.clear_scoped(id, Some(model), Some(revision));
    }

    fn clear_scoped(&self, id: &MemberId, model: Option<&str>, revision: Option<u64>) {
        let now = (self.clock)();
        let mut book = self.lock();
        self.maintain_locked(&mut book, now);
        forget_probe_guard(&mut book, id);
        let key = CooldownKey::new(id, model);
        let member = CooldownKey::new(id, None);
        let stale = match (model, revision) {
            (_, None) => false,
            (None, Some(revision)) => {
                revision == 0
                    || !book
                        .states
                        .get(&key)
                        .is_some_and(|state| state.revision == revision)
            }
            (Some(_), Some(revision)) => {
                revision == 0
                    || [&key, &member].into_iter().any(|key| {
                        book.states
                            .get(key)
                            .is_some_and(|state| state.revision > revision)
                    })
            }
        };
        if stale {
            return;
        }
        if let Some(state) = book.states.remove(&key)
            && let Some(cooling) = state.cooling
        {
            recover(&mut book, key, cooling.cause);
        }
        if model.is_some()
            && let Some(state) = book.states.get_mut(&member)
        {
            state.failures = 0;
        }
    }

    /// Records a contained panic on the generic failure ladder.
    pub fn record_panic(&self, id: &MemberId) -> Cooling {
        let now = (self.clock)();
        let mut book = self.lock();
        self.maintain_locked(&mut book, now);
        let key = CooldownKey::new(id, None);
        let revision = next_revision(&mut book);
        let Some(state) = self.entry_for_insert(&mut book, &key, now, revision) else {
            let mut state = State::new(now, 0);
            return Self::generic(&mut state, now, &self.policy, CooldownCause::Panic);
        };
        let candidate = Self::generic(state, now, &self.policy, CooldownCause::Panic);
        let applied = apply(state, candidate, false);
        drop(book);
        self.emit(&key, &applied, now);
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
        let key = CooldownKey::new(id, None);
        let revision = next_revision(&mut book);
        let Some(state) = self.entry_for_insert(&mut book, &key, now, revision) else {
            return Some(cooling(now, duration, CooldownCause::Observed, 0));
        };
        let applied = apply_observation(
            state,
            cooling(now, duration, CooldownCause::Observed, state.failures),
        );
        drop(book);
        self.emit(&key, &applied, now);
        Some(applied)
    }

    /// Returns the active member-wide cooldown, or `None` when it has expired.
    #[must_use]
    pub fn cooling(&self, id: &MemberId) -> Option<Cooling> {
        self.cooling_at(id, None)
    }

    /// Returns the longer active member-wide or model-specific cooldown.
    #[must_use]
    pub fn cooling_for_model(&self, id: &MemberId, model: &str) -> Option<Cooling> {
        self.cooling_at(id, Some(model))
    }

    fn cooling_at(&self, id: &MemberId, model: Option<&str>) -> Option<Cooling> {
        let now = (self.clock)();
        let mut book = self.lock();
        self.maintain_locked(&mut book, now);
        let member = CooldownKey::new(id, None);
        let specific = model.map(|model| CooldownKey::new(id, Some(model)));
        let mut cooling = None;
        for key in std::iter::once(&member).chain(specific.iter()) {
            if let Some(state) = book.states.get_mut(key) {
                state.touched = now;
                if let Some(candidate) =
                    state.cooling.as_ref().filter(|cooling| now < cooling.until)
                    && cooling
                        .as_ref()
                        .is_none_or(|current: &Cooling| current.until < candidate.until)
                {
                    cooling = Some(candidate.clone());
                }
            }
        }
        cooling
    }

    /// Returns active member-wide cooldowns for a future routing view.
    #[must_use]
    pub fn entries(&self) -> Vec<(MemberId, Cooling)> {
        self.scoped_entries()
            .into_iter()
            .filter_map(|(id, model, cooling)| model.is_none().then_some((id, cooling)))
            .collect()
    }

    /// Returns every active member-wide and model-specific cooldown.
    #[must_use]
    pub fn scoped_entries(&self) -> Vec<(MemberId, Option<String>, Cooling)> {
        let now = (self.clock)();
        let mut book = self.lock();
        self.maintain_locked(&mut book, now);
        for state in book.states.values_mut() {
            state.touched = now;
        }
        let mut entries = book
            .states
            .iter()
            .filter_map(|(key, state)| {
                state
                    .cooling
                    .as_ref()
                    .filter(|cooling| now < cooling.until)
                    .map(|cooling| (key.member.clone(), key.model.clone(), cooling.clone()))
            })
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            left.0
                .to_string()
                .cmp(&right.0.to_string())
                .then_with(|| left.1.cmp(&right.1))
        });
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
        book.states.retain(|key, _| members.contains(&key.member));
        book.windowed_credit.retain(|id| members.contains(id));
        book.credit_windows.retain(|id, _| members.contains(id));
        book.probe_lifted.retain(|id, _| members.contains(id));
        book.probe_missed.retain(|id| members.contains(id));
    }

    /// Removes one member after its provider identity changes.
    ///
    /// The changed identity drops active rests without recovery events.
    pub fn remove(&self, id: &MemberId) {
        let now = (self.clock)();
        let mut book = self.lock();
        self.maintain_locked(&mut book, now);
        book.states.retain(|key, _| key.member != *id);
        book.windowed_credit.remove(id);
        book.credit_windows.remove(id);
        book.probe_lifted.remove(id);
        book.probe_missed.remove(id);
    }

    /// Drains pending recovery events without their model scopes, oldest first.
    /// Callers that route model rests should use [`Self::drain_scoped_recoveries`].
    pub fn drain_recoveries(&self) -> Vec<(MemberId, CooldownCause)> {
        self.drain_scoped_recoveries()
            .into_iter()
            .map(|(id, _, cause)| (id, cause))
            .collect()
    }

    /// Drains pending one-per-rest recovery events with their model scopes.
    /// The queue retains at most 1,024 events and drops the oldest on overflow.
    pub fn drain_scoped_recoveries(&self) -> Vec<(MemberId, Option<String>, CooldownCause)> {
        self.lock()
            .recoveries
            .drain(..)
            .map(|(key, cause)| (key.member, key.model, cause))
            .collect()
    }

    fn generic_model(
        &self,
        book: &mut Book,
        id: &MemberId,
        model: &str,
        failure: Failure,
        now: Instant,
        error: &PluginError,
    ) -> Cooling {
        let member = CooldownKey::new(id, None);
        let specific = CooldownKey::new(id, Some(model));
        let revision = next_revision(book);
        let _ = self.entry_for_insert(book, &member, now, revision);
        let _ = self.entry_for_insert(book, &specific, now, revision);
        let backing_off = [&member, &specific].into_iter().any(|key| {
            book.states.get(key).is_some_and(|state| {
                state.active(now)
                    && matches!(
                        state.last_cause,
                        Some(CooldownCause::Failure | CooldownCause::Panic)
                    )
            })
        });
        if !backing_off {
            for key in [&member, &specific] {
                if let Some(state) = book.states.get_mut(key) {
                    state.failures = state.failures.saturating_add(1);
                }
            }
        }
        let member_failures = book
            .states
            .get(&member)
            .map_or(0, |state| state.failures)
            .max(1);
        let model_failures = book.states.get(&specific).map_or(0, |state| state.failures);
        let (target, failures) = if model_failures > member_failures {
            (specific, model_failures)
        } else {
            (member, member_failures)
        };
        let Failure::Generic(cause) = failure else {
            unreachable!("generic model failure");
        };
        let duration = ladder(self.policy.base, self.policy.generic_max, failures);
        let mut candidate = cooling(now, duration, cause, failures);
        candidate.status = error.status();
        candidate.error = error.message().trim().chars().take(300).collect();
        candidate.background_probe =
            target.model.is_none() && id.subscription().is_none() && probe_error(error);
        if let Some(state) = book.states.get_mut(&target) {
            state.idle_since = None;
            apply(state, candidate, false)
        } else {
            candidate
        }
    }

    fn apply_failure(
        &self,
        state: &mut State,
        failure: Failure,
        now: Instant,
        error: &PluginError,
        credit: CreditContext,
        background_probe: bool,
    ) -> Cooling {
        let mut ladder = None;
        let (mut candidate, named) = match failure {
            Failure::Generic(cause) => (Self::generic(state, now, &self.policy, cause), false),
            Failure::RateLimit => (self.rate_limit(state, now, error.retry_after()), false),
            Failure::Quota => {
                let hint = positive_hint(error.retry_after());
                let duration = hint.map_or(self.policy.quota_default, |after| {
                    after.min(self.policy.retry_after_max)
                });
                let mut candidate = cooling(now, duration, CooldownCause::Quota, state.failures);
                candidate.guessed = hint.is_none();
                candidate.vendor_timed = hint.is_some();
                (candidate, hint.is_some())
            }
            Failure::Credit => {
                let (candidate, named, on_ladder) = self.credit(state, now, error, credit);
                ladder = Some(on_ladder);
                (candidate, named)
            }
        };
        candidate.status = error.status();
        candidate.error = error.message().trim().chars().take(300).collect();
        candidate.background_probe = background_probe;
        let candidate_until = candidate.until;
        let applied = apply(state, candidate, named);
        // The credit ladder moves only with the rest it wrote: a kept rest
        // keeps its rung and run (reference: internal/gateway/routing.go:
        // 1518-1533 leaves the note alone when the rest stands).
        if let Some(on_ladder) = ladder
            && applied.until == candidate_until
        {
            state.credit_rung = applied.failures;
            state.credit_ladder = on_ladder;
        }
        state.idle_since = None;
        if matches!(failure, Failure::RateLimit) {
            state.rate_until = Some(applied.until);
        }
        if matches!(failure, Failure::Credit) && applied.cause == CooldownCause::Credit {
            state.credit_until = Some(applied.until);
        }
        applied
    }

    fn credit(
        &self,
        state: &mut State,
        now: Instant,
        error: &PluginError,
        context: CreditContext,
    ) -> (Cooling, bool, bool) {
        let reset = positive_hint(error.reset_after()).map(|after| after.min(CREDIT_RESET_MAX));
        let retry = positive_hint(error.retry_after());
        // `named`: the vendor gave this time (reset or retry hint); `guessed`:
        // the rest stands in for a time nobody named (reference:
        // internal/gateway/routing.go:241-262, 1264-1296, 1345).
        let (duration, rung, on_ladder, named, guessed, vendor_timed) =
            if context.subscription && !context.windowed {
                (self.policy.credit, 0, false, false, true, false)
            } else if let Some(after) = reset {
                (after, 0, false, true, false, true)
            } else if context.windowed {
                // The window's own time is never a guess. A retry hint chosen
                // here could only replace a same-cause guess made before the
                // window was configured, the flat subscription rest, which is
                // always shorter, so it is not marked named.
                let window_after = context
                    .window_reset
                    .map(|until| until.saturating_duration_since(now))
                    .filter(|after| !after.is_zero());
                let duration = window_after.unwrap_or_else(|| {
                    retry
                        .filter(|after| *after > WINDOWED_CREDIT_DEFAULT)
                        .unwrap_or(WINDOWED_CREDIT_DEFAULT)
                });
                let vendor_timed = window_after.is_none()
                    && retry.is_some_and(|after| after > WINDOWED_CREDIT_DEFAULT);
                (
                    duration.min(CREDIT_RESET_MAX),
                    0,
                    false,
                    false,
                    false,
                    vendor_timed,
                )
            } else if retry.is_some() || error.credit_soon() {
                (self.policy.credit, 0, false, false, true, retry.is_some())
            } else {
                let continuing = state.last_cause == Some(CooldownCause::Credit)
                    && state.credit_ladder
                    && state.credit_rung > 0
                    && state.credit_until.is_some();
                let rung = if continuing {
                    let next = if state.active(now) {
                        state.credit_rung
                    } else {
                        state.credit_rung.saturating_add(1)
                    };
                    next.min(u32::try_from(CREDIT_LADDER.len()).unwrap_or(u32::MAX))
                } else {
                    1
                };
                let index = usize::try_from(rung - 1).unwrap_or(0);
                (CREDIT_LADDER[index], rung, true, false, true, false)
            };
        let mut candidate = cooling(now, duration, CooldownCause::Credit, rung);
        candidate.guessed = guessed;
        candidate.vendor_timed = vendor_timed;
        (candidate, named, on_ladder)
    }

    fn entry_for_insert<'a>(
        &self,
        book: &'a mut Book,
        key: &CooldownKey,
        now: Instant,
        revision: u64,
    ) -> Option<&'a mut State> {
        if book.states.contains_key(key) {
            let state = book.states.get_mut(key).expect("existing state");
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
                .map(|(key, _)| key.clone());
            let oldest = oldest?;
            book.states.remove(&oldest);
        }
        book.states.insert(key.clone(), State::new(now, revision));
        book.states.get_mut(key)
    }

    fn generic(
        state: &mut State,
        now: Instant,
        policy: &CooldownPolicy,
        cause: CooldownCause,
    ) -> Cooling {
        let resting = matches!(
            state.last_cause,
            Some(CooldownCause::Failure | CooldownCause::Panic)
        ) && state.active(now);
        if !resting || state.failures == 0 {
            state.failures = state.failures.saturating_add(1);
        }
        let duration = ladder(policy.base, policy.generic_max, state.failures);
        cooling(now, duration, cause, state.failures)
    }

    fn rate_limit(
        &self,
        state: &mut State,
        now: Instant,
        retry_after: Option<Duration>,
    ) -> Cooling {
        // The ladder continues only when the last rest kept was a rate rest;
        // an intervening rest for another cause restarts it (reference:
        // internal/gateway/routing.go rateRest).
        let previous_rate = state.last_cause == Some(CooldownCause::RateLimit);
        let resting = previous_rate && state.active(now);
        let within_forget = previous_rate
            && state.rate_until.is_some_and(|until| {
                now.saturating_duration_since(until) < self.policy.rate_forget
            });
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
        for (key, state) in &mut book.states {
            if state
                .cooling
                .as_ref()
                .is_some_and(|cooling| now >= cooling.until)
                && let Some(cooling) = state.cooling.take()
            {
                state.idle_since = Some(cooling.until);
                recovered.push((key.clone(), cooling.cause));
            }
            if state.rate_until.is_some_and(|until| {
                now.saturating_duration_since(until) >= self.policy.rate_forget
            }) {
                state.rate_until = None;
                state.rate_failures = 0;
            }
        }
        for (key, cause) in recovered {
            recover(book, key, cause);
        }
        let forget = self.policy.rate_forget;
        book.states.retain(|_, state| {
            if state.active(now) {
                return true;
            }
            let history =
                state.failures > 0 || state.rate_failures > 0 || state.rate_until.is_some();
            let ordinary = history
                && state
                    .idle_since
                    .is_none_or(|since| now.saturating_duration_since(since) < forget);
            ordinary || (state.credit_ladder && state.credit_rung > 0)
        });
    }

    fn lock(&self) -> MutexGuard<'_, Book> {
        self.book
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn emit_for_model(&self, id: &MemberId, model: Option<&str>, cooling: &Cooling, now: Instant) {
        self.emit(&CooldownKey::new(id, model), cooling, now);
    }

    fn emit(&self, key: &CooldownKey, cooling: &Cooling, now: Instant) {
        let duration_ms = u128_as_u64(cooling.until.saturating_duration_since(now).as_millis());
        let cause = cooling.cause.as_str();
        tracing::info!(
            "member.id" = %key.member,
            model = key.model.as_deref().unwrap_or(""),
            cause,
            duration_ms,
            failures = cooling.failures,
            "member_cooldown"
        );
        metrics::counter!("hofund_member_cooldowns", "cause" => cause).increment(1);
    }
}

/// The renewal of an account's full account-wide windows, as last read.
#[derive(Clone)]
struct CreditWindow {
    /// The last known positive reset among the full windows.
    until: Option<Instant>,
    /// Whether a full window gave no reset.
    unknown: bool,
    /// Known positive resets by full window name.
    full: HashMap<String, Instant>,
}

impl CreditWindow {
    /// Whether this reading renews later than `current`: a full window with
    /// no reset renews latest, no full window renews at once, and readings
    /// within a second of each other count as the same.
    fn renews_later_than(&self, current: &Self) -> bool {
        match (self.unknown, current.unknown) {
            (_, true) => false,
            (true, false) => true,
            (false, false) => match (self.until, current.until) {
                (Some(_), None) => true,
                (Some(before), Some(after)) => {
                    before.saturating_duration_since(after) > Duration::from_secs(1)
                }
                (None, _) => false,
            },
        }
    }

    /// Whether a window full at the last good reading stands started again:
    /// its reset has gone by, or the same window now resets in a later
    /// cycle. Resets come as wall times turned into instants, so readings
    /// within a second of each other count as the same reset.
    fn started_again(&self, read: &HashMap<String, Option<Instant>>, now: Instant) -> bool {
        self.full.iter().any(|(name, reset)| {
            read.get(name).is_some_and(|current| {
                *reset <= now
                    || (*current).is_some_and(|current| {
                        current.saturating_duration_since(*reset) > Duration::from_secs(1)
                    })
            })
        })
    }
}

#[derive(Clone, Copy)]
struct CreditContext {
    subscription: bool,
    windowed: bool,
    window_reset: Option<Instant>,
}

fn credit_context(book: &Book, id: &MemberId, now: Instant) -> CreditContext {
    CreditContext {
        subscription: id.subscription().is_some(),
        windowed: book.windowed_credit.contains(id),
        window_reset: book
            .credit_windows
            .get(id)
            .and_then(|window| window.until)
            .filter(|until| *until > now),
    }
}

#[derive(Clone, Copy)]
enum Failure {
    Generic(CooldownCause),
    RateLimit,
    Quota,
    Credit,
}

fn note_probe_failure(book: &mut Book, id: &MemberId, now: Instant) {
    if let Some(lifted) = book.probe_lifted.remove(id)
        && now.saturating_duration_since(lifted) < Duration::from_secs(30 * 60)
    {
        book.probe_missed.insert(id.clone());
    }
}

fn forget_probe_guard(book: &mut Book, id: &MemberId) {
    book.probe_lifted.remove(id);
    book.probe_missed.remove(id);
}

fn probe_error(error: &PluginError) -> bool {
    if error.is_credit()
        || error.kind() == PluginErrorKind::Quota
        || error.status() == Some(429)
        || error.retry_after().is_some()
    {
        return false;
    }
    match error.kind() {
        PluginErrorKind::Connect
        | PluginErrorKind::Timeout
        | PluginErrorKind::Transport
        | PluginErrorKind::Interrupted => true,
        PluginErrorKind::Status => matches!(error.status(), Some(408 | 500..=599)),
        _ => false,
    }
}

fn failure_kind(error: &PluginError) -> Option<Failure> {
    if !error.cools() || !crate::decision::rests(error) {
        return None;
    }
    if error.is_credit() {
        return Some(Failure::Credit);
    }
    if crate::decision::model_unserved(error) {
        return Some(Failure::Generic(CooldownCause::Failure));
    }
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

fn recover(book: &mut Book, key: CooldownKey, cause: CooldownCause) {
    if book.recoveries.len() == RECOVERY_CAPACITY {
        book.recoveries.pop_front();
    }
    book.recoveries.push_back((key, cause));
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
        status: None,
        error: String::new(),
        background_probe: false,
        guessed: false,
        vendor_timed: false,
    }
}

/// Failure events keep whichever rest ends later; a candidate ending at the
/// same instant replaces the current rest, so a shorter failure never
/// shortens a longer rest (ruling R-494b, superseding R-494). One
/// exception: a guessed quota or credit rest gives way to a rest of the same
/// cause whose time the vendor named (`named`), though it is sooner
/// (`reference: internal/gateway/routing.go:1518-1533`).
fn apply(state: &mut State, candidate: Cooling, named: bool) -> Cooling {
    let applied = match &state.cooling {
        Some(current)
            if current.until > candidate.until
                && !(named && current.guessed && current.cause == candidate.cause) =>
        {
            current.clone()
        }
        _ => candidate,
    };
    state.cooling = Some(applied.clone());
    state.last_cause = Some(applied.cause);
    applied
}

/// Passive observations keep whichever rest ends later, with its own cause
/// and failure count. A candidate ending at the same instant replaces it.
fn apply_observation(state: &mut State, candidate: Cooling) -> Cooling {
    let applied = match &state.cooling {
        Some(current) if current.until > candidate.until => current.clone(),
        _ => candidate,
    };
    state.cooling = Some(applied.clone());
    state.last_cause = Some(applied.cause);
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
