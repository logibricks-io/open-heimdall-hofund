//! Quota snapshots, cached readings, history, and router allowance views.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hofund_control_proto::{
    Quota, QuotaHistory, QuotaKind, QuotaLine, QuotaPoint, QuotaSpan, QuotaStanding, QuotaTime,
};
use hofund_plugin_api::QuotaWindow;
use serde::{Deserialize, Serialize};

/// How long window history is retained.
pub const HISTORY_RETENTION: Duration = Duration::from_secs(45 * 24 * 60 * 60);
/// Maximum points retained in one window's history.
pub const HISTORY_POINT_LIMIT: usize = 6000;
/// How long a served request contributes to lastServedAt.
pub const SERVED_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

const HISTORY_GAP: Duration = Duration::from_secs(5 * 60);
const HISTORY_SAME: Duration = Duration::from_secs(30);
const HISTORY_SLACK: Duration = Duration::from_secs(5 * 60);
const BALANCE_MOVE_LIMIT: usize = 32;
const BALANCE_SHARE_WINDOW: Duration = Duration::from_secs(5 * 60);

/// Metadata for a fresh key balance read, stored beside the quota record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BalanceRecordMeta {
    /// Whether the upstream balance is account-wide rather than per key.
    pub account_wide: bool,
    /// Whether the card carries split balance parts.
    pub balance_parts: bool,
    /// Single wallet currency when unambiguous; empty otherwise.
    pub balance_unit: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct BalanceMove {
    from: String,
    to: String,
    at: QuotaTime,
}

/// Routing metadata and the public representation of one quota window.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowInfo {
    /// Public window fields.
    pub span: QuotaSpan,
    /// Whether exhaustion blocks this member.
    #[serde(default = "yes")]
    pub blocking: bool,
    /// The one model this window covers, or all models.
    #[serde(default)]
    pub scope_model: Option<String>,
    /// Window length, used to derive a history cycle's start.
    #[serde(default)]
    pub window_span: Option<Duration>,
    /// Relative reset, converted against the reading clock on report.
    #[serde(default)]
    pub reset_after: Option<Duration>,
}

impl WindowInfo {
    /// Wraps a public span with routing metadata.
    #[must_use]
    pub fn new(span: QuotaSpan) -> Self {
        Self {
            span,
            blocking: true,
            scope_model: None,
            window_span: None,
            reset_after: None,
        }
    }

    /// Sets whether exhaustion blocks routing.
    #[must_use]
    pub fn with_blocking(mut self, blocking: bool) -> Self {
        self.blocking = blocking;
        self
    }

    /// Restricts this window to one model.
    #[must_use]
    pub fn with_scope_model(mut self, model: impl Into<String>) -> Self {
        self.scope_model = Some(model.into());
        self
    }

    /// Sets the known window length.
    #[must_use]
    pub fn with_window_span(mut self, span: Duration) -> Self {
        self.window_span = Some(span);
        self
    }

    /// Sets a reset relative to the report clock.
    #[must_use]
    pub fn with_reset_after(mut self, after: Duration) -> Self {
        self.reset_after = Some(after);
        self
    }

    /// Converts a plugin count window to the shared model.
    #[must_use]
    pub fn from_plugin(window: &QuotaWindow) -> Self {
        let used = window.limit().map_or(0.0, |limit| {
            if limit == 0 {
                100.0
            } else {
                window.used() as f64 * 100.0 / limit as f64
            }
        });
        let span = QuotaSpan {
            name: window.name().to_owned(),
            used,
            remaining: (100.0 - used).max(0.0),
            resets_at: window.resets_at().map(quota_time),
            amount: window.used() as f64,
            limit: window.limit().unwrap_or(0) as f64,
            unlimited: window.limit().is_none(),
            ..QuotaSpan::default()
        };
        Self {
            span,
            blocking: window.blocking(),
            scope_model: window.scope_model().map(str::to_owned),
            window_span: None,
            reset_after: None,
        }
    }

