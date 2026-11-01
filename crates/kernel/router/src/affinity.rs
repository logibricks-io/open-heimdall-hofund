//! Conversation affinity, cache warmth, and durable answerer state.

pub(crate) mod sealed;

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Tokens read or written that make a cached conversation worth keeping.
pub const CACHE_WORTH: u64 = 1024;
/// Default time an unread prompt cache is considered warm.
pub const CACHE_COLD: Duration = Duration::from_secs(5 * 60);
/// Cache lifetime used by accounts and hosts with a longer measured lifetime.
pub const CACHE_HOUR: Duration = Duration::from_secs(60 * 60);
/// How long an answerer is remembered.
pub const STICK_KEEP: Duration = Duration::from_secs(24 * 60 * 60);
/// Maximum conversations written to disk.
pub const STICKS_KEPT: usize = 512;

/// How long a conversation stays with its last answerer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Stays {
    /// Keep within a turn and across turns while the cache is useful and warm.
    #[default]
    Auto,
    /// Keep for the whole conversation unless the member is unavailable.
    Session,
    /// Keep only within the current turn.
    Turn,
    /// Never apply an existing stick.
    Off,
    /// Keep only the answerer, including through rests and broken replies.
    Strict,
}

impl Stays {
    /// Parses a validated configuration spelling.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "session" => Some(Self::Session),
            "turn" => Some(Self::Turn),
            "off" => Some(Self::Off),
            "strict" => Some(Self::Strict),
            _ => None,
        }
    }

    /// The configuration spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Session => "session",
            Self::Turn => "turn",
            Self::Off => "off",
            Self::Strict => "strict",
        }
    }
}

/// Identity and turn position of one request's conversation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conversation {
    /// Scope, stable conversation id, and optional subagent id.
    pub key: String,
    /// Number of user turns in the conversation.
    pub turn: u64,
    /// Whether tool results or agent-only notes continue the last turn.
    pub within: bool,
    /// Original request body, used by sealed-reasoning recovery.
    pub body: Vec<u8>,
}

impl Conversation {
    /// A conversation at a new user turn.
    #[must_use]
    pub fn new(key: impl Into<String>, turn: u64, within: bool) -> Self {
        Self {
            key: key.into(),
            turn,
            within,
            body: Vec::new(),
        }
    }

    /// Supplies the original request body.
    #[must_use]
    pub fn with_body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }
}

/// Cache counters reported by one answer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheUsage {
    /// Prompt tokens sent. Absent means the upstream reported no usage.
    pub input: Option<u64>,
    /// Tokens read from the prompt cache.
    pub read: u64,
    /// Tokens written to the prompt cache.
    pub write: u64,
    /// Part of `write` stored with a one-hour lifetime.
    pub write_hour: u64,
}

/// A candidate as affinity sees it after strategy ordering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// Stable member id shared across router snapshots.
    pub member: String,
    /// Model this candidate serves.
    pub model: String,
    /// Whether the member/model pair is cooling down.
    pub resting: bool,
    /// Whether its known allowance is nearly spent.
    pub nearly_spent: bool,
    /// How long its unread prompt cache remains warm.
    pub cold_after: Duration,
}

impl Candidate {
    /// An available candidate with the default cache lifetime.
    #[must_use]
    pub fn new(member: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            member: member.into(),
            model: model.into(),
            resting: false,
            nearly_spent: false,
            cold_after: CACHE_COLD,
        }
    }
}

/// The used share of an allowance past which an account counts all but
/// spent (`reference: internal/provider/codex_switch.go`, `SpentShare`).
pub const SPENT_SHARE: f64 = 98.0;

/// The used share past which `routing` counts an account spent: In order
/// tries an account in its turn until a window is used up, so only 100%
/// counts there (`reference: internal/provider/codex_switch.go`,
/// `SpentShareOf`).
#[must_use]
pub fn spent_share_of(routing: &str) -> f64 {
    if routing == "order" {
        100.0
    } else {
        SPENT_SHARE
    }
}

