//! Conversation affinity, cache warmth, and durable answerer state.

mod rules;
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

#[derive(Clone, Copy)]
pub(crate) struct Answerer<'a> {
    pub(crate) member: &'a str,
    pub(crate) model: &'a str,
    pub(crate) effort: &'a str,
    pub(crate) stood_in: bool,
}

/// A candidate as affinity sees it after strategy ordering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// Stable member id shared across router snapshots.
    pub member: String,
    /// Model this candidate serves.
    pub model: String,
    /// Fixed reasoning effort of the group member, empty when it follows the request.
    pub effort: String,
    /// Whether the member/model pair is cooling down.
    pub resting: bool,
    /// The provider's at-once count when this member is in reserve, else zero.
    pub reserve: usize,
    /// Whether its known allowance is nearly spent.
    pub nearly_spent: bool,
    /// Whether its known allowance is at least 90 percent used.
    pub low_allowance: bool,
    /// Whether this row is a provider fallback.
    pub fallback: bool,
    /// Whether this row is set aside for another protocol.
    pub aside: bool,
    /// Group member identity, shared by its keys and accounts.
    pub group_member: String,
    /// One-based position of the group member, zero outside a group.
    pub place: usize,
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
            effort: String::new(),
            resting: false,
            reserve: 0,
            nearly_spent: false,
            low_allowance: false,
            fallback: false,
            aside: false,
            group_member: String::new(),
            place: 0,
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
    /// A provider fallback returned the conversation to the ready provider.
    Main,
    /// An ordered group returned the conversation to an earlier ready member.
    Order,
    /// A stand-in returned the conversation to an earlier ready group member.
    Back,
    /// Session affinity kept the answerer.
    Session,
    /// The request continues the current turn.
    Turn,
    /// A useful warm cache kept the answerer.
    Cache,
    /// The answerer is in reserve, so a kept member takes the conversation.
    Reserve,
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
            Self::Main => "main",
            Self::Order => "order",
            Self::Back => "back",
            Self::Session => "session",
            Self::Turn => "turn",
            Self::Cache => "cache",
            Self::Reserve => "reserve",
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
    /// Fixed reasoning effort that answered.
    pub effort: String,
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
    /// Whether this answerer stood in for an earlier group member.
    pub stood_in: bool,
}