    fn reset(&self, now: SystemTime) -> Option<SystemTime> {
        self.span
            .resets_at
            .as_ref()
            .and_then(parse_quota_time)
            .or_else(|| self.reset_after.map(|after| now + after))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct QuotaRecord {
    quota: Quota,
    windows: Vec<WindowInfo>,
    #[serde(default)]
    persistent: bool,
    #[serde(default)]
    account_wide: bool,
    #[serde(default)]
    balance_parts: bool,
    #[serde(default)]
    balance_unit: String,
}

#[derive(Clone, Debug)]
enum ReadIssue {
    Deferred(Quota),
    Failed { quota: Quota, transient: bool },
}

#[derive(Default)]
struct State {
    records: Vec<QuotaRecord>,
    issues: BTreeMap<String, ReadIssue>,
    history: BTreeMap<String, BTreeMap<String, Vec<QuotaPoint>>>,
    balance_moves: BTreeMap<String, Vec<BalanceMove>>,
}

/// Explicit quota cache and history state for one daemon.
pub struct QuotaStore {
    root: Option<PathBuf>,
    clock: Arc<dyn Fn() -> SystemTime + Send + Sync>,
    state: Mutex<State>,
}

impl QuotaStore {
    /// Loads quota and history files beside the daemon configuration.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_clock(root, Arc::new(SystemTime::now))
    }

    /// Creates a store that never touches the filesystem.
    #[must_use]
    pub fn in_memory() -> Self {
        Self {
            root: None,
            clock: Arc::new(SystemTime::now),
            state: Mutex::new(State::default()),
        }
    }

    /// Loads state with an injected wall clock.
    #[must_use]
    pub fn with_clock(
        root: impl Into<PathBuf>,
        clock: Arc<dyn Fn() -> SystemTime + Send + Sync>,
    ) -> Self {
        let root = root.into();
        let mut state = State::default();
        if let Ok(bytes) = std::fs::read(root.join("quotas.json"))
            && let Ok(mut records) = serde_json::from_slice::<Vec<QuotaRecord>>(&bytes)
        {
            for record in &mut records {
                record.quota.as_of = record.quota.read_at.clone();
            }
            state.records = records;
        }
        if let Ok(bytes) = std::fs::read(root.join("quota-history.json"))
            && let Ok(history) = serde_json::from_slice(&bytes)
        {
            state.history = history;
        }
        if let Ok(bytes) = std::fs::read(root.join("balance-moves.json"))
            && let Ok(moves) = serde_json::from_slice(&bytes)
        {
            state.balance_moves = moves;
        }
        Self {
            root: Some(root),
            clock,
            state: Mutex::new(state),
        }
    }

    /// Current wall clock used for new readings and reports.
    #[must_use]
    pub fn now(&self) -> SystemTime {
        (self.clock)()
    }

    /// Adds an account whose first read has not been attempted.
    pub fn register(&self, quota: Quota) {
        let mut state = self.state.lock().expect("quota store");
        let key = quota_key(&quota);
        if !state
            .records
            .iter()
            .any(|record| quota_key(&record.quota) == key)
            && !state.issues.contains_key(&key)
        {
            state.issues.insert(key, ReadIssue::Deferred(quota));
        }
    }

    /// Records a fresh successful read and its history points.
    pub fn record_success(&self, quota: Quota, windows: Vec<WindowInfo>, now: SystemTime) {
        self.record_success_meta(quota, windows, now, None);
    }

    /// Records a fresh key balance read with sharing metadata.
    pub fn record_balance(
        &self,
        quota: Quota,
        windows: Vec<WindowInfo>,
        now: SystemTime,
        meta: BalanceRecordMeta,
    ) {
        self.record_success_meta(quota, windows, now, Some(meta));
    }

    fn record_success_meta(
        &self,
        mut quota: Quota,
        mut windows: Vec<WindowInfo>,
        now: SystemTime,
        balance: Option<BalanceRecordMeta>,
    ) {
        let read_at = quota
            .read_at
            .as_ref()
            .and_then(parse_quota_time)
            .filter(|at| *at <= now)
            .unwrap_or(now);
        quota.read_at = Some(quota_time(read_at));
        quota.as_of = None;
        quota.read_deferred = false;
        quota.error.clear();
        for info in &mut windows {
            if let Some(reset) = info.reset(read_at) {
                info.span.resets_at = Some(quota_time(reset));
            }
        }
        quota.windows = windows.iter().map(|info| info.span.clone()).collect();
        let persistent = !quota.windows.is_empty() || !quota.balance.is_empty();
        let mut state = self.state.lock().expect("quota store");
        let key = quota_key(&quota);
        let previous_balance = state
            .records
            .iter()
            .find(|existing| quota_key(&existing.quota) == key)
            .map(|existing| existing.quota.balance.clone());
        let (account_wide, balance_parts, balance_unit) = balance.as_ref().map_or(
            (
                state
                    .records
                    .iter()
                    .find(|existing| quota_key(&existing.quota) == key)
                    .map(|existing| existing.account_wide)
                    .unwrap_or_default(),
                state
                    .records
                    .iter()
                    .find(|existing| quota_key(&existing.quota) == key)
                    .map(|existing| existing.balance_parts)
                    .unwrap_or_default(),
                state
                    .records
                    .iter()
                    .find(|existing| quota_key(&existing.quota) == key)
                    .map(|existing| existing.balance_unit.clone())
                    .unwrap_or_default(),
            ),
            |meta| {
                (
                    meta.account_wide,
                    meta.balance_parts,
                    meta.balance_unit.clone(),
                )
            },
        );
        let record = QuotaRecord {
            quota: quota.clone(),
            windows,
            persistent,
            account_wide,
            balance_parts,
            balance_unit,
        };
        state.issues.remove(&key);
        if let Some(index) = state
            .records
            .iter()
            .position(|existing| quota_key(&existing.quota) == key)
        {
            state.records[index] = record;
        } else {
            state.records.push(record);
        }
        if let Some(previous) = previous_balance.as_deref() {
            note_balance_move(&mut state, &quota, previous, &quota.balance, read_at);
        }
        note_history(&mut state, &quota, read_at, now);
        if let Some(root) = &self.root {
            save_state(root, &state);
        }
    }

    /// Records a deferred read. No upstream request was made.
    pub fn record_deferred(&self, quota: Quota) {
        let mut state = self.state.lock().expect("quota store");
        state
            .issues
            .insert(quota_key(&quota), ReadIssue::Deferred(quota));
    }

    /// Whether `quota` has a successful reading with windows or a balance.
    #[must_use]
    pub fn has_reading(&self, quota: &Quota) -> bool {
        let key = quota_key(quota);
        let state = self.state.lock().expect("quota store");
        state
            .records
            .iter()
            .any(|record| quota_key(&record.quota) == key && record.persistent)
    }

    /// Records a failed read. A transient failure uses the last good reading.
    pub fn record_failure(&self, quota: Quota, transient: bool) {
        let mut state = self.state.lock().expect("quota store");
        state
            .issues
            .insert(quota_key(&quota), ReadIssue::Failed { quota, transient });
    }

    /// Marks keys of one provider that read the same account-wide balance.
    pub fn share_provider_balances(&self, provider: &str) {
        let mut state = self.state.lock().expect("quota store");
        for record in &mut state.records {
            if record.quota.provider == provider && record.quota.kind == QuotaKind::Balance {
                record.quota.shared_balance.clear();
            }
        }
        let mut indices = Vec::new();
        for (index, record) in state.records.iter().enumerate() {
            if balance_share_candidate(&state, provider, record) {
                indices.push(index);
            }
        }
        for left in 0..indices.len() {
            if !state.records[indices[left]].quota.shared_balance.is_empty() {
                continue;
            }
            for right in left + 1..indices.len() {
                if !state.records[indices[right]]
                    .quota
                    .shared_balance
                    .is_empty()
                {
                    continue;
                }
                let left_record = &state.records[indices[left]];
                let right_record = &state.records[indices[right]];
                if !balance_cards_share(&state, left_record, right_record) {
                    continue;
                }
                let leader = left_record.quota.user.clone();
                state.records[indices[left]].quota.shared_balance = leader.clone();
                state.records[indices[right]].quota.shared_balance = leader;
                break;
            }
        }
        if let Some(root) = &self.root {
            save_state(root, &state);
        }
    }

    /// Returns a report in configured provider order without fetching anything.
    #[must_use]
    pub fn report(&self, provider_order: &[String]) -> Vec<Quota> {
        let state = self.state.lock().expect("quota store");
        let mut quotas = report_of(&state);
        in_provider_order(&mut quotas, provider_order);
        quotas
    }

    /// Returns history since `days`, optionally filtered by provider and user.
    #[must_use]
    pub fn histories(
        &self,
        days: Option<&str>,
        provider: Option<&str>,
        user: Option<&str>,
        now: SystemTime,
    ) -> Vec<QuotaHistory> {
        let since = history_since(days, now);
        let state = self.state.lock().expect("quota store");
        let mut out = Vec::new();
        for (key, windows) in &state.history {
            let Some((owner, account)) = key.split_once('|') else {
                continue;
            };
            if provider.is_some_and(|id| id != owner)
                || user.is_some_and(|id| !id.eq_ignore_ascii_case(account))
            {
                continue;
            }
            let mut lines = Vec::new();
            for (name, points) in windows {
                let first = points.partition_point(|point| {
                    parse_quota_time(&point.at).is_some_and(|at| at < since)
                });
                let start = first.saturating_sub(1);
                if start < points.len() {
                    lines.push(QuotaLine {
                        name: name.clone(),
                        points: points[start..].to_vec(),
                    });
                }
            }
            if !lines.is_empty() {
                out.push(QuotaHistory {
                    provider: owner.to_owned(),
                    user: account.to_owned(),
                    lines,
                });
            }
        }
        out
    }

    /// Each account's standing at `now`, in configured provider order, for
    /// `hofund quota wait`. Key balances have no windows to wait on and are
    /// left out.
    #[must_use]
    pub fn standings(&self, provider_order: &[String], now: SystemTime) -> Vec<QuotaStanding> {
        let state = self.state.lock().expect("quota store");
        let mut quotas = report_of(&state);
        in_provider_order(&mut quotas, provider_order);
        quotas
            .into_iter()
            .filter(|quota| quota.kind != QuotaKind::Balance)
            .map(|quota| {
                let key = quota_key(&quota);
                let windows: Vec<WindowInfo> = state
                    .records
                    .iter()
                    .find(|record| quota_key(&record.quota) == key)
                    .map(|record| record.windows.clone())
                    .unwrap_or_default();
                standing(&quota, &windows, now)
            })
            .collect()
    }

    /// Router-facing allowance for every currently readable member.
    #[must_use]
    pub fn allowances(&self, now: SystemTime) -> Vec<Allowance> {
        let state = self.state.lock().expect("quota store");
        report_of(&state)
            .into_iter()
            .filter(|quota| quota.error.is_empty() && !quota.display_only)
            .filter_map(|quota| {
                let key = quota_key(&quota);
                let record = state
                    .records
                    .iter()
                    .find(|record| quota_key(&record.quota) == key)?;
                let windows = record
                    .windows
                    .iter()
                    .map(|info| {
                        let reset = info.reset(now);
                        let expired = reset.is_some_and(|at| at <= now);
                        let used = if expired {
                            0.0
                        } else if info.span.balance == Some(0.0) {
                            100.0
                        } else {
                            info.span.used
                        };
                        AllowanceWindow {
                            name: info.span.name.clone(),
                            used,
                            remaining: (100.0 - used).max(0.0),
                            resets_at: reset,
                            amount: info.span.amount,
                            limit: info.span.limit,
                            unit: info.span.unit.clone(),
                            blocking: info.blocking,
                            scope_model: info.scope_model.clone(),
                            window_span: window_span(info),
                            runs_out_at: info.span.runs_out_at.as_ref().and_then(parse_quota_time),
                            runs_out: info.span.runs_out,
                            balance: info.span.balance,
                        }
                    })
                    .collect();
                Some(Allowance {
                    member: served_key(&quota),
                    provider: quota.provider.clone(),
                    windows,
                })
            })
            .collect()
    }
}

/// Whether `quota` has allowance at `now`, reckoned over `windows`.
///
/// A reading is readable when it has no error, is not a kept reading, and
/// names windows. It is used up when a window that stops the account for
/// every model is at 100% and has not reset. It is back when the last such
/// window resets; when one of them does not say, nobody can tell.
#[must_use]
pub fn standing(quota: &Quota, windows: &[WindowInfo], now: SystemTime) -> QuotaStanding {
    let mut out = QuotaStanding::new(quota.provider.clone(), quota.user.clone());
    out.name = quota.name.clone();
    out.error = quota.error.clone();
    out.readable = quota.error.is_empty() && quota.as_of.is_none() && !quota.windows.is_empty();
    let stopping: Vec<&WindowInfo> = windows
        .iter()
        .filter(|info| {
            info.blocking
                && info.scope_model.is_none()
                && info.span.used >= 100.0
                && !info
                    .span
                    .resets_at
                    .as_ref()
                    .and_then(parse_quota_time)
                    .is_some_and(|reset| reset <= now)
        })
        .collect();
    out.used_up = !stopping.is_empty();
    let mut back: Option<SystemTime> = None;
    for info in &stopping {
        match info.span.resets_at.as_ref().and_then(parse_quota_time) {
            Some(at) => back = Some(back.map_or(at, |known| known.max(at))),
            None => {
                back = None;
                break;
            }
        }
    }
    out.back_at = back.map(quota_time);
    out
}

/// Shortest window that is a budget rather than a rate cap.
const BUDGET_SPAN: Duration = Duration::from_secs(24 * 60 * 60);
/// Budget span assumed when a window tells neither its span nor its reset.
const WEEK_SPAN: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const HOUR: Duration = Duration::from_secs(60 * 60);

/// One member's remaining windows.
#[derive(Clone, Debug, PartialEq)]
pub struct Allowance {
    /// Stable member identity shared with served request rows.
    pub member: String,
    /// Provider or subscription id, shared by all of its accounts.
    pub provider: String,
    /// Current windows.
    pub windows: Vec<AllowanceWindow>,
}

impl Allowance {
    /// The applicable blocking window with the greatest used share.
    #[must_use]
    pub fn fullest(&self, model: &str, now: SystemTime) -> Option<&AllowanceWindow> {
        self.windows
            .iter()
            .filter(|window| window.applies(model, now))
            .max_by(|left, right| {
                left.used
                    .total_cmp(&right.used)
                    .then_with(|| left.resets_at.cmp(&right.resets_at))
            })
    }

    /// Used share for routing: the fullest window that counts `model`,
    /// credit windows included. A window whose reset has passed is empty.
    #[must_use]
    pub fn used_for_routing(&self, model: &str, now: SystemTime) -> f64 {
        self.windows
            .iter()
            .filter(|window| window.counts(model))
            .map(|window| {
                if window.expired(now) {
                    0.0
                } else {
                    window.used
                }
            })
            .fold(0.0, f64::max)
    }

    /// Applicable reset deadlines, earliest first.
    #[must_use]
    pub fn renewals(&self, model: &str, now: SystemTime) -> Vec<SystemTime> {
        let mut resets: Vec<_> = self
            .windows
            .iter()
            .filter(|window| window.applies(model, now))
            .filter_map(|window| window.resets_at)
            .collect();
        resets.sort();
        resets
    }