/// Why affinity kept or released a conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// No previous answerer is remembered.
    First,
    /// Affinity is disabled.
    Off,
    /// The answerer is no longer a candidate.
    Gone,
    /// Strict affinity kept the answerer.
    Strict,
    /// Session affinity kept the answerer.
    Session,
    /// The request continues the current turn.
    Turn,
    /// A useful warm cache kept the answerer.
    Cache,
    /// The answerer is cooling down.
    Resting,
    /// The answerer's allowance is nearly spent.
    Spent,
    /// A turn-only or automatic stick does not cross this new turn.
    NewTurn,
    /// The previous reply had too little cache activity.
    NoCache,
    /// The previous reply's cache is too old.
    Cold,
}

impl Reason {
    /// Stable explanation text.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::First => "first",
            Self::Off => "off",
            Self::Gone => "gone",
            Self::Strict => "strict",
            Self::Session => "session",
            Self::Turn => "turn",
            Self::Cache => "cache",
            Self::Resting => "resting",
            Self::Spent => "spent",
            Self::NewTurn => "new-turn",
            Self::NoCache => "no-cache",
            Self::Cold => "cold",
        }
    }
}

/// The affinity decision over ordered candidate indexes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision {
    /// Candidate indexes after affinity, still a permutation of the input.
    pub order: Vec<usize>,
    /// Whether an existing answerer was kept.
    pub kept: bool,
    /// Why the decision was made.
    pub reason: Reason,
    /// Whether failures after a successful reply must preserve the stick.
    pub strict: bool,
}

/// One remembered answerer and its latest cache facts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stick {
    /// Stable member id.
    pub member: String,
    /// Model that answered.
    pub model: String,
    /// User-turn number that answered.
    pub turn: u64,
    /// When the answer completed.
    pub at: SystemTime,
    /// Cache tokens read by the answer.
    pub cache_read: u64,
    /// Cache tokens written, or sent when a cache write was silent.
    pub cache_write: u64,
    /// Tokens retained for an hour.
    pub hour_tokens: u64,
    /// When the one-hour cache was last written or fully read.
    pub hour_at: Option<SystemTime>,
}

#[derive(Default)]
struct State {
    sticks: BTreeMap<String, Stick>,
    dirty: bool,
    scheduled: bool,
}

/// Explicit affinity state shared by related router snapshots.
#[derive(Clone)]
pub struct AffinityState {
    inner: Arc<AffinityInner>,
}

#[doc(hidden)]
pub struct AffinityInner {
    state: Mutex<State>,
    saving: Mutex<()>,
    path: Option<PathBuf>,
    clock: Arc<dyn Fn() -> SystemTime + Send + Sync>,
    debounce: Duration,
    sealed: sealed::SealedState,
}

impl std::ops::Deref for AffinityState {
    type Target = AffinityInner;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl AffinityState {
    /// Loads `affinity.json` beside the configuration file.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self::with_clock(path, Arc::new(SystemTime::now), Duration::from_secs(2))
    }

    /// Empty state that never touches the filesystem.
    #[must_use]
    pub fn in_memory() -> Self {
        Self {
            inner: Arc::new(AffinityInner {
                state: Mutex::new(State::default()),
                saving: Mutex::new(()),
                path: None,
                clock: Arc::new(SystemTime::now),
                debounce: Duration::ZERO,
                sealed: sealed::SealedState::default(),
            }),
        }
    }

    /// State with an injected clock and debounce, for deterministic tests.
    #[must_use]
    pub fn with_clock(
        path: impl Into<PathBuf>,
        clock: Arc<dyn Fn() -> SystemTime + Send + Sync>,
        debounce: Duration,
    ) -> Self {
        let path = path.into();
        let now = clock();
        let mut state = State::default();
        if let Ok(bytes) = std::fs::read(&path)
            && let Ok(saved) = serde_json::from_slice::<BTreeMap<String, SavedStick>>(&bytes)
        {
            for (key, stick) in saved {
                let Some(stick) = stick.into_stick() else {
                    continue;
                };
                if fresh(stick.at, now) && !stick.member.is_empty() {
                    state.sticks.insert(key, stick);
                }
            }
        }
        Self {
            inner: Arc::new(AffinityInner {
                state: Mutex::new(state),
                saving: Mutex::new(()),
                path: Some(path),
                clock,
                debounce,
                sealed: sealed::SealedState::default(),
            }),
        }
    }

    pub(crate) fn sealed(&self) -> sealed::SealedState {
        self.sealed.clone()
    }