#[derive(Default)]
struct State {
    sticks: BTreeMap<String, Stick>,
    opening: BTreeMap<String, Stick>,
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
                    candidate.member == stick.member
                        && candidate.model == stick.model
                        && candidate.effort == stick.effort
                })
                .or_else(|| {
                    candidates.iter().position(|candidate| {
                        candidate.member == stick.member && candidate.model == stick.model
                    })
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
            if candidate.reserve > 0 {
                Reason::Reserve
            } else if candidate.resting {
                Reason::Resting
            } else if !conversation.within && rules::back_to_main(candidates, index) {
                Reason::Main
            } else if !conversation.within
                && mode == Stays::Auto
                && routing == "order"
                && rules::back_to_first(candidates, index)
            {
                Reason::Order
            } else if !conversation.within
                && mode == Stays::Auto
                && routing != "order"
                && routing != "rotate"
                && stick.stood_in
                && rules::back_from_stand_in(candidates, index)
            {
                Reason::Back
            } else if index > 0 && candidate.nearly_spent {
                Reason::Spent
            } else if mode == Stays::Session {
                Reason::Session
            } else if conversation.within {
                Reason::Turn
            } else if mode == Stays::Turn || routing == "rotate" {
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
        let spread = rules::spreads_sessions(mode, routing);
        let order = match reason {
            Reason::NewTurn if routing == "rotate" => match at {
                Some(at) => rules::after(order, at, candidates),
                None => order,
            },
            Reason::First if routing == "rotate" => {
                self.least_held(conversation, candidates, order, false, now)
            }
            Reason::First if spread && conversation.key.contains("|agent:") => {
                self.with_lead(conversation, candidates, order, now)
            }
            Reason::First if spread => self.least_held(conversation, candidates, order, true, now),
            _ => order,
        };
        Decision {
            order,
            kept,
            reason,
            strict,
        }
    }

    /// Remembers the answerer and its cache usage.
    pub fn answered(&self, key: &str, member: &str, model: &str, turn: u64, usage: CacheUsage) {
        self.answered_as(
            key,
            Answerer {
                member,
                model,
                effort: "",
                stood_in: false,
            },
            turn,
            usage,
        );
    }

    /// Remembers the answerer, its effort, and whether it stood in for an earlier member.
    pub(crate) fn answered_as(
        &self,
        key: &str,
        answerer: Answerer<'_>,
        turn: u64,
        usage: CacheUsage,
    ) {
        let Answerer {
            member,
            model,
            effort,
            stood_in,
        } = answerer;
        let now = (self.clock)();
        let cache_write = if usage.read == 0 && usage.write == 0 {
            usage.input.unwrap_or(0)
        } else {
            usage.write
        };
        let mut state = self.lock();
        let previous = state
            .sticks
            .get(key)
            .filter(|stick| stick.member == member && stick.model == model)
            .cloned();
        let stood_in = stood_in || previous.as_ref().is_some_and(|stick| stick.stood_in);
        let (hour_tokens, hour_at) = if usage.write_hour > 0 {
            (usage.read.saturating_add(usage.write_hour), Some(now))
        } else if let Some(previous) = previous.as_ref().filter(|previous| {
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
                effort: effort.to_owned(),
                turn,
                at: now,
                cache_read: usage.read,
                cache_write,
                hour_tokens,
                hour_at,
                stood_in,
            },
        );
        state.opening.remove(key);
        if state.sticks.len() > 4096 {
            state.sticks.retain(|_, stick| fresh(stick.at, now));
        }
        drop(state);
        self.note();
    }

    pub(crate) fn opened(&self, key: &str) {
        self.lock().opening.remove(key);
    }

    pub(crate) fn stands_in_for(
        answerer: &Candidate,
        planned: &[Candidate],
        resting: impl FnMut(&Candidate) -> bool,
    ) -> bool {
        rules::stands_in_for(answerer, planned, resting)
    }

    fn least_held(
        &self,
        conversation: &Conversation,
        candidates: &[Candidate],
        order: Vec<usize>,
        ready: bool,
        now: SystemTime,
    ) -> Vec<usize> {
        if candidates.len() < 2 {
            return order;
        }
        let scope = conversation
            .key
            .split_once('|')
            .map_or(conversation.key.as_str(), |(scope, _)| scope);
        let prefix = format!("{scope}|");
        let mut held = rules::Holdings::new();
        let mut state = self.lock();
        for (key, stick) in state.sticks.iter().chain(state.opening.iter()) {
            if key.starts_with(&prefix)
                && age(stick.at, now) <= HELD_FOR
                && !(ready && key.contains("|agent:"))
            {
                *held
                    .entry((
                        stick.member.clone(),
                        stick.model.clone(),
                        stick.effort.clone(),
                    ))
                    .or_default() += 1;
            }
        }
        let order = rules::least_held(order, candidates, &held, ready);
        let selected = &candidates[order[0]];
        state.opening.insert(
            conversation.key.clone(),
            Stick {
                member: selected.member.clone(),
                model: selected.model.clone(),
                effort: selected.effort.clone(),
                turn: 0,
                at: now,
                cache_read: 0,
                cache_write: 0,
                hour_tokens: 0,
                hour_at: None,
                stood_in: false,
            },
        );
        order
    }

    fn with_lead(
        &self,
        conversation: &Conversation,
        candidates: &[Candidate],
        mut order: Vec<usize>,
        now: SystemTime,
    ) -> Vec<usize> {
        let Some((lead, _)) = conversation.key.split_once("|agent:") else {
            return order;
        };
        let stick = self.lock().sticks.get(lead).cloned();
        let Some(stick) = stick.filter(|stick| age(stick.at, now) <= HELD_FOR) else {
            return order;
        };
        for index in 1..candidates.len() {
            let candidate = &candidates[index];
            if candidate.member == stick.member
                && candidate.model == stick.model
                && candidate.effort == stick.effort
                && rules::ready_for_new(candidates, index)
            {
                let selected = order.remove(index);
                order.insert(0, selected);
                break;
            }
        }
        order
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

const HELD_FOR: Duration = Duration::from_secs(30 * 60);

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
    effort: String,
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
    #[serde(
        default,
        rename = "stoodIn",
        skip_serializing_if = "std::ops::Not::not"
    )]
    stood_in: bool,
}