    /// Renewal times for smart routing, longest window first and, among
    /// equal spans, the later reset first. A window not started, or whose
    /// reset has passed, renews its span from now; one whose remaining
    /// portion lapses before it renews renews then. `None` is a renewal not
    /// known, which smart orders after every known one.
    #[must_use]
    pub fn routing_renewals(&self, model: &str, now: SystemTime) -> Vec<Option<SystemTime>> {
        let mut windows = self
            .windows
            .iter()
            .filter(|window| window.counts(model))
            .map(|window| {
                let span = window.window_span.unwrap_or_default();
                let mut at = window
                    .resets_at
                    .filter(|reset| *reset > now)
                    .or_else(|| (!span.is_zero()).then(|| now + span));
                if let Some(lapse) = window.runs_out_at.filter(|lapse| *lapse > now)
                    && at.is_none_or(|at| lapse < at)
                {
                    at = Some(lapse);
                }
                (span, at)
            })
            .collect::<Vec<_>>();
        windows.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
        windows.into_iter().map(|(_, at)| at).collect()
    }

    /// Soonest remaining portion that lapses apart from a window renewal.
    #[must_use]
    pub fn lapse(&self, model: &str, now: SystemTime) -> Option<SystemTime> {
        self.windows
            .iter()
            .filter(|window| window.counts(model))
            .filter_map(|window| window.runs_out_at)
            .filter(|at| *at > now)
            .min()
    }

    /// Share of its budget the account has left for `model`, per hour until
    /// that budget renews: the tightest of the windows a day or longer (or
    /// of unknown span, taken as a budget running to its reset, else a
    /// week). Hours are never fewer than one. With only shorter windows,
    /// the tightest of those; with none, what is unused over a week.
    #[must_use]
    pub fn pace(&self, model: &str, now: SystemTime) -> f64 {
        let mut budget: Option<f64> = None;
        let mut short: Option<f64> = None;
        let mut used: f64 = 0.0;
        for window in self.windows.iter().filter(|window| window.counts(model)) {
            let share = if window.expired(now) {
                0.0
            } else {
                window.used.clamp(0.0, 100.0)
            };
            let span = window.window_span.unwrap_or_default();
            let reset = window
                .resets_at
                .filter(|reset| *reset > now)
                .and_then(|reset| reset.duration_since(now).ok());
            let is_short = !span.is_zero() && span < BUDGET_SPAN;
            let until = reset.unwrap_or(if span.is_zero() { WEEK_SPAN } else { span });
            let pace = (100.0 - share) / (until.max(HOUR).as_secs_f64() / 3600.0);
            let slot = if is_short {
                used = used.max(share);
                &mut short
            } else {
                &mut budget
            };
            *slot = Some(slot.map_or(pace, |best: f64| best.min(pace)));
        }
        budget
            .or(short)
            .unwrap_or((100.0 - used) / (WEEK_SPAN.as_secs_f64() / 3600.0))
    }

    /// Lowest wallet balance counting `model` and its provider/unit identity.
    #[must_use]
    pub fn left(&self, model: &str) -> Option<(f64, String)> {
        let (balance, unit) = self
            .windows
            .iter()
            .filter(|window| window.counts(model))
            .filter_map(|window| window.balance.map(|balance| (balance, &window.unit)))
            .min_by(|a, b| a.0.total_cmp(&b.0))?;
        Some((balance, format!("{}\0{unit}", self.provider)))
    }

    /// Whether the account goes on past its allowance on credits: it has a
    /// window counting `model` whose exhaustion does not block it.
    #[must_use]
    pub fn spends_credits(&self, model: &str) -> bool {
        self.windows
            .iter()
            .any(|window| !window.blocking && window.counts(model))
    }

    /// Whether no applicable blocking window is exhausted.
    #[must_use]
    pub fn available(&self, model: &str, now: SystemTime) -> bool {
        !self.windows.is_empty()
            && self
                .windows
                .iter()
                .filter(|window| window.blocking && window.counts(model))
                .all(|window| {
                    window.remaining > 0.0 || window.resets_at.is_some_and(|at| at <= now)
                })
    }
}

/// Remaining share and count data for one member window.
#[derive(Clone, Debug, PartialEq)]
pub struct AllowanceWindow {
    /// Window name.
    pub name: String,
    /// Percent used after accounting for an expired reset.
    pub used: f64,
    /// Percent left, never negative.
    pub remaining: f64,
    /// Absolute reset time.
    pub resets_at: Option<SystemTime>,
    /// Amount used in the provider's unit.
    pub amount: f64,
    /// Cap in the provider's unit.
    pub limit: f64,
    /// Provider's unit.
    pub unit: String,
    /// Whether exhaustion blocks routing.
    pub blocking: bool,
    /// The model this window covers, or all models.
    pub scope_model: Option<String>,
    /// Window length, when known.
    pub window_span: Option<Duration>,
    /// When a remaining portion lapses apart from the reset.
    pub runs_out_at: Option<SystemTime>,
    /// Amount lost at `runs_out_at`.
    pub runs_out: f64,
    /// Prepaid wallet balance, where shares do not apply.
    pub balance: Option<f64>,
}

impl AllowanceWindow {
    fn counts(&self, model: &str) -> bool {
        self.scope_model
            .as_deref()
            .is_none_or(|scope| scope.eq_ignore_ascii_case(model))
    }

    fn expired(&self, now: SystemTime) -> bool {
        self.resets_at.is_some_and(|reset| reset <= now)
    }

    fn applies(&self, model: &str, now: SystemTime) -> bool {
        self.blocking && self.counts(model) && !self.resets_at.is_some_and(|reset| reset <= now)
    }
}

/// A successful request used to decorate quota cards.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServedRequest {
    /// Member id recorded on the usage row.
    pub member: String,
    /// When the answer completed.
    pub at: SystemTime,
}

/// Applies last-served fields without changing the source request list.
pub fn apply_last_served(quotas: &mut [Quota], served: &[ServedRequest], now: SystemTime) {
    let mut latest = BTreeMap::<String, SystemTime>::new();
    for request in served {
        if now
            .duration_since(request.at)
            .is_ok_and(|age| age <= SERVED_RETENTION)
        {
            latest
                .entry(request.member.clone())
                .and_modify(|at| *at = (*at).max(request.at))
                .or_insert(request.at);
        }
    }
    let mut newest = None;
    for quota in quotas.iter_mut() {
        let exact = served_key(quota);
        let at = if quota.kind.is_account() && !quota.user.is_empty() {
            latest.get(&exact).copied()
        } else if quota.user.is_empty() {
            latest
                .iter()
                .filter(|(id, _)| {
                    id.as_str() == exact || id.starts_with(&format!("account:{}:", quota.provider))
                })
                .map(|(_, at)| *at)
                .max()
        } else {
            latest.get(&exact).copied()
        };
        quota.last_served_at = at.map(quota_time);
        quota.last = false;
        if let Some(at) = at {
            newest = Some(newest.map_or(at, |old: SystemTime| old.max(at)));
        }
    }
    for quota in quotas {
        quota.last = newest
            .is_some_and(|at| quota.last_served_at.as_ref().and_then(parse_quota_time) == Some(at));
    }
}

/// Stable identity shared by a quota card and served request rows.
#[must_use]
pub fn served_key(quota: &Quota) -> String {
    if !quota.member.is_empty() {
        return quota.member.clone();
    }
    if quota.kind.is_account() && !quota.user.is_empty() {
        format!("account:{}:{}", quota.provider, quota.user.to_lowercase())
    } else {
        quota.provider.clone()
    }
}

/// Sorts cards by configured provider order without mutating cached state.
pub fn in_provider_order(quotas: &mut [Quota], provider_order: &[String]) {
    let rank = |provider: &str| {
        provider_order
            .iter()
            .position(|id| id == provider)
            .unwrap_or(provider_order.len())
    };
    quotas.sort_by_key(|quota| rank(&quota.provider));
}

/// Computes a history range start; invalid or excessive ranges use all retained history.
#[must_use]
pub fn history_since(days: Option<&str>, now: SystemTime) -> SystemTime {
    let seconds = days
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|days| days.is_finite() && *days > 0.0 && *days <= 45.0)
        .map(|days| (days * 24.0 * 60.0 * 60.0) as u64)
        .unwrap_or(HISTORY_RETENTION.as_secs());
    now.checked_sub(Duration::from_secs(seconds))
        .unwrap_or(UNIX_EPOCH)
}

fn balance_share_candidate(state: &State, provider: &str, record: &QuotaRecord) -> bool {
    let quota = &record.quota;
    quota.provider == provider
        && quota.kind == QuotaKind::Balance
        && record.account_wide
        && !quota.user.is_empty()
        && !quota.balance.is_empty()
        && quota.error.is_empty()
        && !record.balance_parts
        && !record.balance_unit.is_empty()
        && !balance_reading_kept(state, quota)
}

fn balance_reading_kept(state: &State, quota: &Quota) -> bool {
    matches!(
        state.issues.get(&quota_key(quota)),
        Some(ReadIssue::Failed {
            transient: true,
            ..
        })
    )
}

fn balance_cards_share(state: &State, left: &QuotaRecord, right: &QuotaRecord) -> bool {
    if left.quota.balance != right.quota.balance {
        return false;
    }
    let Some(left_move) = latest_balance_move(state, &left.quota) else {
        return false;
    };
    let Some(right_move) = latest_balance_move(state, &right.quota) else {
        return false;
    };
    left_move.from == right_move.from
        && left_move.to == right_move.to
        && time_between(&left_move.at, &right_move.at) <= BALANCE_SHARE_WINDOW
}

fn latest_balance_move<'a>(state: &'a State, quota: &Quota) -> Option<&'a BalanceMove> {
    let key = balance_move_key(quota);
    state.balance_moves.get(&key)?.last()
}

fn balance_move_key(quota: &Quota) -> String {
    format!("{}|{}", quota.provider, quota.user.to_lowercase())
}

