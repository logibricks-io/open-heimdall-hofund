//! Per-account choice of how subscription credits are spent in routing.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use hofund_plugin_api::MemberId;
use serde::{Deserialize, Serialize};

use crate::quota::Allowance;
use crate::strategy::Seat;

const SPENT_SHARE: f64 = 98.0;

/// How an account spends credits once its blocking allowance is used up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CreditsMode {
    /// Default: credits keep the account in normal strategy order.
    #[default]
    On,
    /// Spend credits before other members while credits remain.
    First,
    /// Spend credits only after every other usable member.
    Last,
    /// Never spend credits; a spent blocking window stops the account.
    Off,
}

impl CreditsMode {
    /// Parses a mode name from the CLI or control API.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "on" => Some(Self::On),
            "first" => Some(Self::First),
            "last" => Some(Self::Last),
            "off" => Some(Self::Off),
            _ => None,
        }
    }

    /// Wire and CLI spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::First => "first",
            Self::Last => "last",
            Self::Off => "off",
        }
    }

    /// Whether this is the default routing behavior (`on`).
    #[must_use]
    pub const fn is_default(self) -> bool {
        matches!(self, Self::On)
    }
}

/// Persisted operator settings keyed by `provider|account`.
#[derive(Default)]
struct State {
    modes: BTreeMap<String, CreditsMode>,
}

/// Operator-set credits modes beside the daemon quota directory.
pub struct CreditsModeStore {
    root: Option<PathBuf>,
    state: Mutex<State>,
}

impl CreditsModeStore {
    /// Loads `credits-modes.json` beside the configuration directory.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let mut state = State::default();
        if let Ok(bytes) = std::fs::read(root.join("credits-modes.json"))
            && let Ok(modes) = serde_json::from_slice::<BTreeMap<String, CreditsMode>>(&bytes)
        {
            state.modes = modes;
        }
        Self {
            root: Some(root),
            state: Mutex::new(state),
        }
    }

    /// In-memory store for tests.
    #[must_use]
    pub fn in_memory() -> Self {
        Self {
            root: None,
            state: Mutex::new(State::default()),
        }
    }

    /// Mode for one subscription account, defaulting to [`CreditsMode::On`].
    #[must_use]
    pub fn mode(&self, provider: &str, user: &str) -> CreditsMode {
        let key = account_key(provider, user);
        self.state
            .lock()
            .expect("credits mode store")
            .modes
            .get(&key)
            .copied()
            .unwrap_or_default()
    }

    /// Mode for a router member id such as `account:sub:alice`.
    #[must_use]
    pub fn mode_for_member(&self, member: &str) -> CreditsMode {
        parse_member(member)
            .map(|(provider, user)| self.mode(provider, user))
            .unwrap_or_default()
    }

    /// Sets one account's mode and persists when configured.
    pub fn set(&self, provider: &str, user: &str, mode: CreditsMode) {
        let key = account_key(provider, user);
        let mut state = self.state.lock().expect("credits mode store");
        if mode.is_default() {
            state.modes.remove(&key);
        } else {
            state.modes.insert(key, mode);
        }
        if let Some(root) = &self.root {
            save(root, &state.modes);
        }
    }

    /// Every non-default mode, sorted by provider then account.
    #[must_use]
    pub fn listed(&self) -> Vec<(String, String, CreditsMode)> {
        self.state
            .lock()
            .expect("credits mode store")
            .modes
            .iter()
            .filter_map(|(key, mode)| {
                let (provider, user) = split_key(key)?;
                Some((provider, user, *mode))
            })
            .collect()
    }

    /// Drops settings for accounts no longer configured.
    pub fn retain_accounts(&self, live: &BTreeMap<String, ()>) {
        let mut state = self.state.lock().expect("credits mode store");
        let before = state.modes.len();
        state.modes.retain(|key, _| live.contains_key(key));
        if state.modes.len() != before
            && let Some(root) = &self.root
        {
            save(root, &state.modes);
        }
    }

    /// Rebuilds the live-account map from configured router members.
    pub fn retain_members(&self, members: &HashSet<MemberId>) {
        let live = members
            .iter()
            .filter_map(|member| {
                let subscription = member.subscription()?;
                let account = member.account()?;
                Some((account_key(subscription.as_str(), account.as_str()), ()))
            })
            .collect();
        self.retain_accounts(&live);
    }
}

/// Whether `allowance` is past its blocking windows but may continue on credits.
#[must_use]
pub fn on_credits_spending(allowance: &Allowance, model: &str, now: SystemTime) -> bool {
    allowance.spends_credits(model) && !allowance.available(model, now)
}

/// Effective credits capability for routing after applying `mode`.
#[must_use]
pub fn effective_spends_credits(allowance: &Allowance, model: &str, mode: CreditsMode) -> bool {
    if mode == CreditsMode::Off {
        return false;
    }
    allowance.spends_credits(model)
}