    /// Applies affinity to an ordered candidate list.
    #[must_use]
    pub fn order(
        &self,
        conversation: &Conversation,
        mode: Stays,
        routing: &str,
        candidates: &[Candidate],
    ) -> Decision {
        let now = (self.clock)();
        let stick = self
            .lock()
            .sticks
            .get(&conversation.key)
            .filter(|stick| fresh(stick.at, now))
            .cloned();
        let at = stick.as_ref().and_then(|stick| {
            candidates
                .iter()
                .position(|candidate| {
                    candidate.member == stick.member && candidate.model == stick.model
                })
                .or_else(|| {
                    candidates
                        .iter()
                        .position(|candidate| candidate.member == stick.member)
                })
        });
        let mut order: Vec<usize> = (0..candidates.len()).collect();
        let mut strict = false;
        let reason = if mode == Stays::Off {
            Reason::Off
        } else if stick.is_none() {
            Reason::First
        } else if at.is_none() {
            Reason::Gone
        } else if mode == Stays::Strict {
            strict = true;
            Reason::Strict
        } else if let Some(index) = at {
            let candidate = &candidates[index];
            let stick = stick.as_ref().expect("stick exists when candidate matches");
            if candidate.resting {
                Reason::Resting
            } else if candidate.nearly_spent {
                Reason::Spent
            } else if mode == Stays::Session {
                Reason::Session
            } else if conversation.within {
                Reason::Turn
            } else if mode == Stays::Turn {
                Reason::NewTurn
            } else if stick.cache_read.saturating_add(stick.cache_write) < CACHE_WORTH {
                Reason::NoCache
            } else if !warm(stick, candidate.cold_after, now) {
                Reason::Cold
            } else {
                Reason::Cache
            }
        } else {
            Reason::Gone
        };
        let kept = matches!(
            reason,
            Reason::Strict | Reason::Session | Reason::Turn | Reason::Cache
        );
        if kept && let Some(index) = at.filter(|index| *index > 0) {
            let selected = order.remove(index);
            order.insert(0, selected);
        }
        if strict && let Some(index) = at {
            let member = &candidates[index].member;
            order.retain(|candidate| candidates[*candidate].member == *member);
        }
        let order = later_rules(order, stick.as_ref(), routing);
        Decision {
            order,
            kept,
            reason,
            strict,
        }
    }

    /// Remembers the answerer and its cache usage.
    pub fn answered(&self, key: &str, member: &str, model: &str, turn: u64, usage: CacheUsage) {
        let now = (self.clock)();
        let cache_write = if usage.read == 0 && usage.write == 0 {
            usage.input.unwrap_or(0)
        } else {
            usage.write
        };
        let mut state = self.lock();
        let previous = state.sticks.get(key).filter(|stick| {
            stick.member == member && stick.model == model && stick.hour_at.is_some()
        });
        let (hour_tokens, hour_at) = if usage.write_hour > 0 {
            (usage.read.saturating_add(usage.write_hour), Some(now))
        } else if let Some(previous) = previous.filter(|previous| {
            previous.hour_tokens > 0
                && previous
                    .hour_at
                    .is_some_and(|at| age(at, now) <= CACHE_HOUR)
                && usage.read >= previous.hour_tokens
        }) {
            (previous.hour_tokens, Some(now))
        } else {
            (0, None)
        };
        state.sticks.insert(
            key.to_owned(),
            Stick {
                member: member.to_owned(),
                model: model.to_owned(),
                turn,
                at: now,
                cache_read: usage.read,
                cache_write,
                hour_tokens,
                hour_at,
            },
        );
        if state.sticks.len() > 4096 {
            state.sticks.retain(|_, stick| fresh(stick.at, now));
        }
        drop(state);
        self.note();
    }

    /// Forgets a broken answer except under strict affinity.
    pub fn unanswered(&self, key: &str, member: &str, strict: bool) {
        if strict {
            return;
        }
        let removed = {
            let mut state = self.lock();
            if state
                .sticks
                .get(key)
                .is_some_and(|stick| stick.member == member)
            {
                state.sticks.remove(key);
                true
            } else {
                false
            }
        };
        if removed {
            self.note();
        }
    }

    /// Returns one current stick, for the next affinity phase and tests.
    #[must_use]
    pub fn stick(&self, key: &str) -> Option<Stick> {
        let now = (self.clock)();
        self.lock()
            .sticks
            .get(key)
            .filter(|stick| fresh(stick.at, now))
            .cloned()
    }