fn note_balance_move(state: &mut State, quota: &Quota, from: &str, to: &str, read_at: SystemTime) {
    if from.is_empty() || from == to || quota.provider.is_empty() || quota.user.is_empty() {
        return;
    }
    let key = balance_move_key(quota);
    let points = state.balance_moves.entry(key).or_default();
    let next = BalanceMove {
        from: from.to_owned(),
        to: to.to_owned(),
        at: quota_time(read_at),
    };
    if points.last() == Some(&next) {
        return;
    }
    points.push(next);
    if points.len() > BALANCE_MOVE_LIMIT {
        let extra = points.len() - BALANCE_MOVE_LIMIT;
        points.drain(..extra);
    }
}

fn report_of(state: &State) -> Vec<Quota> {
    let mut out = Vec::new();
    for record in &state.records {
        let mut quota = record.quota.clone();
        match state.issues.get(&quota_key(&quota)) {
            Some(ReadIssue::Deferred(_)) => {
                quota.as_of = quota.read_at.clone();
                quota.read_deferred = true;
            }
            Some(ReadIssue::Failed {
                transient: true, ..
            }) => {
                quota.as_of = quota.read_at.clone();
                quota.read_deferred = false;
            }
            Some(ReadIssue::Failed {
                quota: failed,
                transient: false,
            }) => quota = failed.clone(),
            None => {}
        }
        out.push(quota);
    }
    for (key, issue) in &state.issues {
        if state
            .records
            .iter()
            .any(|record| quota_key(&record.quota) == *key)
        {
            continue;
        }
        match issue {
            ReadIssue::Deferred(quota) => {
                let mut quota = quota.clone();
                quota.read_deferred = true;
                out.push(quota);
            }
            ReadIssue::Failed { quota, .. } => out.push(quota.clone()),
        }
    }
    out
}

fn note_history(state: &mut State, quota: &Quota, read_at: SystemTime, now: SystemTime) {
    if !quota.error.is_empty() || quota.as_of.is_some() || quota.provider.is_empty() {
        return;
    }
    let key = format!("{}|{}", quota.provider, quota.user.to_lowercase());
    let record = state
        .records
        .iter()
        .find(|record| quota_key(&record.quota) == quota_key(quota));
    let Some(record) = record else {
        return;
    };
    for info in &record.windows {
        let window = &info.span;
        if window.unlimited || !info.blocking || window.name.is_empty() || !window.used.is_finite()
        {
            continue;
        }
        let reset = info.reset(read_at);
        let point = QuotaPoint {
            at: quota_time(read_at),
            left: ((100.0 - window.used).clamp(0.0, 100.0) * 100.0).round() / 100.0,
            start: reset.and_then(|end| {
                window_span(info)
                    .map(|span| quota_time(end.checked_sub(span).unwrap_or(UNIX_EPOCH)))
            }),
            resets_at: reset.map(quota_time),
        };
        let points = state
            .history
            .entry(key.clone())
            .or_default()
            .entry(window.name.clone())
            .or_default();
        *points = add_point(std::mem::take(points), point);
    }
    prune_history(&mut state.history, now);
}

fn add_point(mut points: Vec<QuotaPoint>, next: QuotaPoint) -> Vec<QuotaPoint> {
    let Some(next_at) = parse_quota_time(&next.at) else {
        return points;
    };
    if points.last().and_then(|point| parse_quota_time(&point.at)) >= Some(next_at) {
        return points;
    }
    if points.len() >= 2 {
        let first = &points[points.len() - 2];
        let second = &points[points.len() - 1];
        if !new_cycle(first, second) && !new_cycle(second, &next) {
            let flat =
                (next.left - second.left).abs() < 0.05 && (second.left - first.left).abs() < 0.05;
            let close = time_between(&first.at, &next.at) < HISTORY_GAP;
            if flat || close {
                let last = points.len() - 1;
                points[last] = next;
                return points;
            }
        }
    }
    if let Some(previous) = points.last()
        && time_between(&previous.at, &next.at) < HISTORY_SAME
        && !new_cycle(previous, &next)
    {
        let last = points.len() - 1;
        points[last] = next;
        return points;
    }
    points.push(next);
    points
}

fn new_cycle(previous: &QuotaPoint, next: &QuotaPoint) -> bool {
    let (Some(a), Some(b)) = (parse_quota_time(&previous.at), parse_quota_time(&next.at)) else {
        return false;
    };
    let ar = previous.resets_at.as_ref().and_then(parse_quota_time);
    let br = next.resets_at.as_ref().and_then(parse_quota_time);
    match (ar, br) {
        (None, None) => next.left > previous.left + 5.0,
        (None, Some(_)) | (Some(_), None) => true,
        (Some(ar), Some(br)) => {
            if ar <= b {
                return true;
            }
            let moved_signed = if br >= ar {
                br.duration_since(ar).unwrap_or_default().as_secs_f64()
            } else {
                -ar.duration_since(br).unwrap_or_default().as_secs_f64()
            };
            let elapsed = b.duration_since(a).unwrap_or_default().as_secs_f64();
            let rolling =
                moved_signed > 0.0 && (moved_signed - elapsed).abs() <= HISTORY_SLACK.as_secs_f64();
            if !rolling
                && moved_signed.abs() <= HISTORY_SLACK.as_secs_f64()
                && next.left > previous.left + 5.0
            {
                return true;
            }
            moved_signed > elapsed + HISTORY_SLACK.as_secs_f64()
                || moved_signed < -HISTORY_SLACK.as_secs_f64()
        }
    }
}

fn prune_history(
    history: &mut BTreeMap<String, BTreeMap<String, Vec<QuotaPoint>>>,
    now: SystemTime,
) {
    let cut = now.checked_sub(HISTORY_RETENTION).unwrap_or(UNIX_EPOCH);
    history.retain(|_, windows| {
        windows.retain(|_, points| {
            let first = points
                .partition_point(|point| parse_quota_time(&point.at).is_some_and(|at| at < cut));
            if first > 0 {
                points.drain(..first);
            }
            if points.len() > HISTORY_POINT_LIMIT {
                let extra = points.len() - HISTORY_POINT_LIMIT;
                points.drain(..extra);
            }
            !points.is_empty()
        });
        !windows.is_empty()
    });
}

fn window_span(info: &WindowInfo) -> Option<Duration> {
    if info.window_span.is_some() {
        return info.window_span;
    }
    let name = info.span.name.to_lowercase();
    if name.contains("daily") {
        return Some(Duration::from_secs(24 * 60 * 60));
    }
    if name.contains("weekly") {
        return Some(Duration::from_secs(7 * 24 * 60 * 60));
    }
    if name.contains("monthly") {
        return Some(Duration::from_secs(30 * 24 * 60 * 60));
    }
    let mut numbers = String::new();
    for (index, ch) in name.char_indices() {
        if ch.is_ascii_digit() {
            numbers.push(ch);
        } else if !numbers.is_empty() {
            let number = numbers.parse::<u64>().ok()?;
            let rest = name[index..].trim_start_matches([' ', '-']);
            let unit = rest.split_whitespace().next()?;
            let seconds = match unit.trim_end_matches('s') {
                "h" | "hr" | "hour" => 60 * 60,
                "d" | "day" => 24 * 60 * 60,
                "w" | "wk" | "week" => 7 * 24 * 60 * 60,
                "month" => 30 * 24 * 60 * 60,
                _ => return None,
            };
            return Some(Duration::from_secs(number.checked_mul(seconds)?));
        }
    }
    None
}

fn time_between(first: &QuotaTime, second: &QuotaTime) -> Duration {
    match (parse_quota_time(first), parse_quota_time(second)) {
        (Some(first), Some(second)) => second.duration_since(first).unwrap_or_default(),
        _ => Duration::ZERO,
    }
}

fn quota_key(quota: &Quota) -> String {
    format!(
        "{}|{:?}|{}",
        quota.provider,
        quota.kind,
        quota.user.to_lowercase()
    )
}

fn save_state(root: &Path, state: &State) {
    let records: Vec<_> = state
        .records
        .iter()
        .filter(|record| record.persistent)
        .cloned()
        .collect();
    atomic_json(&root.join("quotas.json"), &records);
    atomic_json(&root.join("quota-history.json"), &state.history);
    atomic_json(&root.join("balance-moves.json"), &state.balance_moves);
}

fn atomic_json(path: &Path, value: &impl Serialize) {
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let Ok(bytes) = serde_json::to_vec(value) else {
        return;
    };
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    if std::fs::write(&temporary, bytes).is_err() {
        return;
    }
    if std::fs::rename(&temporary, path).is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
}

/// Formats a UTC timestamp to whole-second RFC 3339 precision.
#[must_use]
pub fn quota_time(at: SystemTime) -> QuotaTime {
    let seconds = at
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_else(|error| -(error.duration().as_secs() as i64));
    let days = seconds.div_euclid(86_400);
    let daytime = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    QuotaTime::new(format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        daytime / 3600,
        daytime / 60 % 60,
        daytime % 60
    ))
}

/// Parses the RFC 3339 timestamps emitted by this module and numeric offsets.
#[must_use]
pub fn parse_quota_time(value: &QuotaTime) -> Option<SystemTime> {
    let text = value.as_str();
    if text.len() < 20 {
        return None;
    }
    let number = |start: usize, len: usize| text.get(start..start + len)?.parse::<i64>().ok();
    if text.get(4..5) != Some("-")
        || text.get(7..8) != Some("-")
        || !matches!(text.get(10..11), Some("T" | "t" | " "))
        || text.get(13..14) != Some(":")
        || text.get(16..17) != Some(":")
    {
        return None;
    }
    let year = number(0, 4)?;
    let month = number(5, 2)?;
    let day = number(8, 2)?;
    let hour = number(11, 2)?;
    let minute = number(14, 2)?;
    let second = number(17, 2)?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let local = days.checked_mul(86_400)? + hour * 3600 + minute * 60 + second.min(59);
    let suffix = text.get(19..)?;
    let offset = if suffix.eq_ignore_ascii_case("z") {
        0
    } else if suffix.len() == 6
        && matches!(suffix.as_bytes().first(), Some(b'+' | b'-'))
        && suffix.get(3..4) == Some(":")
    {
        let hours = suffix.get(1..3)?.parse::<i64>().ok()?;
        let minutes = suffix.get(4..6)?.parse::<i64>().ok()?;
        if hours > 23 || minutes > 59 {
            return None;
        }

        (hours * 3600 + minutes * 60) * if suffix.starts_with('-') { -1 } else { 1 }
    } else {
        return None;
    };
    let seconds = local.checked_sub(offset)?;
    Some(if seconds >= 0 {
        UNIX_EPOCH + Duration::from_secs(seconds as u64)
    } else {
        UNIX_EPOCH - Duration::from_secs(seconds.unsigned_abs())
    })
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 }.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 }.div_euclid(400);
    let yoe = year - era * 400;
    let mp = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