impl SavedStick {
    fn from_stick(stick: &Stick) -> Self {
        Self {
            member: stick.member.clone(),
            model: stick.model.clone(),
            effort: stick.effort.clone(),
            turn: stick.turn,
            at_ms: millis(stick.at),
            cache_read: stick.cache_read,
            cache_write: stick.cache_write,
            cache_hour: stick.hour_tokens,
            cache_hour_at_ms: stick.hour_at.map(millis),
            stood_in: stick.stood_in,
        }
    }

    fn into_stick(self) -> Option<Stick> {
        Some(Stick {
            member: self.member,
            model: self.model,
            effort: self.effort,
            turn: self.turn,
            at: from_millis(self.at_ms)?,
            cache_read: self.cache_read,
            cache_write: self.cache_write,
            hour_tokens: self.cache_hour,
            hour_at: self.cache_hour_at_ms.and_then(from_millis),
            stood_in: self.stood_in,
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
        AffinityState, Answerer, CACHE_COLD, CACHE_HOUR, CacheUsage, Candidate, Conversation,
        Reason, STICK_KEEP, STICKS_KEPT, Stays,
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
        let unused = tempfile::tempdir().expect("temp dir");
        let state =
            AffinityState::with_clock(unused.path().join("unused-affinity"), clock, Duration::ZERO);
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
        let unused = tempfile::tempdir().expect("temp dir");
        let state = AffinityState::with_clock(
            unused.path().join("unused-hour-affinity"),
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
    fn d26_5_affinity_moves_off_reserve_except_in_strict_mode() {
        let state = AffinityState::in_memory();
        state.answered("demo|session", "second", "model", 1, usage(0, 3000));
        let mut candidates = candidates();
        candidates[1].reserve = 2;
        let decision = state.order(&conversation(), Stays::Session, "smart", &candidates);
        assert_eq!(decision.order, [0, 1]);
        assert_eq!(decision.reason, Reason::Reserve);
        assert_eq!(decision.reason.as_str(), "reserve");
        assert!(!decision.kept);
        let strict = state.order(&conversation(), Stays::Strict, "smart", &candidates);
        assert_eq!(strict.order, [1]);
        assert_eq!(strict.reason, Reason::Strict);
        assert!(strict.strict);
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

    fn grouped() -> Vec<Candidate> {
        candidates()
            .into_iter()
            .enumerate()
            .map(|(index, mut candidate)| {
                candidate.group_member = format!("member-{index}");
                candidate.place = index + 1;
                candidate
            })
            .collect()
    }

    fn three() -> Vec<Candidate> {
        let mut candidates = grouped();
        candidates.push(Candidate::new("third", "model"));
        candidates[2].group_member = "member-2".to_owned();
        candidates[2].place = 3;
        candidates
    }

    #[test]
    fn c4a_fallback_gives_back_only_at_a_new_turn_and_not_under_strict() {
        let state = AffinityState::in_memory();
        state.answered("demo|session", "second", "model", 1, usage(0, 3000));
        let mut candidates = candidates();
        candidates[1].fallback = true;

        let decision = state.order(&conversation(), Stays::Auto, "smart", &candidates);
        assert_eq!(decision.reason, Reason::Main);
        assert_eq!(decision.reason.as_str(), "main");
        assert_eq!(decision.order, [0, 1]);
        assert_eq!(
            state
                .order(&conversation(), Stays::Session, "smart", &candidates)
                .reason,
            Reason::Main
        );
        let within = Conversation::new("demo|session", 1, true);
        assert_eq!(
            state
                .order(&within, Stays::Auto, "smart", &candidates)
                .reason,
            Reason::Turn
        );
        let strict = state.order(&conversation(), Stays::Strict, "smart", &candidates);
        assert_eq!(strict.reason, Reason::Strict);
        assert_eq!(strict.order, [1]);

        candidates[0].resting = true;
        let waiting = state.order(&conversation(), Stays::Auto, "smart", &candidates);
        assert_eq!(waiting.reason, Reason::Cache);
        assert_eq!(waiting.order, [1, 0]);
    }

    #[test]
    fn c4b_effort_matching_prefers_the_exact_tier_before_wider_member_matches() {
        let state = AffinityState::in_memory();
        state.answered_as(
            "demo|session",
            Answerer {
                member: "shared",
                model: "model",
                effort: "high",
                stood_in: false,
            },
            1,
            usage(0, 3000),
        );
        let mut low = Candidate::new("shared", "model");
        low.effort = "low".to_owned();
        let mut high = Candidate::new("shared", "model");
        high.effort = "high".to_owned();
        let other = Candidate::new("shared", "other");
        let decision = state.order(
            &conversation(),
            Stays::Auto,
            "smart",
            &[low.clone(), high, other.clone()],
        );
        assert_eq!(decision.order, [1, 0, 2]);
        assert_eq!(
            state
                .order(&conversation(), Stays::Auto, "smart", &[low, other.clone()])
                .order,
            [0, 1]
        );
        assert_eq!(
            state
                .order(&conversation(), Stays::Auto, "smart", &[other])
                .reason,
            Reason::Cache
        );
    }

    #[test]
    fn c5a_ordered_group_returns_only_across_members_turns_and_auto_mode() {
        let state = AffinityState::in_memory();
        state.answered("demo|session", "second", "model", 1, usage(0, 3000));
        let candidates = grouped();
        let decision = state.order(&conversation(), Stays::Auto, "order", &candidates);
        assert_eq!(decision.reason, Reason::Order);
        assert_eq!(decision.reason.as_str(), "order");
        assert_eq!(decision.order, [0, 1]);
        let within = Conversation::new("demo|session", 1, true);
        assert_eq!(
            state
                .order(&within, Stays::Auto, "order", &candidates)
                .reason,
            Reason::Turn
        );
        assert_eq!(
            state
                .order(&conversation(), Stays::Session, "order", &candidates)
                .reason,
            Reason::Session
        );
        assert_eq!(
            state
                .order(&conversation(), Stays::Auto, "smart", &candidates)
                .reason,
            Reason::Cache
        );

        let mut same_member = candidates;
        same_member[1].group_member = same_member[0].group_member.clone();
        same_member[1].place = same_member[0].place;
        assert_eq!(
            state
                .order(&conversation(), Stays::Auto, "order", &same_member)
                .reason,
            Reason::Cache
        );
    }

    #[test]
    fn c6a_session_spread_counts_answered_and_opening_leads_for_thirty_minutes() {
        let root = tempfile::tempdir().expect("root");
        let current = Arc::new(std::sync::Mutex::new(SystemTime::now()));
        let clock = {
            let current = Arc::clone(&current);
            Arc::new(move || *current.lock().expect("clock"))
        };
        let state =
            AffinityState::with_clock(root.path().join("affinity.json"), clock, Duration::ZERO);
        let candidates = three();
        state.answered("demo|old", "first", "model", 1, usage(0, 3000));
        for (key, first) in [("new-1", 1), ("new-2", 2), ("new-3", 0)] {
            let conversation = Conversation::new(format!("demo|{key}"), 1, false);
            let decision = state.order(&conversation, Stays::Session, "smart", &candidates);
            assert_eq!(decision.reason, Reason::First);
            assert_eq!(decision.order[0], first, "{key}");
        }
        *current.lock().expect("clock") += Duration::from_secs(31 * 60);
        let expired = Conversation::new("demo|expired", 1, false);
        assert_eq!(
            state
                .order(&expired, Stays::Session, "smart", &candidates)
                .order[0],
            0
        );
    }

    #[test]
    fn c6b_session_spread_respects_readiness_and_strategy_gating() {
        let state = AffinityState::in_memory();
        state.answered("demo|a", "first", "model", 1, usage(0, 3000));
        state.answered("demo|b", "second", "model", 1, usage(0, 3000));
        let mut candidates = three();
        for fallback in [false, true] {
            candidates[2].low_allowance = !fallback;
            candidates[2].fallback = fallback;
            let new = Conversation::new(format!("demo|new-{fallback}"), 1, false);
            assert_eq!(
                state
                    .order(&new, Stays::Session, "smart", &candidates)
                    .order[0],
                0
            );
            state.opened(&new.key);
        }
        for (mode, routing, expected) in [
            (Stays::Session, "smart", true),
            (Stays::Strict, "usage", true),
            (Stays::Session, "pace", true),
            (Stays::Auto, "smart", false),
            (Stays::Turn, "smart", false),
            (Stays::Off, "smart", false),
            (Stays::Session, "order", false),
            (Stays::Session, "weight", false),
            (Stays::Session, "rotate", false),
        ] {
            assert_eq!(super::rules::spreads_sessions(mode, routing), expected);
        }
    }

    #[test]
    fn c6c_a_subagent_follows_its_ready_lead_with_the_same_effort() {
        let state = AffinityState::in_memory();
        state.answered_as(
            "demo|session",
            Answerer {
                member: "second",
                model: "model",
                effort: "high",
                stood_in: false,
            },
            1,
            usage(0, 3000),
        );
        let mut candidates = grouped();
        candidates[0].effort = "low".to_owned();
        candidates[1].effort = "high".to_owned();
        let subagent = Conversation::new("demo|session|agent:one", 1, false);
        assert_eq!(
            state
                .order(&subagent, Stays::Session, "smart", &candidates)
                .order,
            [1, 0]
        );
        candidates[1].low_allowance = true;
        assert_eq!(
            state
                .order(&subagent, Stays::Session, "smart", &candidates)
                .order,
            [0, 1]
        );
    }

    #[test]
    fn c6d_rotate_goes_after_the_answerer_and_opened_releases_a_hold() {
        let state = AffinityState::in_memory();
        state.answered("demo|session", "second", "model", 1, usage(0, 3000));
        let mut candidates = three();
        assert_eq!(
            state
                .order(&conversation(), Stays::Turn, "rotate", &candidates)
                .order,
            [2, 0, 1]
        );
        candidates[2].resting = true;
        assert_eq!(
            state
                .order(&conversation(), Stays::Turn, "rotate", &candidates)
                .order[0],
            0
        );

        state.answered("opening|old", "first", "model", 1, usage(0, 3000));
        let opening = Conversation::new("opening|new", 1, false);
        assert_eq!(
            state
                .order(&opening, Stays::Auto, "rotate", &candidates[..2])
                .order[0],
            1
        );
        state.opened(&opening.key);
        assert_eq!(
            state
                .order(&opening, Stays::Auto, "rotate", &candidates[..2])
                .order[0],
            1
        );
    }

    #[test]
    fn c6e_nothing_held_keeps_the_routing_order_and_still_counts_the_opening() {
        let state = AffinityState::in_memory();
        let mut candidates = three();
        candidates[0].resting = true;
        let first = Conversation::new("demo|a", 1, false);
        assert_eq!(
            state
                .order(&first, Stays::Session, "smart", &candidates)
                .order,
            [0, 1, 2]
        );
        let second = Conversation::new("demo|b", 1, false);
        assert_eq!(
            state
                .order(&second, Stays::Session, "smart", &candidates)
                .order[0],
            1
        );
    }

    #[test]
    fn c4c_a_route_of_fallback_rows_only_keeps_its_own_affinity() {
        let mut candidates = candidates();
        candidates[0].fallback = true;
        candidates[1].fallback = true;
        assert!(!super::rules::back_to_main(&candidates, 1));
        let state = AffinityState::in_memory();
        state.answered("demo|session", "second", "model", 1, usage(0, 3000));
        let decision = state.order(&conversation(), Stays::Auto, "smart", &candidates);
        assert_ne!(decision.reason, Reason::Main);
        assert_eq!(decision.order[0], 1);
        state.answered("demo|turn", "first", "model", 1, usage(0, 3000));
        let turn = Conversation::new("demo|turn", 2, false);
        assert_eq!(
            state.order(&turn, Stays::Turn, "rotate", &candidates).order[0],
            1
        );
    }

    #[test]
    fn d25d_stand_in_return_needs_a_member_listed_ahead() {
        let mut candidates = three();
        assert!(super::rules::back_from_stand_in(&candidates, 2));
        candidates[0].place = 3;
        assert!(!super::rules::back_from_stand_in(&candidates, 2));
        candidates[0].place = 4;
        assert!(!super::rules::back_from_stand_in(&candidates, 2));
    }

    #[test]
    fn c6f_session_spread_does_not_count_subagents_of_other_sessions() {
        let state = AffinityState::in_memory();
        state.answered("demo|lead|agent:one", "first", "model", 1, usage(0, 3000));
        let new = Conversation::new("demo|new", 1, false);
        assert_eq!(
            state.order(&new, Stays::Session, "smart", &three()).order[0],
            0
        );
    }

    #[test]
    fn d14_rotate_skips_fallback_rows_for_new_and_returning_conversations() {
        let state = AffinityState::in_memory();
        let mut candidates = three();
        candidates[2].fallback = true;
        state.answered("demo|a", "first", "model", 1, usage(0, 3000));
        state.answered("demo|b", "second", "model", 1, usage(0, 3000));
        let new = Conversation::new("demo|new", 1, false);
        assert_eq!(
            state.order(&new, Stays::Auto, "rotate", &candidates).order[0],
            0
        );
        state.answered("demo|session", "second", "model", 1, usage(0, 3000));
        assert_eq!(
            state
                .order(&conversation(), Stays::Turn, "rotate", &candidates)
                .order[0],
            0
        );
    }

    #[test]
    fn d25a_stand_in_return_requires_an_unavailable_earlier_member_and_auto() {
        let state = AffinityState::in_memory();
        state.answered_as(
            "demo|session",
            Answerer {
                member: "second",
                model: "model",
                effort: "high",
                stood_in: true,
            },
            1,
            usage(0, 3000),
        );
        state.answered_as(
            "demo|session",
            Answerer {
                member: "second",
                model: "model",
                effort: "high",
                stood_in: false,
            },
            2,
            usage(0, 3000),
        );
        assert!(state.stick("demo|session").expect("stick").stood_in);
        let candidates = grouped();
        let decision = state.order(&conversation(), Stays::Auto, "smart", &candidates);
        assert_eq!(decision.reason, Reason::Back);
        assert_eq!(decision.reason.as_str(), "back");
        let within = Conversation::new("demo|session", 1, true);
        assert_eq!(
            state
                .order(&within, Stays::Auto, "smart", &candidates)
                .reason,
            Reason::Turn
        );
        assert_eq!(
            state
                .order(&conversation(), Stays::Session, "smart", &candidates)
                .reason,
            Reason::Session
        );
        assert_eq!(
            state
                .order(&conversation(), Stays::Strict, "smart", &candidates)
                .reason,
            Reason::Strict
        );
        assert_eq!(
            state
                .order(&conversation(), Stays::Auto, "rotate", &candidates)
                .reason,
            Reason::NewTurn
        );

        state.answered("demo|merits", "second", "model", 1, usage(0, 3000));
        let merits = Conversation::new("demo|merits", 2, false);
        assert_eq!(
            state
                .order(&merits, Stays::Auto, "smart", &candidates)
                .reason,
            Reason::Cache
        );
        let mut planned = grouped();
        planned[0].resting = true;
        assert!(AffinityState::stands_in_for(&planned[1], &planned, |_| {
            false
        }));
        planned[0].resting = false;
        assert!(!AffinityState::stands_in_for(&planned[1], &planned, |_| {
            false
        }));
        planned[0].nearly_spent = true;
        assert!(AffinityState::stands_in_for(&planned[1], &planned, |_| {
            false
        }));
    }

    #[test]
    fn d25b_stood_in_and_effort_survive_restart_and_legacy_files_default_false() {
        let root = tempfile::tempdir().expect("root");
        let path = root.path().join("affinity.json");
        let now = SystemTime::now();
        let state = AffinityState::with_clock(&path, Arc::new(SystemTime::now), Duration::ZERO);
        state.answered_as(
            "demo|session",
            Answerer {
                member: "second",
                model: "model",
                effort: "high",
                stood_in: true,
            },
            1,
            usage(0, 3000),
        );
        state.flush();
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("saved")).expect("json");
        assert_eq!(saved["demo|session"]["stoodIn"], true);
        assert_eq!(saved["demo|session"]["effort"], "high");
        let restarted = AffinityState::with_clock(&path, Arc::new(SystemTime::now), Duration::ZERO);
        let stick = restarted.stick("demo|session").expect("stand-in");
        assert!(stick.stood_in);
        assert_eq!(stick.effort, "high");
        assert_eq!(
            restarted
                .order(&conversation(), Stays::Auto, "smart", &grouped())
                .reason,
            Reason::Back
        );

        let at_ms = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("time")
            .as_millis();
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "legacy|session": {
                    "member": "second",
                    "model": "model",
                    "turn": 1,
                    "at_ms": at_ms,
                    "cache_write": 3000
                }
            }))
            .expect("legacy json"),
        )
        .expect("legacy file");
        let legacy = AffinityState::with_clock(&path, Arc::new(SystemTime::now), Duration::ZERO);
        let stick = legacy.stick("legacy|session").expect("legacy");
        assert!(!stick.stood_in);
        assert_eq!(stick.effort, "");
    }
}