/// Fills routing fields on `seat` from `allowance` and `mode`.
pub fn apply_to_seat(
    seat: &mut Seat,
    allowance: &Allowance,
    model: &str,
    now: SystemTime,
    mode: CreditsMode,
) {
    seat.credits_mode = mode;
    seat.on_credits = on_credits_spending(allowance, model, now) && mode != CreditsMode::Off;
    seat.credits = effective_spends_credits(allowance, model, mode);
    if mode == CreditsMode::Off && seat.on_credits {
        seat.on_credits = false;
    }
    if mode == CreditsMode::Off && !allowance.available(model, now) {
        seat.used = seat.used.max(SPENT_SHARE);
    }
}

/// Reorders `order` after a strategy sort: `first` stays in front while on
/// credits; `last` on credits moves behind every other index.
pub fn adjust_order(order: &mut [usize], seats: &[Seat]) {
    if order.len() < 2 {
        return;
    }
    let mut first_on = Vec::new();
    let mut middle = Vec::new();
    let mut last_on = Vec::new();
    for index in order.iter().copied() {
        let seat = &seats[index];
        if seat.credits_mode == CreditsMode::First
            && seat.credits
            && (seat.on_credits || seat.used >= SPENT_SHARE)
        {
            first_on.push(index);
        } else if seat.credits_mode == CreditsMode::Last && seat.on_credits {
            last_on.push(index);
        } else {
            middle.push(index);
        }
    }
    if first_on.is_empty() && last_on.is_empty() {
        return;
    }
    let mut rebuilt = first_on;
    rebuilt.extend(middle);
    rebuilt.extend(last_on);
    order.copy_from_slice(&rebuilt);
}

fn account_key(provider: &str, user: &str) -> String {
    format!("{}|{}", provider, user.to_ascii_lowercase())
}

fn split_key(key: &str) -> Option<(String, String)> {
    let (provider, user) = key.split_once('|')?;
    Some((provider.to_owned(), user.to_owned()))
}

fn parse_member(member: &str) -> Option<(&str, &str)> {
    let rest = member.strip_prefix("account:")?;
    let (provider, user) = rest.split_once(':')?;
    Some((provider, user))
}

fn save(root: &Path, modes: &BTreeMap<String, CreditsMode>) {
    let path = root.join("credits-modes.json");
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let Ok(bytes) = serde_json::to_vec(modes) else {
        return;
    };
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    if std::fs::write(&temporary, bytes).is_err() {
        return;
    }
    if std::fs::rename(&temporary, &path).is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;
    use crate::quota::{QuotaStore, WindowInfo};
    use hofund_control_proto::{Quota, QuotaKind, QuotaSpan};

    fn span(name: &str, used: f64, reset: Option<SystemTime>) -> QuotaSpan {
        QuotaSpan {
            name: name.to_owned(),
            used,
            remaining: (100.0 - used).max(0.0),
            resets_at: reset.map(crate::quota::quota_time),
            ..QuotaSpan::default()
        }
    }

    fn allowance_on_credits(now: SystemTime) -> Allowance {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = QuotaStore::new(dir.path());
        store.record_success(
            Quota {
                provider: "sub".to_owned(),
                name: "sub".to_owned(),
                kind: QuotaKind::default(),
                user: "a".to_owned(),
                ..Quota::default()
            },
            vec![
                WindowInfo::new(span("day", 100.0, Some(now + Duration::from_secs(3600)))),
                WindowInfo::new(span("credits", 10.0, Some(now + Duration::from_secs(3600))))
                    .with_blocking(false),
            ],
            now,
        );
        store.allowances(now).into_iter().next().expect("allowance")
    }

    fn at(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(seconds)
    }

    #[test]
    fn d42_modes_exclusive_and_persist() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = CreditsModeStore::new(dir.path());
        store.set("sub", "a@x.io", CreditsMode::Last);
        store.set("sub", "a@x.io", CreditsMode::First);
        assert_eq!(store.mode("sub", "a@x.io"), CreditsMode::First);
        let store = CreditsModeStore::new(dir.path());
        assert_eq!(store.mode("sub", "a@x.io"), CreditsMode::First);
        store.set("sub", "a@x.io", CreditsMode::On);
        assert!(store.listed().is_empty());
    }

    #[test]
    fn d42_off_treats_spent_blocking_as_unavailable() {
        let now = at(1_900_000_000);
        let allowance = allowance_on_credits(now);
        assert!(on_credits_spending(&allowance, "model", now));
        let mut seat = Seat::new("account:sub:a");
        apply_to_seat(&mut seat, &allowance, "model", now, CreditsMode::Off);
        assert!(!seat.credits);
        assert!(!seat.on_credits);
        assert!(seat.used >= SPENT_SHARE);
    }

    #[test]
    fn d42_last_and_first_adjust_strategy_order() {
        let mut seats = vec![Seat::new("a"), Seat::new("b"), Seat::new("c")];
        seats[0].credits_mode = CreditsMode::Last;
        seats[0].on_credits = true;
        seats[2].credits_mode = CreditsMode::First;
        seats[2].credits = true;
        seats[2].on_credits = true;
        seats[2].used = 99.0;
        let mut order = vec![0, 1, 2];
        adjust_order(&mut order, &seats);
        assert_eq!(order, [2, 1, 0]);
    }
}