const fn yes() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use hofund_control_proto::QuotaKind;
    use hofund_plugin_api::QuotaWindow as PluginWindow;

    use super::*;

    fn at(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(seconds)
    }

    fn quota(provider: &str, user: &str) -> Quota {
        Quota {
            provider: provider.to_owned(),
            name: provider.to_owned(),
            kind: if user.is_empty() {
                QuotaKind::Balance
            } else {
                QuotaKind::default()
            },
            user: user.to_owned(),
            ..Quota::default()
        }
    }

    fn span(name: &str, used: f64, reset: Option<SystemTime>) -> QuotaSpan {
        QuotaSpan {
            name: name.to_owned(),
            used,
            remaining: (100.0 - used).max(0.0),
            resets_at: reset.map(quota_time),
            ..QuotaSpan::default()
        }
    }

    fn info(name: &str, used: f64, reset: Option<SystemTime>) -> WindowInfo {
        WindowInfo::new(span(name, used, reset))
    }

    fn point(seconds: u64, left: f64, reset: Option<SystemTime>) -> QuotaPoint {
        QuotaPoint {
            at: quota_time(at(seconds)),
            left,
            start: None,
            resets_at: reset.map(quota_time),
        }
    }

    fn balance_card(provider: &str, user: &str) -> Quota {
        Quota {
            provider: provider.to_owned(),
            name: provider.to_owned(),
            kind: QuotaKind::Balance,
            user: user.to_owned(),
            ..Quota::default()
        }
    }

    fn wide_meta() -> BalanceRecordMeta {
        BalanceRecordMeta {
            account_wide: true,
            balance_parts: false,
            balance_unit: "USD".into(),
        }
    }

    #[test]
    fn p3o_shared_provider_balances_mark_matching_cards() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = QuotaStore::new(dir.path());
        let meta = wide_meta();
        let seed = at(1_900_000_000);
        for user in ["a", "b"] {
            let mut card = balance_card("relay", user);
            card.balance = "$10.00".to_owned();
            store.record_balance(card, Vec::new(), seed, meta.clone());
        }
        let moved = seed + Duration::from_secs(30);
        for user in ["a", "b"] {
            let mut card = balance_card("relay", user);
            card.balance = "$9.00".to_owned();
            store.record_balance(card, Vec::new(), moved, meta.clone());
        }
        store.share_provider_balances("relay");
        let report = store.report(&[]);
        assert_eq!(report[0].shared_balance, "a");
        assert_eq!(report[1].shared_balance, "a");
    }

    #[test]
    fn p3s_same_text_and_wide_without_moves_stays_unshared() {
        let store = QuotaStore::in_memory();
        let meta = wide_meta();
        let read_at = at(1_900_000_000);
        for user in ["a", "b"] {
            let mut card = balance_card("relay", user);
            card.balance = "$10.00".to_owned();
            store.record_balance(card, Vec::new(), read_at, meta.clone());
        }
        store.share_provider_balances("relay");
        let report = store.report(&[]);
        assert!(report.iter().all(|card| card.shared_balance.is_empty()));
    }

    #[test]
    fn p3t_same_move_within_five_minutes_shares_first_key() {
        let store = QuotaStore::in_memory();
        let meta = wide_meta();
        let seed = at(1_900_000_000);
        for user in ["a", "b"] {
            let mut card = balance_card("relay", user);
            card.balance = "$10.00".to_owned();
            store.record_balance(card, Vec::new(), seed, meta.clone());
        }
        let moved = seed + Duration::from_secs(60);
        for user in ["a", "b"] {
            let mut card = balance_card("relay", user);
            card.balance = "$9.00".to_owned();
            store.record_balance(card, Vec::new(), moved, meta.clone());
        }
        store.share_provider_balances("relay");
        let report = store.report(&[]);
        assert_eq!(report[0].shared_balance, "a");
        assert_eq!(report[1].shared_balance, "a");
    }

    #[test]
    fn p3u_ten_minute_gap_and_non_wide_cards_stay_unshared() {
        let store = QuotaStore::in_memory();
        let wide = wide_meta();
        let seed = at(1_900_000_000);
        for user in ["a", "b"] {
            let mut card = balance_card("relay", user);
            card.balance = "$10.00".to_owned();
            store.record_balance(card, Vec::new(), seed, wide.clone());
        }
        let late = seed + Duration::from_secs(10 * 60);
        let mut first = balance_card("relay", "a");
        first.balance = "$9.00".to_owned();
        store.record_balance(first, Vec::new(), late, wide.clone());
        let mut second = balance_card("relay", "b");
        second.balance = "$9.00".to_owned();
        store.record_balance(
            second,
            Vec::new(),
            late + Duration::from_secs(10 * 60),
            wide,
        );
        store.share_provider_balances("relay");
        let report = store.report(&[]);
        assert!(report.iter().all(|card| card.shared_balance.is_empty()));

        let narrow = BalanceRecordMeta {
            account_wide: false,
            balance_parts: false,
            balance_unit: "USD".into(),
        };
        let store = QuotaStore::in_memory();
        let moved = at(2_000_000_000);
        for user in ["a", "b"] {
            let mut card = balance_card("relay", user);
            card.balance = "$10.00".to_owned();
            store.record_balance(
                card,
                Vec::new(),
                moved - Duration::from_secs(60),
                narrow.clone(),
            );
            let mut card = balance_card("relay", user);
            card.balance = "$9.00".to_owned();
            store.record_balance(card, Vec::new(), moved, narrow.clone());
        }
        store.share_provider_balances("relay");
        let report = store.report(&[]);
        assert!(report.iter().all(|card| card.shared_balance.is_empty()));
    }

    #[test]
    fn p3v_diverged_amounts_clear_a_previous_shared_pair() {
        let store = QuotaStore::in_memory();
        let meta = wide_meta();
        let seed = at(1_900_000_000);
        for user in ["a", "b"] {
            let mut card = balance_card("relay", user);
            card.balance = "$10.00".to_owned();
            store.record_balance(card, Vec::new(), seed, meta.clone());
            let mut card = balance_card("relay", user);
            card.balance = "$9.00".to_owned();
            store.record_balance(
                card,
                Vec::new(),
                seed + Duration::from_secs(30),
                meta.clone(),
            );
        }
        store.share_provider_balances("relay");
        assert!(
            store
                .report(&[])
                .iter()
                .all(|card| card.shared_balance == "a")
        );
        let mut only_a = balance_card("relay", "a");
        only_a.balance = "$8.00".to_owned();
        store.record_balance(
            only_a,
            Vec::new(),
            seed + Duration::from_secs(90),
            meta.clone(),
        );
        store.share_provider_balances("relay");
        let report = store.report(&[]);
        assert!(report.iter().all(|card| card.shared_balance.is_empty()));
    }

    #[test]
    fn q1c_report_clamps_remaining_and_keeps_run_out_distinct() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = QuotaStore::new(dir.path());
        let end = at(2_000_000_000);
        let mut window = info("credits", 140.0, None);
        window.span.runs_out_at = Some(quota_time(end));
        window.span.runs_out = 18.44;
        store.record_success(quota("p", "a"), vec![window], at(1_900_000_000));
        let report = store.report(&[]);
        assert_eq!(report[0].windows[0].remaining, 0.0);
        assert_eq!(report[0].windows[0].runs_out_at, Some(quota_time(end)));
        assert_eq!(report[0].windows[0].runs_out, 18.44);
        assert_eq!(report[0].windows[0].resets_at, None);
    }

    #[test]
    fn q1d_plugin_windows_convert_without_losing_routing_metadata() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = QuotaStore::new(dir.path());
        let plugin = PluginWindow::new("weekly", 3, Some(10), Some(at(2_000_000_000)))
            .expect("plugin window")
            .with_blocking(false)
            .with_scope_model("model-a");
        store.record_success(
            quota("p", "a"),
            vec![WindowInfo::from_plugin(&plugin)],
            at(1_900_000_000),
        );
        let report = store.report(&[]);
        assert_eq!(report[0].windows[0].used, 30.0);
        assert_eq!(report[0].windows[0].remaining, 70.0);
        assert_eq!(report[0].windows[0].amount, 3.0);
        assert_eq!(report[0].windows[0].limit, 10.0);
        assert!(
            serde_json::to_value(&report[0]).expect("JSON")["windows"][0]
                .get("scopeModel")
                .is_none()
        );
        let allowance = store.allowances(at(1_900_000_001));
        assert_eq!(
            allowance[0].windows[0].scope_model.as_deref(),
            Some("model-a")
        );
        assert!(!allowance[0].windows[0].blocking);
    }

    #[test]
    fn q1e_plugin_window_zero_cap_is_used_up() {
        let zero = PluginWindow::new("day", 0, Some(0), None).expect("zero cap");
        let zero = WindowInfo::from_plugin(&zero);
        assert_eq!(zero.span.used, 100.0);
        assert_eq!(zero.span.remaining, 0.0);
        let full = PluginWindow::new("day", 10, Some(10), None).expect("full");
        let full = WindowInfo::from_plugin(&full);
        assert_eq!(full.span.used, 100.0);
        assert_eq!(full.span.remaining, 0.0);
    }

    #[test]
    fn q1f_in_memory_store_never_writes_files() {
        let store = QuotaStore::in_memory();
        assert!(store.root.is_none());
        store.record_success(
            quota("in-memory-probe", "a"),
            vec![info("day", 1.0, None)],
            at(1_900_000_000),
        );
        assert!(!Path::new("quotas.json").exists());
        assert!(!Path::new("quota-history.json").exists());
        assert_eq!(store.report(&[]).len(), 1);
    }

    #[test]
    fn q3f_success_takes_a_past_read_time_clamps_a_future_one_and_clears_as_of() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = QuotaStore::new(dir.path());
        let now = at(1_900_000_000);
        let mut future = quota("p", "a");
        future.read_at = Some(quota_time(now + Duration::from_secs(3600)));
        future.as_of = Some(quota_time(at(1_800_000_000)));
        store.record_success(future, vec![info("day", 10.0, None)], now);
        let report = store.report(&[]);
        assert_eq!(report[0].read_at, Some(quota_time(now)));
        assert_eq!(report[0].as_of, None);
        let mut past = quota("p", "a");
        past.read_at = Some(quota_time(now - Duration::from_secs(60)));
        store.record_success(past, vec![info("day", 10.0, None)], now);
        assert_eq!(
            store.report(&[])[0].read_at,
            Some(quota_time(now - Duration::from_secs(60)))
        );
    }

    #[test]
    fn q2a_relative_reset_uses_injected_clock() {
        let dir = tempfile::tempdir().expect("tempdir");
        let now = at(1_900_000_000);
        let store = QuotaStore::with_clock(dir.path(), Arc::new(move || now));
        let absolute = now + Duration::from_secs(9000);
        store.record_success(
            quota("p", "a"),
            vec![
                info("relative", 20.0, None).with_reset_after(Duration::from_secs(3600)),
                info("absolute", 20.0, Some(absolute)).with_reset_after(Duration::from_secs(10)),
            ],
            now,
        );
        let report = store.report(&[]);
        assert_eq!(
            report[0].windows[0].resets_at,
            Some(quota_time(now + Duration::from_secs(3600)))
        );
        assert_eq!(report[0].windows[1].resets_at, Some(quota_time(absolute)));
    }

    #[test]
    fn q3a_successful_read_persists_and_reloads_as_of() {
        let dir = tempfile::tempdir().expect("tempdir");
        let read_at = at(1_900_000_000);
        let path = dir.path().join("quotas.json");
        {
            let store = QuotaStore::new(dir.path());
            store.record_success(
                quota("p", "A@example.com"),
                vec![info("day", 25.0, None)],
                read_at,
            );
            let fresh = store.report(&[]);
            assert_eq!(fresh[0].read_at, Some(quota_time(read_at)));
            assert_eq!(fresh[0].as_of, None);
        }
        assert!(path.exists());
        let store = QuotaStore::new(dir.path());
        let kept = store.report(&[]);
        assert_eq!(kept[0].as_of, Some(quota_time(read_at)));
        assert_eq!(kept[0].read_at, Some(quota_time(read_at)));
        assert_eq!(kept[0].windows[0].used, 25.0);
    }

    #[test]
    fn q3b_failed_read_distinguishes_cached_fallback_and_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = QuotaStore::new(dir.path());
        let read_at = at(1_900_000_000);
        store.record_success(quota("p", "a"), vec![info("day", 25.0, None)], read_at);
        let mut failure = quota("p", "a");
        failure.error = "HTTP 503".to_owned();
        store.record_failure(failure, true);
        let cached = store.report(&[]);
        assert_eq!(cached[0].as_of, Some(quota_time(read_at)));
        assert_eq!(cached[0].error, "");
        assert!(!cached[0].read_deferred);
        assert_eq!(cached[0].windows[0].used, 25.0);
        let mut refused = quota("p", "a");
        refused.error = "401 unauthorized".to_owned();
        store.record_failure(refused, false);
        let failed = store.report(&[]);
        assert_eq!(failed[0].error, "401 unauthorized");
        assert_eq!(failed[0].as_of, None);
    }

    #[test]
    fn q3c_deferred_read_is_not_a_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = QuotaStore::new(dir.path());
        let read_at = at(1_900_000_000);
        store.record_success(quota("p", "a"), vec![info("day", 32.0, None)], read_at);
        store.record_deferred(quota("p", "a"));
        let cached = store.report(&[]);
        assert!(cached[0].read_deferred);
        assert_eq!(cached[0].error, "");
        assert_eq!(cached[0].as_of, Some(quota_time(read_at)));
        assert_eq!(cached[0].windows[0].used, 32.0);
    }

    #[test]
    fn q3e_expired_cached_window_is_not_still_exhausted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let read_at = at(1_900_000_000);
        let reset = read_at + Duration::from_secs(60);
        {
            let store = QuotaStore::new(dir.path());
            store.record_success(
                quota("p", "a"),
                vec![info("minute", 100.0, Some(reset))],
                read_at,
            );
        }
        let store = QuotaStore::new(dir.path());
        let report = store.report(&[]);
        assert_eq!(report[0].as_of, Some(quota_time(read_at)));
        assert_eq!(report[0].windows[0].used, 100.0);
        let allowance = store.allowances(reset + Duration::from_secs(1));
        assert_eq!(allowance[0].windows[0].used, 0.0);
        assert_eq!(allowance[0].windows[0].remaining, 100.0);
        assert!(allowance[0].available("model", reset + Duration::from_secs(1)));
    }

    #[test]
    fn q4a_provider_order_is_stable_and_non_mutating() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = QuotaStore::new(dir.path());
        for (provider, user, kind) in [
            ("a", "a1", QuotaKind::default()),
            ("unknown", "r", QuotaKind::default()),
            ("c", "c1", QuotaKind::default()),
            ("b", "", QuotaKind::Plan),
            ("a", "a2", QuotaKind::default()),
            ("c", "", QuotaKind::Balance),
        ] {
            let mut card = quota(provider, user);
            card.kind = kind;
            store.record_success(card, vec![info("day", 1.0, None)], at(1_900_000_000));
        }
        let order = vec!["c".to_owned(), "a".to_owned(), "b".to_owned()];
        let identities = |cards: &[Quota]| {
            cards
                .iter()
                .map(|card| {
                    format!("{}:{}:{:?}", card.provider, card.user, card.kind).to_lowercase()
                })
                .collect::<Vec<_>>()
        };
        let first = store.report(&order);
        assert_eq!(
            identities(&first),
            [
                "c:c1:subscription",
                "c::balance",
                "a:a1:subscription",
                "a:a2:subscription",
                "b::plan",
                "unknown:r:subscription"
            ]
        );
        assert_eq!(identities(&store.report(&order)), identities(&first));
    }

    #[test]
    fn q6a_history_records_normalized_points() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = QuotaStore::new(dir.path());
        let read_at = at(1_900_000_000);
        let reset = read_at + Duration::from_secs(5 * 3600);
        store.record_success(
            quota("p", "A@Example.com"),
            vec![
                info("5 hours", 12.345, Some(reset))
                    .with_window_span(Duration::from_secs(5 * 3600)),
            ],
            read_at,
        );
        let histories = store.histories(None, None, None, read_at);
        assert_eq!(histories.len(), 1);
        assert_eq!(histories[0].user, "a@example.com");
        let point = &histories[0].lines[0].points[0];
        assert_eq!(point.at, quota_time(read_at));
        assert_eq!(point.left, 87.66);
        assert_eq!(point.resets_at, Some(quota_time(reset)));
        assert_eq!(point.start, Some(quota_time(read_at)));
    }

    #[test]
    fn q6b_history_excludes_non_new_or_undrawable_readings() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = QuotaStore::new(dir.path());
        let read_at = at(1_900_000_000);
        let windows = vec![
            info("valid", 20.0, None),
            WindowInfo::new(QuotaSpan {
                unlimited: true,
                ..span("unlimited", 0.0, None)
            }),
            info("aside", 20.0, None).with_blocking(false),
            info("", 20.0, None),
            info("nan", f64::NAN, None),
        ];
        store.record_success(quota("p", "a"), windows.clone(), read_at);
        store.record_success(quota("p", "a"), windows, read_at);
        let mut failure = quota("p", "a");
        failure.error = "failed".to_owned();
        store.record_failure(failure, false);
        let histories = store.histories(None, None, None, read_at);
        assert_eq!(histories[0].lines.len(), 1);
        assert_eq!(histories[0].lines[0].name, "valid");
        assert_eq!(histories[0].lines[0].points.len(), 1);
    }

    #[test]
    fn q6c_history_samples_near_and_flat_points() {
        let points = vec![point(0, 90.0, None), point(20, 89.0, None)];
        let points = add_point(points, point(100, 87.0, None));
        assert_eq!(points.len(), 2);
        assert_eq!(points[1].at, quota_time(at(100)));
        let points = add_point(points, point(400, 87.0, None));
        let points = add_point(points, point(700, 87.0, None));
        assert_eq!(points.len(), 3);
        assert_eq!(points[2].at, quota_time(at(700)));
        assert_eq!(points[2].left, 87.0);
    }

    #[test]
    fn q6d_history_detects_cycle_boundaries() {
        let start = at(1_900_000_000);
        let reset = start + Duration::from_secs(3600);
        let first = point(1_900_000_000, 20.0, Some(reset));
        let rolling = point(1_900_000_060, 20.0, Some(reset + Duration::from_secs(60)));
        assert!(!new_cycle(&first, &rolling));
        let refill = point(1_900_000_060, 100.0, Some(reset));
        assert!(new_cycle(&first, &refill));
        assert_eq!(add_point(vec![first.clone()], refill).len(), 2);
        let moved = point(1_900_000_060, 20.0, Some(reset + Duration::from_secs(600)));
        assert!(new_cycle(&first, &moved));
        let past = point(1_900_003_700, 100.0, Some(reset));
        assert!(new_cycle(&first, &past));
        let no_reset = point(1_900_000_060, 20.0, None);
        assert!(new_cycle(&first, &no_reset));
        let correction = point(1_900_000_060, 24.0, Some(reset + Duration::from_secs(60)));
        assert!(!new_cycle(&first, &correction));
    }

    #[test]
    fn q6h_history_replaces_a_reading_within_thirty_seconds() {
        let points = add_point(vec![point(0, 90.0, None)], point(10, 89.0, None));
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].at, quota_time(at(10)));
        assert_eq!(points[0].left, 89.0);
        let points = add_point(points, point(60, 88.0, None));
        assert_eq!(points.len(), 2);
    }

    #[test]
    fn q6i_cycle_without_resets_is_a_refill_and_a_passed_reset_is_a_boundary() {
        assert!(new_cycle(&point(0, 20.0, None), &point(60, 30.0, None)));
        assert!(!new_cycle(&point(0, 20.0, None), &point(60, 24.0, None)));
        let reset = at(100);
        let before = point(0, 20.0, Some(reset));
        assert!(new_cycle(&before, &point(100, 20.0, Some(reset))));
        assert!(!new_cycle(&before, &point(99, 20.0, Some(reset))));
    }

    #[test]
    fn q6e_history_enforces_retention_and_point_cap() {
        let now = at(1_900_000_000);
        let old = now - HISTORY_RETENTION - Duration::from_secs(1);
        let mut points = vec![point(
            old.duration_since(UNIX_EPOCH)
                .expect("after epoch")
                .as_secs(),
            1.0,
            None,
        )];
        let begin = now - Duration::from_secs(10_000);
        for index in 0..=HISTORY_POINT_LIMIT as u64 {
            points.push(point(
                begin
                    .duration_since(UNIX_EPOCH)
                    .expect("after epoch")
                    .as_secs()
                    + index * 31,
                50.0,
                None,
            ));
        }
        let mut history = BTreeMap::from([(
            "p|a".to_owned(),
            BTreeMap::from([("day".to_owned(), points)]),
        )]);
        prune_history(&mut history, now);
        let points = &history["p|a"]["day"];
        assert_eq!(points.len(), HISTORY_POINT_LIMIT);
        assert!(
            points
                .iter()
                .all(|point| parse_quota_time(&point.at).expect("time") >= begin)
        );
        history.insert("empty|".to_owned(), BTreeMap::new());
        prune_history(&mut history, now);
        assert!(!history.contains_key("empty|"));
    }

    #[test]
    fn q6f_history_range_and_filters_match_reference() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = QuotaStore::new(dir.path());
        let now = at(1_900_000_000);
        let day = Duration::from_secs(24 * 3600);
        {
            let mut state = store.state.lock().expect("state");
            state.history.insert(
                "p|a@example.com".to_owned(),
                BTreeMap::from([(
                    "day".to_owned(),
                    vec![
                        point(
                            (now - 2 * day)
                                .duration_since(UNIX_EPOCH)
                                .expect("epoch")
                                .as_secs(),
                            90.0,
                            None,
                        ),
                        point(
                            (now - day)
                                .duration_since(UNIX_EPOCH)
                                .expect("epoch")
                                .as_secs(),
                            80.0,
                            None,
                        ),
                        point(
                            (now - Duration::from_secs(60))
                                .duration_since(UNIX_EPOCH)
                                .expect("epoch")
                                .as_secs(),
                            70.0,
                            None,
                        ),
                    ],
                )]),
            );
        }
        let selected = store.histories(Some("1"), Some("p"), Some("A@EXAMPLE.COM"), now);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].lines[0].points.len(), 3);
        assert!(
            store
                .histories(Some("0"), Some("other"), None, now)
                .is_empty()
        );
        assert_eq!(history_since(Some("46"), now), now - HISTORY_RETENTION);
        assert_eq!(history_since(Some("nope"), now), now - HISTORY_RETENTION);
    }

    #[test]
    fn q8a_last_served_uses_successful_rows_within_30_days() {
        let now = at(1_900_000_000);
        let recent = now - Duration::from_secs(60);
        let old = now - SERVED_RETENTION - Duration::from_secs(1);
        let mut cards = vec![
            quota("p", "A@Example.com"),
            quota("p", "b"),
            quota("q", "c"),
        ];
        apply_last_served(
            &mut cards,
            &[
                ServedRequest {
                    member: "account:p:a@example.com".to_owned(),
                    at: recent,
                },
                ServedRequest {
                    member: "account:p:b".to_owned(),
                    at: old,
                },
            ],
            now,
        );
        assert_eq!(cards[0].last_served_at, Some(quota_time(recent)));
        assert_eq!(cards[1].last_served_at, None);
        assert_eq!(cards[2].last_served_at, None);
    }

    #[test]
    fn q8b_last_served_marks_latest_and_ties() {
        let now = at(1_900_000_000);
        let served = vec![
            ServedRequest {
                member: "account:p:a".to_owned(),
                at: now - Duration::from_secs(60),
            },
            ServedRequest {
                member: "account:p:b".to_owned(),
                at: now - Duration::from_secs(30),
            },
            ServedRequest {
                member: "account:p:c".to_owned(),
                at: now - Duration::from_secs(30),
            },
        ];
        let mut provider_card = quota("p", "");
        provider_card.kind = QuotaKind::default();
        let mut cards = vec![
            quota("p", "a"),
            quota("p", "b"),
            quota("p", "c"),
            provider_card,
        ];
        apply_last_served(&mut cards, &served, now);
        assert!(!cards[0].last);
        assert!(cards[1].last && cards[2].last);
        assert_eq!(
            cards[3].last_served_at,
            Some(quota_time(now - Duration::from_secs(30)))
        );
        assert!(cards[3].last);
    }

    #[test]
    fn q9a_allowance_reports_share_left_and_renewal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = QuotaStore::new(dir.path());
        let now = at(1_900_000_000);
        let reset = now + Duration::from_secs(60);
        let mut window = info("week", 125.0, Some(reset));
        window.span.amount = 50.0;
        window.span.limit = 40.0;
        window.span.unit = "requests".to_owned();
        store.record_success(quota("p", "a"), vec![window.clone()], now);
        let allowance = store.allowances(now);
        assert_eq!(allowance[0].windows[0].remaining, 0.0);
        assert_eq!(allowance[0].windows[0].amount, 50.0);
        assert_eq!(allowance[0].windows[0].limit, 40.0);
        assert_eq!(allowance[0].windows[0].unit, "requests");
        let allowance = store.allowances(reset + Duration::from_secs(1));
        assert_eq!(allowance[0].windows[0].remaining, 100.0);
    }

    #[test]
    fn q9b_allowance_respects_model_scope_blocking_and_failures() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = QuotaStore::new(dir.path());
        let now = at(1_900_000_000);
        store.record_success(
            quota("p", "a"),
            vec![
                info("model", 100.0, None).with_scope_model("model-a"),
                info("informational", 100.0, None).with_blocking(false),
            ],
            now,
        );
        let allowance = &store.allowances(now)[0];
        assert!(!allowance.available("model-a", now));
        assert!(allowance.available("model-b", now));
        assert_eq!(allowance.fullest("model-b", now), None);
        let mut failed = quota("p", "a");
        failed.error = "401".to_owned();
        store.record_failure(failed, false);
        assert!(store.allowances(now).is_empty());
        assert!(
            store
                .allowances(now)
                .iter()
                .all(|entry| entry.member != "unknown")
        );
    }

    #[test]
    fn q9d_held_allowance_frees_after_reset_and_matches_scope_case_insensitively() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = QuotaStore::new(dir.path());
        let now = at(1_900_000_000);
        let reset = now + Duration::from_secs(60);
        store.record_success(
            quota("p", "a"),
            vec![info("model", 100.0, Some(reset)).with_scope_model("Model-A")],
            now,
        );
        let allowance = &store.allowances(now)[0];
        assert!(!allowance.available("model-a", now));
        assert!(!allowance.available("MODEL-A", now));
        assert!(allowance.available("model-b", now));
        assert!(allowance.available("model-a", reset));
    }

    #[test]
    fn q7g_standing_reads_used_up_and_back_at_like_the_reference() {
        let now = at(1_900_000_000);
        let later = now + Duration::from_secs(600);
        let latest = now + Duration::from_secs(7200);
        let mut card = quota("p", "a");
        card.windows = vec![span("5h", 100.0, Some(later))];
        let windows = vec![
            info("5h", 100.0, Some(later)),
            info("week", 100.0, Some(latest)),
            info("past", 100.0, Some(now - Duration::from_secs(1))),
            info("model", 100.0, None).with_scope_model("model-a"),
            info("aside", 100.0, None).with_blocking(false),
            info("open", 99.9, None),
        ];
        let out = standing(&card, &windows, now);
        assert!(out.readable);
        assert!(out.used_up);
        assert_eq!(
            out.back_at,
            Some(quota_time(latest)),
            "the latest reset wins"
        );
        assert_eq!(out.provider, "p");
        assert_eq!(out.user, "a");
        assert_eq!(out.name, "p");

        let unknown = standing(
            &card,
            &[info("5h", 100.0, Some(later)), info("week", 100.0, None)],
            now,
        );
        assert!(unknown.used_up);
        assert_eq!(
            unknown.back_at, None,
            "one silent window hides when it is back"
        );

        let exact = standing(&card, &[info("5h", 100.0, Some(now))], now);
        assert!(
            !exact.used_up,
            "a window resetting now is no longer used up"
        );
        let free = standing(&card, &[info("open", 99.9, Some(later))], now);
        assert!(!free.used_up);
        assert_eq!(free.back_at, None);
    }

    #[test]
    fn q7h_standing_is_unreadable_for_errors_cached_or_empty_cards() {
        let now = at(1_900_000_000);
        let mut card = quota("p", "a");
        assert!(!standing(&card, &[], now).readable, "no windows");
        card.windows = vec![span("5h", 10.0, None)];
        assert!(standing(&card, &[], now).readable);
        card.as_of = Some(quota_time(now));
        assert!(!standing(&card, &[], now).readable, "cached");
        card.as_of = None;
        card.error = "401".to_owned();
        let failed = standing(&card, &[], now);
        assert!(!failed.readable);
        assert_eq!(failed.error, "401");
    }

    #[test]
    fn q7i_standings_follow_provider_order_and_skip_balances() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = QuotaStore::new(dir.path());
        let now = at(1_900_000_000);
        let reset = now + Duration::from_secs(60);
        store.record_success(quota("z", "a"), vec![info("5h", 100.0, Some(reset))], now);
        store.record_success(quota("b", "u"), vec![info("5h", 5.0, None)], now);
        store.record_success(quota("k", ""), Vec::new(), now);
        let out = store.standings(&["z".to_owned(), "b".to_owned()], now);
        let order: Vec<&str> = out.iter().map(|entry| entry.provider.as_str()).collect();
        assert_eq!(order, ["z", "b"]);
        assert!(out[0].used_up);
        assert_eq!(out[0].back_at, Some(quota_time(reset)));
        assert!(!out[1].used_up);
    }

    #[test]
    fn q9c_allowance_fullest_and_renewal_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = QuotaStore::new(dir.path());
        let now = at(1_900_000_000);
        let week = now + Duration::from_secs(7 * 24 * 3600);
        let month = now + Duration::from_secs(30 * 24 * 3600);
        store.record_success(
            quota("p", "a"),
            vec![
                info("week", 32.0, Some(week)),
                info("month", 16.0, Some(month)),
                info("other", 99.0, Some(month)).with_scope_model("other-model"),
            ],
            now,
        );
        let allowance = &store.allowances(now)[0];
        assert_eq!(
            allowance.fullest("model", now).expect("fullest").name,
            "week"
        );
        assert_eq!(allowance.renewals("model", now), vec![week, month]);
        let tied = Allowance {
            member: "p".to_owned(),
            provider: "p".to_owned(),
            windows: vec![
                AllowanceWindow {
                    name: "week".to_owned(),
                    used: 100.0,
                    remaining: 0.0,
                    resets_at: Some(week),
                    amount: 0.0,
                    limit: 0.0,
                    unit: String::new(),
                    blocking: true,
                    scope_model: None,
                    window_span: None,
                    runs_out_at: None,
                    runs_out: 0.0,
                    balance: None,
                },
                AllowanceWindow {
                    name: "month".to_owned(),
                    used: 100.0,
                    remaining: 0.0,
                    resets_at: Some(month),
                    amount: 0.0,
                    limit: 0.0,
                    unit: String::new(),
                    blocking: true,
                    scope_model: None,
                    window_span: None,
                    runs_out_at: None,
                    runs_out: 0.0,
                    balance: None,
                },
            ],
        };
        assert_eq!(tied.fullest("model", now).expect("tie").name, "month");
    }

    fn allowance_of(windows: Vec<WindowInfo>, now: SystemTime) -> Allowance {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = QuotaStore::new(dir.path());
        store.record_success(quota("p", "a"), windows, now);
        store.allowances(now).remove(0)
    }

    const H: u64 = 3600;

    #[test]
    fn b3b_pace_takes_the_tightest_budget_with_an_hour_floor() {
        let now = at(1_900_000_000);
        // a week 50% used renewing in 50h (1.0/h) beside a month 10% used
        // renewing in 300h (0.3/h): the tightest budget is the month's
        let week = info("w", 50.0, Some(now + Duration::from_secs(50 * H)))
            .with_window_span(Duration::from_secs(168 * H));
        let month = info("m", 10.0, Some(now + Duration::from_secs(300 * H)))
            .with_window_span(Duration::from_secs(720 * H));
        let five = info("5h", 0.0, Some(now + Duration::from_secs(H)))
            .with_window_span(Duration::from_secs(5 * H));
        let allowance = allowance_of(vec![month, week.clone(), five.clone()], now);
        assert!((allowance.pace("model", now) - 0.3).abs() < 1e-9);
        // a budget a minute from renewing counts at least an hour
        let soon = info("w", 40.0, Some(now + Duration::from_secs(60)))
            .with_window_span(Duration::from_secs(168 * H));
        assert!((allowance_of(vec![soon], now).pace("model", now) - 60.0).abs() < 1e-9);
        // only a short window: it goes by that, per hour until its reset
        let only_short = info("5h", 20.0, Some(now + Duration::from_secs(4 * H)))
            .with_window_span(Duration::from_secs(5 * H));
        assert!((allowance_of(vec![only_short], now).pace("model", now) - 20.0).abs() < 1e-9);
        // a nearly spent short window does not set the pace beside a budget
        let tight_short = info("5h", 99.0, Some(now + Duration::from_secs(4 * H)))
            .with_window_span(Duration::from_secs(5 * H));
        let roomy_week = info("Weekly", 0.0, Some(now + Duration::from_secs(168 * H)))
            .with_window_span(Duration::from_secs(168 * H));
        assert!(
            (allowance_of(vec![tight_short, roomy_week], now).pace("model", now) - 100.0 / 168.0)
                .abs()
                < 1e-9
        );
        // a short window not started runs its whole span
        assert!(
            (allowance_of(
                vec![info("5h", 0.0, None).with_window_span(Duration::from_secs(5 * H))],
                now
            )
            .pace("model", now)
                - 20.0)
                .abs()
                < 1e-9
        );
        // a budget whose span and reset are unknown is a week from now
        let unknown = allowance_of(vec![info("budget", 16.0, None)], now);
        assert!((unknown.pace("model", now) - 0.5).abs() < 1e-9);
        // a window whose reset has passed is empty again
        let passed = info("w", 90.0, Some(now - Duration::from_secs(H)))
            .with_window_span(Duration::from_secs(168 * H));
        assert!((allowance_of(vec![passed], now).pace("model", now) - 100.0 / 168.0).abs() < 1e-9);
        // nothing counts the model: unused over a week
        let scoped = info("w", 10.0, Some(now + Duration::from_secs(H))).with_scope_model("other");
        assert!((allowance_of(vec![scoped], now).pace("model", now) - 100.0 / 168.0).abs() < 1e-9);
    }

    #[test]
    fn b1d_renewals_rank_longest_first_and_fill_unstarted_spans() {
        let now = at(1_900_000_000);
        let day = now + Duration::from_secs(24 * H);
        let week = now + Duration::from_secs(168 * H);
        let lapse = now + Duration::from_secs(H);
        let mut weekly = info("Weekly", 50.0, Some(week));
        weekly.span.runs_out_at = Some(quota_time(lapse));
        weekly.span.runs_out = 10.0;
        let unstarted = info("5h", 0.0, None).with_window_span(Duration::from_secs(5 * H));
        let unknown = info("budget", 0.0, None);
        let allowance = allowance_of(
            vec![info("Daily", 10.0, Some(day)), unstarted, unknown, weekly],
            now,
        );
        assert_eq!(
            allowance.routing_renewals("model", now),
            [
                Some(lapse),
                Some(day),
                Some(now + Duration::from_secs(5 * H)),
                None
            ]
        );
        assert_eq!(allowance.lapse("model", now), Some(lapse));
        assert_eq!(allowance.used_for_routing("model", now), 50.0);
        assert!(allowance.renewals("model", now) == [day, week]);
        // equal spans: the later reset first
        let a = info("x1", 0.0, Some(day)).with_window_span(Duration::from_secs(168 * H));
        let b = info("x2", 0.0, Some(week)).with_window_span(Duration::from_secs(168 * H));
        assert_eq!(
            allowance_of(vec![a, b], now).routing_renewals("model", now),
            [Some(week), Some(day)]
        );
        // a reset passed renews its span from now
        let passed = info("Daily", 70.0, Some(now - Duration::from_secs(60)));
        let passed = allowance_of(vec![passed], now);
        assert_eq!(passed.routing_renewals("model", now), [Some(day)]);
        assert_eq!(passed.used_for_routing("model", now), 0.0);
        // a portion that already lapsed is no longer the next lapse
        let mut lapsed = info("Weekly", 50.0, Some(week));
        lapsed.span.runs_out_at = Some(quota_time(now - Duration::from_secs(60)));
        lapsed.span.runs_out = 10.0;
        let lapsed = allowance_of(vec![lapsed], now);
        assert_eq!(lapsed.lapse("model", now), None);
        assert_eq!(lapsed.routing_renewals("model", now), [Some(week)]);
    }

    #[test]
    fn b1e_credit_windows_count_toward_use_and_mark_credits() {
        let now = at(1_900_000_000);
        let week = now + Duration::from_secs(168 * H);
        // a credit window part used still counts toward the share used
        let credits = allowance_of(
            vec![
                info("Weekly", 20.0, Some(week)),
                info("credit week", 60.0, Some(week)).with_blocking(false),
            ],
            now,
        );
        assert!(credits.spends_credits("model"));
        assert_eq!(credits.used_for_routing("model", now), 60.0);
        // once the windows reset, the reading taken before counts as empty
        assert_eq!(
            credits.used_for_routing("model", week + Duration::from_secs(60)),
            0.0
        );
        let plain = allowance_of(vec![info("Weekly", 99.0, Some(week))], now);
        assert!(!plain.spends_credits("model"));
        assert_eq!(plain.used_for_routing("model", now), 99.0);
        // a wallet: the least balance, named by provider and unit
        let mut wallet = info("wallet", 0.0, None);
        wallet.span.balance = Some(7.5);
        wallet.span.unit = "usd".to_owned();
        let mut bigger = info("wallet2", 0.0, None);
        bigger.span.balance = Some(9.0);
        bigger.span.unit = "usd".to_owned();
        assert_eq!(
            allowance_of(vec![bigger, wallet], now).left("model"),
            Some((7.5, "p\0usd".to_owned()))
        );
    }
}