    /// Flushes pending state to disk. Safe to call during shutdown.
    pub fn flush(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let _saving = self.saving.lock().expect("affinity save");
        let saved = {
            let mut state = self.lock();
            if !state.dirty {
                state.scheduled = false;
                return;
            }
            let now = (self.clock)();
            let mut entries = state
                .sticks
                .iter()
                .filter(|(_, stick)| fresh(stick.at, now))
                .map(|(key, stick)| (key.clone(), SavedStick::from_stick(stick)))
                .collect::<Vec<_>>();
            entries.sort_by_key(|entry| std::cmp::Reverse(entry.1.at_ms));
            entries.truncate(STICKS_KEPT);
            state.dirty = false;
            state.scheduled = false;
            entries.into_iter().collect::<BTreeMap<_, _>>()
        };
        if let Err(error) = write_atomic(path, &saved) {
            tracing::warn!(path = %path.display(), %error, "could not save affinity state");
            self.lock().dirty = true;
        }
    }

    fn note(&self) {
        if self.path.is_none() {
            return;
        }
        let spawn = {
            let mut state = self.lock();
            state.dirty = true;
            if state.scheduled {
                false
            } else {
                state.scheduled = true;
                true
            }
        };
        if !spawn {
            return;
        }
        if self.debounce.is_zero() {
            self.flush();
            return;
        }
        let this = self.clone();
        std::thread::spawn(move || {
            std::thread::sleep(this.debounce);
            this.flush();
        });
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("affinity state")
    }
}

fn later_rules(order: Vec<usize>, stick: Option<&Stick>, routing: &str) -> Vec<usize> {
    let _ = (stick, routing);
    order
}

fn warm(stick: &Stick, cold_after: Duration, now: SystemTime) -> bool {
    age(stick.at, now) <= cold_after
        || stick.hour_tokens >= CACHE_WORTH
            && stick.hour_at.is_some_and(|at| age(at, now) <= CACHE_HOUR)
}

fn fresh(at: SystemTime, now: SystemTime) -> bool {
    age(at, now) <= STICK_KEEP
}

fn age(at: SystemTime, now: SystemTime) -> Duration {
    now.duration_since(at).unwrap_or(Duration::ZERO)
}

#[derive(Serialize, Deserialize)]
struct SavedStick {
    member: String,
    model: String,
    #[serde(default)]
    turn: u64,
    at_ms: u64,
    #[serde(default)]
    cache_read: u64,
    #[serde(default)]
    cache_write: u64,
    #[serde(default)]
    cache_hour: u64,
    #[serde(default)]
    cache_hour_at_ms: Option<u64>,
}

impl SavedStick {
    fn from_stick(stick: &Stick) -> Self {
        Self {
            member: stick.member.clone(),
            model: stick.model.clone(),
            turn: stick.turn,
            at_ms: millis(stick.at),
            cache_read: stick.cache_read,
            cache_write: stick.cache_write,
            cache_hour: stick.hour_tokens,
            cache_hour_at_ms: stick.hour_at.map(millis),
        }
    }

    fn into_stick(self) -> Option<Stick> {
        Some(Stick {
            member: self.member,
            model: self.model,
            turn: self.turn,
            at: from_millis(self.at_ms)?,
            cache_read: self.cache_read,
            cache_write: self.cache_write,
            hour_tokens: self.cache_hour,
            hour_at: self.cache_hour_at_ms.and_then(from_millis),
        })
    }
}

fn millis(at: SystemTime) -> u64 {
    u64::try_from(
        at.duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

fn from_millis(value: u64) -> Option<SystemTime> {
    UNIX_EPOCH.checked_add(Duration::from_millis(value))
}

fn write_atomic(path: &Path, saved: &BTreeMap<String, SavedStick>) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(saved)?;
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.hofund-tmp");
    {
        let mut options = std::fs::OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&temporary, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    use super::{
        AffinityState, CACHE_COLD, CACHE_HOUR, CacheUsage, Candidate, Conversation, Reason,
        STICK_KEEP, STICKS_KEPT, Stays,
    };

    fn conversation() -> Conversation {
        Conversation::new("demo|session", 2, false)
    }

    fn candidates() -> Vec<Candidate> {
        vec![
            Candidate::new("first", "model"),
            Candidate::new("second", "model"),
        ]
    }

    fn usage(read: u64, write: u64) -> CacheUsage {
        CacheUsage {
            input: Some(read + write),
            read,
            write,
            write_hour: 0,
        }
    }

    #[test]
    fn c2a_auto_keeps_turns_and_warm_cache_but_not_cold_or_small_cache() {
        let current = Arc::new(std::sync::Mutex::new(SystemTime::now()));
        let clock = {
            let current = Arc::clone(&current);
            Arc::new(move || *current.lock().expect("clock"))
        };
        let state = AffinityState::with_clock(
            std::env::temp_dir().join(format!("unused-affinity-{}", std::process::id())),
            clock,
            Duration::ZERO,
        );
        state.answered("demo|session", "second", "model", 1, usage(0, 3000));
        let decision = state.order(&conversation(), Stays::Auto, "smart", &candidates());
        assert_eq!(decision.order, [1, 0]);
        assert_eq!(decision.reason, Reason::Cache);
        assert!(decision.kept);

        let within = Conversation::new("demo|session", 1, true);
        assert_eq!(
            state
                .order(&within, Stays::Turn, "smart", &candidates())
                .reason,
            Reason::Turn
        );
        assert_eq!(
            state
                .order(&conversation(), Stays::Turn, "smart", &candidates())
                .reason,
            Reason::NewTurn
        );
        assert_eq!(
            state
                .order(&conversation(), Stays::Off, "smart", &candidates())
                .reason,
            Reason::Off
        );

        state.answered("demo|small", "second", "model", 1, usage(300, 300));
        assert_eq!(
            state
                .order(
                    &Conversation::new("demo|small", 2, false),
                    Stays::Auto,
                    "smart",
                    &candidates()
                )
                .reason,
            Reason::NoCache
        );
        *current.lock().expect("clock") += CACHE_COLD + Duration::from_secs(1);
        assert_eq!(
            state
                .order(&conversation(), Stays::Auto, "smart", &candidates())
                .reason,
            Reason::Cold
        );
        assert_eq!(
            state
                .order(&conversation(), Stays::Session, "smart", &candidates())
                .reason,
            Reason::Session
        );
    }

    #[test]
    fn c2b_silent_cache_prompt_counts_as_written_and_hour_cache_survives_cold() {
        let current = Arc::new(std::sync::Mutex::new(SystemTime::now()));
        let now = *current.lock().expect("clock");
        let clock = {
            let current = Arc::clone(&current);
            Arc::new(move || *current.lock().expect("clock"))
        };
        let state = AffinityState::with_clock(
            std::env::temp_dir().join(format!("unused-hour-affinity-{}", std::process::id())),
            clock,
            Duration::ZERO,
        );
        state.answered(
            "demo|session",
            "second",
            "model",
            1,
            CacheUsage {
                input: Some(3000),
                read: 0,
                write: 0,
                write_hour: 0,
            },
        );
        assert_eq!(
            state.stick("demo|session").expect("stick").cache_write,
            3000
        );
        state.answered(
            "demo|hour",
            "second",
            "model",
            1,
            CacheUsage {
                input: Some(5000),
                read: 2000,
                write: 3000,
                write_hour: 3000,
            },
        );
        let stick = state.stick("demo|hour").expect("hour");
        assert_eq!(stick.hour_tokens, 5000);
        assert_eq!(stick.hour_at, Some(now));
        assert_eq!(
            stick.hour_at.map(|at| at + CACHE_HOUR),
            Some(now + CACHE_HOUR)
        );
        let hour = Conversation::new("demo|hour", 2, false);
        *current.lock().expect("clock") += Duration::from_secs(20 * 60);
        assert_eq!(
            state
                .order(&hour, Stays::Auto, "smart", &candidates())
                .reason,
            Reason::Cache
        );
        *current.lock().expect("clock") = now + CACHE_HOUR + Duration::from_secs(1);
        assert_eq!(
            state
                .order(&hour, Stays::Auto, "smart", &candidates())
                .reason,
            Reason::Cold
        );
    }

    #[test]
    fn c2g_spent_share_is_98_but_100_in_order() {
        assert!((super::spent_share_of("smart") - 98.0).abs() < f64::EPSILON);
        assert!((super::spent_share_of("rotate") - 98.0).abs() < f64::EPSILON);
        assert!((super::spent_share_of("order") - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn c2c_resting_and_nearly_spent_members_move_except_when_alone() {
        let state = AffinityState::in_memory();
        state.answered("demo|session", "second", "model", 1, usage(0, 3000));
        let mut candidates = candidates();
        candidates[1].resting = true;
        assert_eq!(
            state
                .order(&conversation(), Stays::Session, "smart", &candidates)
                .reason,
            Reason::Resting
        );
        candidates[1].resting = false;
        candidates[1].nearly_spent = true;
        assert_eq!(
            state
                .order(&conversation(), Stays::Auto, "smart", &candidates)
                .reason,
            Reason::Spent
        );
        assert_eq!(
            state
                .order(&conversation(), Stays::Auto, "smart", &candidates[1..])
                .reason,
            Reason::Cache
        );
    }

    #[test]
    fn c3a_strict_keeps_resting_spent_and_broken_answerer_only() {
        let state = AffinityState::in_memory();
        state.answered("demo|session", "second", "model", 1, usage(0, 3000));
        let mut candidates = candidates();
        candidates[1].resting = true;
        candidates[1].nearly_spent = true;
        let decision = state.order(&conversation(), Stays::Strict, "smart", &candidates);
        assert_eq!(decision.order, [1]);
        assert_eq!(decision.reason, Reason::Strict);
        assert!(decision.strict);
        state.unanswered("demo|session", "second", true);
        assert!(state.stick("demo|session").is_some());
        state.unanswered("demo|session", "second", false);
        assert!(state.stick("demo|session").is_none());
    }

    #[test]
    fn c3b_removed_member_moves_as_session_would() {
        let state = AffinityState::in_memory();
        state.answered("demo|session", "gone", "model", 1, usage(0, 3000));
        let decision = state.order(&conversation(), Stays::Strict, "smart", &candidates());
        assert_eq!(decision.order, [0, 1]);
        assert_eq!(decision.reason, Reason::Gone);
        assert!(!decision.strict);
    }

    #[test]
    fn c7a_restart_returns_to_answerer_and_file_is_bounded() {
        let root = tempfile::tempdir().expect("root");
        let path = root.path().join("affinity.json");
        let state = AffinityState::with_clock(&path, Arc::new(SystemTime::now), Duration::ZERO);
        state.answered("demo|session", "second", "model", 1, usage(0, 3000));
        state.flush();
        let restarted = AffinityState::with_clock(&path, Arc::new(SystemTime::now), Duration::ZERO);
        let decision = restarted.order(&conversation(), Stays::Auto, "smart", &candidates());
        assert_eq!(decision.order, [1, 0]);
        assert_eq!(decision.reason, Reason::Cache);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path)
                    .expect("metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }

        state.answered("old", "second", "model", 1, usage(0, 3000));
        for index in 0..STICKS_KEPT + 50 {
            state.answered(
                &format!("conversation-{index}"),
                "second",
                "model",
                1,
                usage(0, 3000),
            );
        }
        state.flush();
        let bytes = std::fs::read(&path).expect("saved");
        let saved: serde_json::Map<String, serde_json::Value> =
            serde_json::from_slice(&bytes).expect("json");
        assert_eq!(saved.len(), STICKS_KEPT);
        assert!(!saved.contains_key("old"));
        let _ = STICK_KEEP;
    }

    #[test]
    fn c7b_write_behind_debounces_and_shutdown_flushes() {
        let root = tempfile::tempdir().expect("root");
        let path = root.path().join("affinity.json");
        let state = Arc::new(AffinityState::with_clock(
            &path,
            Arc::new(SystemTime::now),
            Duration::from_millis(30),
        ));
        state.answered("one", "second", "model", 1, usage(0, 3000));
        assert!(!path.exists());
        state.answered("two", "second", "model", 1, usage(0, 3000));
        state.flush();
        let saved: serde_json::Map<String, serde_json::Value> =
            serde_json::from_slice(&std::fs::read(&path).expect("flush")).expect("json");
        assert_eq!(saved.len(), 2);
        state.answered("three", "second", "model", 1, usage(0, 3000));
        for _ in 0..100 {
            if path.exists()
                && std::fs::read(&path)
                    .ok()
                    .and_then(|bytes| {
                        serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&bytes)
                            .ok()
                    })
                    .is_some_and(|saved| saved.contains_key("three"))
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("debounced affinity write did not arrive");
    }
}
