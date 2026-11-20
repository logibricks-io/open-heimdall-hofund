//! Allowance windows Claude Code tells in `rate_limit_event` envelopes, kept
//! per account when `/usage` is away or for setup-token accounts.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hofund_plugin_api::{AccountId, HostContext, PluginError, PluginErrorKind, QuotaWindow};
use serde::{Deserialize, Serialize};

use crate::bridge::decode::RateLimitInfo;
use crate::usage;
use crate::usage_cadence::{self, QuotaIntent, ReadDecision};

/// How long stream-derived windows stand in for a failed `/usage` read.
pub(crate) const HEARD_TTL: Duration = Duration::from_secs(60 * 60);

/// A 5-hour window the vendor did not send cannot be inferred from a week
/// that moved longer ago than this.
const FIVE_HOURS: Duration = Duration::from_secs(5 * 60 * 60);

/// Same weekly reset, allowing the clock slack a reading already uses.
const SAME_WEEK: Duration = Duration::from_secs(60 * 60);

/// Last whole reading kept under the account directory so a restart does
/// not invent a 5-hour window again.
pub(crate) const READING_FILE: &str = "usage-reading.json";

/// One allowance kind Claude Code names in `unifiedWindows`.
#[derive(Clone, Copy, Debug)]
struct KindSpec {
    kind: &'static str,
    name: &'static str,
    model: Option<&'static str>,
}

const KINDS: &[KindSpec] = &[
    KindSpec {
        kind: "five_hour",
        name: "5 hours",
        model: None,
    },
    KindSpec {
        kind: "seven_day",
        name: "7 days",
        model: None,
    },
    KindSpec {
        kind: "seven_day_opus",
        name: "7 days · Opus",
        model: Some("opus"),
    },
    KindSpec {
        kind: "seven_day_sonnet",
        name: "7 days · Sonnet",
        model: Some("sonnet"),
    },
    KindSpec {
        kind: "seven_day_overage_included",
        name: "7 days · Fable",
        model: Some("fable"),
    },
];

/// Parsed share-used and reset from one event kind.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RawLimit {
    pub(crate) kind: String,
    pub(crate) used: f64,
    pub(crate) resets_at: Option<u64>,
}

/// Per-account kept allowance, merged from `/usage` and stream events.
#[derive(Clone, Debug, Default)]
struct Entry {
    windows: Vec<QuotaWindow>,
    heard_at: Option<SystemTime>,
    /// When `windows` were last written by a whole reading or a header.
    at: Option<SystemTime>,
    #[allow(dead_code)]
    whole: bool,
    last_err: Option<String>,
    tried: Option<SystemTime>,
    wait: Duration,
    asked: u64,
}

/// In-memory store keyed by account id (lowercase).
#[derive(Clone, Debug, Default)]
pub(crate) struct Store {
    inner: std::sync::Arc<Mutex<HashMap<String, Entry>>>,
}

impl Store {
    /// Empty store.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn key(account: &AccountId) -> String {
        account.as_str().to_ascii_lowercase()
    }

    /// Windows from `info`, matching the reference `claudeLimits` reader.
    #[must_use]
    pub(crate) fn limits_from_info(info: &RateLimitInfo) -> Vec<RawLimit> {
        let mut out = Vec::new();
        for (kind, window) in &info.unified {
            out.push(RawLimit {
                kind: kind.clone(),
                used: window.utilization,
                resets_at: window.resets_at,
            });
        }
        let rate_type = info.rate_limit_type.as_deref().unwrap_or_default();
        if !rate_type.is_empty() && !info.unified.contains_key(rate_type) {
            match (info.utilization, info.status.as_deref()) {
                (Some(u), _) => out.push(RawLimit {
                    kind: rate_type.to_owned(),
                    used: u,
                    resets_at: info.resets_at,
                }),
                (None, Some("rejected")) => out.push(RawLimit {
                    kind: rate_type.to_owned(),
                    used: 1.0,
                    resets_at: info.resets_at,
                }),
                _ => {}
            }
        }
        out
    }

    /// Records what Claude Code said while answering (`NoteClaudeLimits`).
    pub(crate) fn note(&self, account: &AccountId, info: &RateLimitInfo, now: SystemTime) {
        let heard = self.raw_to_windows(&Self::limits_from_info(info));
        if heard.is_empty() {
            return;
        }
        let key = Self::key(account);
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = guard.entry(key).or_default();
        let mut merged = Vec::new();
        for window in &entry.windows {
            if !heard.iter().any(|h| h.name() == window.name()) {
                merged.push(window.clone());
            }
        }
        merged.extend(heard);
        sort_windows(&mut merged);
        entry.windows = merged;
        entry.heard_at = Some(now);
        entry.at = Some(now);
        entry.last_err = None;
    }

    /// Decides whether Claude Code's `/usage` should run for `account`.
    #[must_use]
    pub(crate) fn usage_read_decision(
        &self,
        account: &AccountId,
        now: SystemTime,
        active: bool,
        card_refresh: bool,
        intent: QuotaIntent,
    ) -> ReadDecision {
        let key = Self::key(account);
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (tried, heard_at, wait, asked) = guard
            .get(&key)
            .map_or((None, None, Duration::ZERO, 0), |entry| {
                (entry.tried, entry.heard_at, entry.wait, entry.asked)
            });
        usage_cadence::decide_read(
            tried,
            heard_at,
            wait,
            asked,
            now,
            active,
            card_refresh,
            intent,
        )
    }

    /// Records that a `/usage` run is starting and draws the next wait.
    pub(crate) fn mark_usage_tried(&self, account: &AccountId, now: SystemTime) {
        let key = Self::key(account);
        let wait = usage_cadence::usage_wait();
        let asked = usage_cadence::current_ask_generation();
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = guard.entry(key).or_default();
        entry.tried = Some(now);
        entry.wait = wait;
        entry.asked = asked;
    }

    /// Windows and time a `/usage` decision must use.
    ///
    /// In-memory windows win. An empty entry falls through to `disk`. Call
    /// this before the command: a rate-limit header while it runs refreshes
    /// the entry time and must not make an old reading look fresh.
    pub(crate) fn snapshot_prior(
        &self,
        account: &AccountId,
        disk: Option<PriorReading>,
    ) -> (Vec<QuotaWindow>, Option<SystemTime>) {
        let key = Self::key(account);
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = guard.get(&key)
            && !entry.windows.is_empty()
        {
            return (entry.windows.clone(), entry.at);
        }
        match disk {
            Some(reading) => (reading.windows, Some(reading.read_at)),
            None => (Vec::new(), None),
        }
    }

    /// Keeps a successful `/usage` parse (`whole` reading).
    ///
    /// `prev` and `prev_at` are the snapshot taken before the command. The
    /// windows stored here are the ones a later restart loads.
    pub(crate) fn record_usage(
        &self,
        account: &AccountId,
        windows: Vec<QuotaWindow>,
        now: SystemTime,
        prev: &[QuotaWindow],
        prev_at: Option<SystemTime>,
    ) -> Vec<QuotaWindow> {
        self.store_whole(account, windows, now, prev, prev_at)
    }

    fn store_whole(
        &self,
        account: &AccountId,
        windows: Vec<QuotaWindow>,
        now: SystemTime,
        prev: &[QuotaWindow],
        prev_at: Option<SystemTime>,
    ) -> Vec<QuotaWindow> {
        let key = Self::key(account);
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prior = guard.get(&key).map(|entry| PriorSnapshot {
            heard_at: entry.heard_at,
            tried: entry.tried,
            wait: entry.wait,
            asked: entry.asked,
        });
        let heard = prior.as_ref().and_then(|row| row.heard_at);
        let (tried, wait, asked) = prior.as_ref().map_or((None, Duration::ZERO, 0), |row| {
            (row.tried, row.wait, row.asked)
        });
        let mut entry = Entry {
            windows: finish_whole(prev, prev_at, windows, now),
            heard_at: heard,
            at: Some(now),
            whole: true,
            last_err: None,
            tried: Some(tried.unwrap_or(now)),
            wait,
            asked,
        };
        sort_windows(&mut entry.windows);
        let stored = entry.windows.clone();
        guard.insert(key, entry);
        stored
    }

    /// Remembers a failed `/usage` read while keeping prior windows.
    pub(crate) fn record_usage_failure(&self, account: &AccountId, err: &PluginError) {
        let key = Self::key(account);
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = guard.entry(key).or_default();
        entry.last_err = Some(err.to_string());
    }

    /// Elapsed windows kept for `account`, when still valid.
    #[must_use]
    pub(crate) fn kept_windows(
        &self,
        account: &AccountId,
        now: SystemTime,
    ) -> Option<Vec<QuotaWindow>> {
        let key = Self::key(account);
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = guard.get(&key)?;
        if entry.windows.is_empty() {
            return None;
        }
        Some(elapsed(&entry.windows, now))
    }

    /// Returns kept windows when `/usage` failed but a recent stream told allowance.
    #[must_use]
    pub(crate) fn fallback_on_usage_error(
        &self,
        account: &AccountId,
        err: &PluginError,
        now: SystemTime,
    ) -> Option<Vec<QuotaWindow>> {
        if matches!(err.kind(), PluginErrorKind::Auth) {
            return None;
        }
        let msg = err.to_string();
        if usage::line_denies_account(&msg) {
            return None;
        }
        let key = Self::key(account);
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = guard.get(&key)?;
        let heard_at = entry.heard_at?;
        if now.duration_since(heard_at).ok()? > HEARD_TTL {
            return None;
        }
        if entry.windows.is_empty() {
            return None;
        }
        Some(elapsed(&entry.windows, now))
    }

    fn raw_to_windows(&self, raw: &[RawLimit]) -> Vec<QuotaWindow> {
        let mut out = Vec::new();
        for limit in raw {
            let Some(spec) = KINDS.iter().find(|k| k.kind == limit.kind) else {
                continue;
            };
            let used_pct = (limit.used * 100.0).clamp(0.0, 100.0).floor() as u64;
            let reset = limit
                .resets_at
                .filter(|&s| s > 0)
                .map(|s| UNIX_EPOCH + Duration::from_secs(s));
            let Ok(mut window) = QuotaWindow::new(spec.name, used_pct, Some(100), reset) else {
                continue;
            };
            if let Some(model) = spec.model {
                window = window.with_scope_model(model);
            }
            out.push(window);
        }
        out
    }
}

/// Cadence fields kept across a whole-reading replace.
struct PriorSnapshot {
    heard_at: Option<SystemTime>,
    tried: Option<SystemTime>,
    wait: Duration,
    asked: u64,
}

/// A whole reading loaded from the account directory.
#[derive(Clone, Debug)]
pub(crate) struct PriorReading {
    pub(crate) windows: Vec<QuotaWindow>,
    pub(crate) read_at: SystemTime,
}

#[derive(Serialize, Deserialize)]
struct SavedReading {
    read_at: String,
    windows: Vec<SavedWindow>,
}

#[derive(Serialize, Deserialize)]
struct SavedWindow {
    name: String,
    used: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    limit: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resets_at: Option<String>,
}

/// Sort windows in Claude Code kind order.
pub(crate) fn sort_windows(windows: &mut [QuotaWindow]) {
    windows.sort_by_key(kind_order);
}

fn kind_order(window: &QuotaWindow) -> usize {
    KINDS
        .iter()
        .position(|k| k.name == window.name())
        .unwrap_or(KINDS.len())
}

/// Adds the not-started 5-hour window when `/usage` left it out.
#[must_use]
pub(crate) fn with_unstarted(mut windows: Vec<QuotaWindow>) -> Vec<QuotaWindow> {
    if !windows.iter().any(|w| w.name() == "5 hours")
        && let Ok(w) = QuotaWindow::new("5 hours", 0, Some(100), None)
    {
        windows.push(w);
    }
    sort_windows(&mut windows);
    windows
}

/// A whole reading, with the unstarted 5-hour window left out when the
/// vendor sends none or the plan already said it has no meters.
#[must_use]
pub(crate) fn finish_whole(
    prev: &[QuotaWindow],
    prev_at: Option<SystemTime>,
    mut windows: Vec<QuotaWindow>,
    now: SystemTime,
) -> Vec<QuotaWindow> {
    if plan_reports_no_meters(&windows) || vendor_sends_no_five_hour(prev, prev_at, &windows, now) {
        sort_windows(&mut windows);
        windows
    } else {
        with_unstarted(windows)
    }
}

/// Whether `windows` is a plan Claude Code reported with no meters.
fn plan_reports_no_meters(windows: &[QuotaWindow]) -> bool {
    windows
        .iter()
        .any(|window| window.name() == usage::NO_METERS_WINDOW)
}

/// Whether `current`, read at `now`, is an account the vendor sends no
/// 5-hour window for.
///
/// An idle account and an account with no such window read the same until
/// the week is seen to rise inside five hours. After that, a reading that
/// still has no 5-hour window stays without one, including the copy kept
/// on disk, until a reading includes a real one. A previous no-meters aside
/// is not that evidence.
#[must_use]
pub(crate) fn vendor_sends_no_five_hour(
    prev: &[QuotaWindow],
    prev_at: Option<SystemTime>,
    current: &[QuotaWindow],
    now: SystemTime,
) -> bool {
    if has_five_hour(current) || prev.is_empty() {
        return false;
    }
    if !has_five_hour(prev) {
        // A no-meters aside has no 5-hour window, and that is not evidence
        // the vendor sends none. Only a previous reading that already had
        // real allowance windows, and still no 5-hour window, stays out.
        return has_allowance_window(prev);
    }
    let Some(prev_at) = prev_at else {
        return false;
    };
    let age = now.duration_since(prev_at).unwrap_or(Duration::ZERO);
    if age >= FIVE_HOURS {
        return false;
    }
    let (Some(before), Some(after)) = (week_with_reset(prev), week_with_reset(current)) else {
        return false;
    };
    let (Some(before_reset), Some(after_reset)) = (before.resets_at(), after.resets_at()) else {
        return false;
    };
    after.used() > before.used() && abs_between(before_reset, after_reset) < SAME_WEEK
}

fn has_five_hour(windows: &[QuotaWindow]) -> bool {
    windows.iter().any(|window| window.name() == "5 hours")
}

/// A session or weekly allowance window, not an aside or a credits line.
fn has_allowance_window(windows: &[QuotaWindow]) -> bool {
    windows.iter().any(|window| {
        let name = window.name();
        name == "5 hours" || name == "7 days" || name.starts_with("7 days ")
    })
}

fn week_with_reset(windows: &[QuotaWindow]) -> Option<&QuotaWindow> {
    windows
        .iter()
        .find(|window| window.name() == "7 days" && window.resets_at().is_some())
}

fn abs_between(left: SystemTime, right: SystemTime) -> Duration {
    left.duration_since(right)
        .or_else(|_| right.duration_since(left))
        .unwrap_or(Duration::MAX)
}

/// Reads the whole reading saved beside an account. `None` when the file
/// is missing or not a reading this process wrote.
#[must_use]
pub(crate) fn load_reading(path: &Path) -> Option<PriorReading> {
    let text = std::fs::read_to_string(path).ok()?;
    let saved: SavedReading = serde_json::from_str(&text).ok()?;
    let read_at = hofund_plugin_sdk::time::parse_rfc3339(&saved.read_at)?;
    let mut windows = Vec::new();
    for item in saved.windows {
        let reset = item
            .resets_at
            .as_deref()
            .and_then(hofund_plugin_sdk::time::parse_rfc3339);
        let Ok(window) = QuotaWindow::new(item.name, item.used, item.limit, reset) else {
            continue;
        };
        windows.push(window);
    }
    if windows.is_empty() {
        return None;
    }
    Some(PriorReading { windows, read_at })
}

/// Writes `windows` as the account's last whole reading.
pub(crate) fn save_reading(
    path: &Path,
    windows: &[QuotaWindow],
    read_at: SystemTime,
) -> Result<(), PluginError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|_| PluginError::io("cannot store the Claude usage reading"))?;
    }
    let saved = SavedReading {
        read_at: hofund_plugin_sdk::time::format_rfc3339(read_at),
        windows: windows
            .iter()
            .map(|window| SavedWindow {
                name: window.name().to_owned(),
                used: window.used(),
                limit: window.limit(),
                resets_at: window
                    .resets_at()
                    .map(hofund_plugin_sdk::time::format_rfc3339),
            })
            .collect(),
    };
    let bytes = serde_json::to_vec(&saved)
        .map_err(|_| PluginError::io("cannot store the Claude usage reading"))?;
    let tmp = path.with_extension("json.tmp");
    {
        use std::io::Write as _;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&tmp)
            .map_err(|_| PluginError::io("cannot store the Claude usage reading"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|_| PluginError::io("cannot store the Claude usage reading"))?;
        }
        file.write_all(&bytes)
            .map_err(|_| PluginError::io("cannot store the Claude usage reading"))?;
    }
    std::fs::rename(&tmp, path)
        .map_err(|_| PluginError::io("cannot store the Claude usage reading"))
}

/// Zeros windows whose reset time has passed.
#[must_use]
pub(crate) fn elapsed(windows: &[QuotaWindow], now: SystemTime) -> Vec<QuotaWindow> {
    windows
        .iter()
        .map(|window| {
            if window.resets_at().is_some_and(|reset| !now.lt(&reset)) {
                let Ok(mut fresh) = QuotaWindow::new(window.name(), 0, window.limit(), None) else {
                    return window.clone();
                };
                fresh = fresh.with_blocking(window.blocking());
                if let Some(model) = window.scope_model() {
                    fresh = fresh.with_scope_model(model);
                }
                if !window.models().is_empty() {
                    fresh = fresh.with_models(window.models().to_vec());
                }
                if !window.excluded_models().is_empty() {
                    fresh = fresh.with_excluded_models(window.excluded_models().to_vec());
                }
                if !window.display().is_empty() {
                    fresh = fresh.with_display(window.display());
                }
                fresh
            } else {
                window.clone()
            }
        })
        .collect()
}

/// Passive cooldown hints from an exhausted event window.
pub(crate) fn observe_from_info(host: &HostContext, info: &RateLimitInfo) {
    for limit in Store::limits_from_info(info) {
        let used_pct = (limit.used * 100.0).clamp(0.0, 100.0).floor() as u64;
        if used_pct < 100 {
            continue;
        }
        let Some(spec) = KINDS.iter().find(|k| k.kind == limit.kind) else {
            continue;
        };
        let mut observation = host.observe().observation().with_window(spec.name);
        observation = observation.with_limit(100).with_remaining(0);
        if let Some(at) = limit.resets_at.filter(|&s| s > 0) {
            let reset = UNIX_EPOCH + Duration::from_secs(at);
            if let Ok(after) = reset.duration_since(host.clock().now()) {
                observation = observation.with_window_reset(after);
            }
        }
        let _ = host.observe().observe(observation);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::decode::{Decoder, Step};

    fn record(
        store: &Store,
        id: &AccountId,
        windows: Vec<QuotaWindow>,
        now: SystemTime,
        disk: Option<PriorReading>,
    ) -> Vec<QuotaWindow> {
        let (prev, prev_at) = store.snapshot_prior(id, disk);
        store.record_usage(id, windows, now, &prev, prev_at)
    }

    fn info_from_json(value: &str) -> RateLimitInfo {
        let mut decoder = Decoder::new();
        let step = decoder.push_line(value).expect("line");
        match step {
            Step::RateLimit(info) => info,
            _ => panic!("expected rate limit"),
        }
    }

    #[test]
    fn r5_48_claude_limits_parses_unified_and_top_level() {
        let info = info_from_json(
            r#"{"type":"rate_limit_event","status":"allowed","rate_limit_info":{"status":"allowed","resetsAt":1800000000,"rateLimitType":"five_hour","utilization":0.42,"unifiedWindows":{"five_hour":{"utilization":0.42,"resetsAt":1800000000},"seven_day":{"utilization":0.1,"resetsAt":1800500000}}}}"#,
        );
        let mut limits = Store::limits_from_info(&info);
        limits.sort_by(|a, b| a.kind.cmp(&b.kind));
        assert_eq!(
            limits,
            vec![
                RawLimit {
                    kind: "five_hour".into(),
                    used: 0.42,
                    resets_at: Some(1800000000),
                },
                RawLimit {
                    kind: "seven_day".into(),
                    used: 0.1,
                    resets_at: Some(1800500000),
                },
            ]
        );
        let top = info_from_json(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","rateLimitType":"seven_day","utilization":0.9,"resetsAt":5}}"#,
        );
        assert_eq!(
            Store::limits_from_info(&top),
            vec![RawLimit {
                kind: "seven_day".into(),
                used: 0.9,
                resets_at: Some(5),
            }]
        );
        let rejected = info_from_json(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"five_hour","resetsAt":7}}"#,
        );
        assert_eq!(
            Store::limits_from_info(&rejected),
            vec![RawLimit {
                kind: "five_hour".into(),
                used: 1.0,
                resets_at: Some(7),
            }]
        );
    }

    #[test]
    fn r5_49_kind_names_order_and_rejected_at_100_percent() {
        let store = Store::new();
        let id = AccountId::from_static("claude-r5-49");
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let info = info_from_json(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"five_hour","resetsAt":1800000100,"unifiedWindows":{"seven_day_opus":{"utilization":0.5,"resetsAt":1800000200}}}}"#,
        );
        store.note(&id, &info, now);
        let windows = store.kept_windows(&id, now).expect("kept");
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].name(), "5 hours");
        assert_eq!(windows[0].used(), 100);
        assert_eq!(windows[1].name(), "7 days · Opus");
        assert_eq!(windows[1].used(), 50);
        assert_eq!(
            windows[1].scope_model().map(|m| m.to_string()),
            Some("opus".into())
        );
    }

    #[test]
    fn r5_50_heard_merges_over_same_named_usage_window() {
        let store = Store::new();
        let id = AccountId::from_static("claude-r5-50");
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let usage = vec![
            QuotaWindow::new("5 hours", 10, Some(100), None).expect("w"),
            QuotaWindow::new("7 days", 4, Some(100), None).expect("w"),
        ];
        record(&store, &id, usage, now, None);
        let info = info_from_json(
            r#"{"type":"rate_limit_event","rate_limit_info":{"unifiedWindows":{"five_hour":{"utilization":0.8,"resetsAt":1800000300}}}}"#,
        );
        store.note(&id, &info, now);
        let windows = store.kept_windows(&id, now).expect("kept");
        assert_eq!(windows[0].used(), 80);
        assert_eq!(windows[1].name(), "7 days");
        assert_eq!(windows[1].used(), 4);
    }

    #[test]
    fn r5_51_one_hour_stand_in_after_usage_quota_error() {
        let store = Store::new();
        let id = AccountId::from_static("claude-r5-51");
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let info = info_from_json(
            r#"{"type":"rate_limit_event","rate_limit_info":{"unifiedWindows":{"five_hour":{"utilization":0.33,"resetsAt":1800000400}}}}"#,
        );
        store.note(&id, &info, now);
        let err = PluginError::quota("Claude Code's /usage told no allowance: rate limit");
        assert!(
            store
                .fallback_on_usage_error(&id, &err, now + Duration::from_secs(1800))
                .is_some()
        );
        assert!(
            store
                .fallback_on_usage_error(&id, &err, now + HEARD_TTL + Duration::from_secs(1))
                .is_none()
        );
    }

    #[test]
    fn d237_idle_week_keeps_an_unstarted_five_hour_window() {
        let store = Store::new();
        let id = AccountId::from_static("claude-d237-idle");
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let reset = now + Duration::from_secs(72 * 60 * 60);
        record(
            &store,
            &id,
            vec![QuotaWindow::new("7 days", 10, Some(100), Some(reset)).expect("week")],
            now,
            None,
        );
        let windows = store.kept_windows(&id, now).expect("kept");
        let five = windows
            .iter()
            .find(|w| w.name() == "5 hours")
            .expect("5 hours");
        assert_eq!(five.used(), 0);
        assert!(five.resets_at().is_none());
    }

    #[test]
    fn d237_week_rose_inside_five_hours_omits_the_invented_window() {
        let store = Store::new();
        let id = AccountId::from_static("claude-d237-rose");
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let reset = now + Duration::from_secs(72 * 60 * 60);
        let week = |used| QuotaWindow::new("7 days", used, Some(100), Some(reset)).expect("week");
        record(&store, &id, vec![week(10)], now, None);
        let later = now + Duration::from_secs(60 * 60);
        record(&store, &id, vec![week(12)], later, None);
        let windows = store.kept_windows(&id, later).expect("kept");
        assert!(
            windows.iter().all(|w| w.name() != "5 hours"),
            "invented window stayed: {windows:?}"
        );
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].used(), 12);
        let idle_again = later + Duration::from_secs(30 * 60);
        record(&store, &id, vec![week(12)], idle_again, None);
        let stayed = store.kept_windows(&id, idle_again).expect("kept");
        assert!(stayed.iter().all(|window| window.name() != "5 hours"));
    }

    #[test]
    fn d237_rise_five_hours_later_still_adds_the_unstarted_window() {
        let store = Store::new();
        let id = AccountId::from_static("claude-d237-old");
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let reset = now + Duration::from_secs(72 * 60 * 60);
        let week = |used| QuotaWindow::new("7 days", used, Some(100), Some(reset)).expect("week");
        record(&store, &id, vec![week(10)], now, None);
        let later = now + FIVE_HOURS;
        record(&store, &id, vec![week(12)], later, None);
        let windows = store.kept_windows(&id, later).expect("kept");
        assert!(windows.iter().any(|window| window.name() == "5 hours"));
    }

    #[test]
    fn d237_week_reset_an_hour_away_does_not_count_as_the_same_week() {
        let store = Store::new();
        let id = AccountId::from_static("claude-d237-reset");
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let reset = now + Duration::from_secs(72 * 60 * 60);
        record(
            &store,
            &id,
            vec![QuotaWindow::new("7 days", 10, Some(100), Some(reset)).expect("week")],
            now,
            None,
        );
        let later = now + Duration::from_secs(60 * 60);
        let moved = reset + SAME_WEEK;
        record(
            &store,
            &id,
            vec![QuotaWindow::new("7 days", 12, Some(100), Some(moved)).expect("week")],
            later,
            None,
        );
        let windows = store.kept_windows(&id, later).expect("kept");
        assert!(windows.iter().any(|window| window.name() == "5 hours"));
    }

    #[test]
    fn d237_reading_file_keeps_the_omission_until_a_real_window() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join(READING_FILE);
        let id = AccountId::from_static("claude-d237-disk");
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let reset = now + Duration::from_secs(72 * 60 * 60);
        let week = |used| QuotaWindow::new("7 days", used, Some(100), Some(reset)).expect("week");
        let store = Store::new();
        let first = record(&store, &id, vec![week(10)], now, None);
        save_reading(&path, &first, now).expect("save");
        let later = now + Duration::from_secs(60 * 60);
        let omitted = record(&store, &id, vec![week(12)], later, None);
        assert!(omitted.iter().all(|window| window.name() != "5 hours"));
        save_reading(&path, &omitted, later).expect("save");

        let fresh = Store::new();
        let disk = load_reading(&path);
        let stayed = record(
            &fresh,
            &id,
            vec![week(12)],
            later + Duration::from_secs(60),
            disk,
        );
        assert!(
            stayed.iter().all(|window| window.name() != "5 hours"),
            "restart invented a window: {stayed:?}"
        );
        let session_reset = later + Duration::from_secs(4 * 60 * 60);
        let brought = record(
            &fresh,
            &id,
            vec![
                QuotaWindow::new("5 hours", 3, Some(100), Some(session_reset)).expect("session"),
                week(13),
            ],
            later + Duration::from_secs(120),
            None,
        );
        let five = brought
            .iter()
            .find(|window| window.name() == "5 hours")
            .expect("real window");
        assert_eq!(five.used(), 3);
        assert_eq!(five.resets_at(), Some(session_reset));
    }

    #[test]
    fn d237_reading_file_sees_a_rise_after_restart() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join(READING_FILE);
        let id = AccountId::from_static("claude-d237-restart-rise");
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let reset = now + Duration::from_secs(72 * 60 * 60);
        let week = |used| QuotaWindow::new("7 days", used, Some(100), Some(reset)).expect("week");
        let store = Store::new();
        let idle = record(&store, &id, vec![week(10)], now, None);
        assert!(idle.iter().any(|window| window.name() == "5 hours"));
        save_reading(&path, &idle, now).expect("save");

        let fresh = Store::new();
        let later = now + Duration::from_secs(60 * 60);
        let omitted = record(&fresh, &id, vec![week(12)], later, load_reading(&path));
        assert!(
            omitted.iter().all(|window| window.name() != "5 hours"),
            "restart kept the invented window: {omitted:?}"
        );
    }

    #[test]
    fn d238_no_meters_reading_does_not_invent_a_five_hour_window() {
        let store = Store::new();
        let id = AccountId::from_static("claude-d238-none");
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let aside = QuotaWindow::new(usage::NO_METERS_WINDOW, 0, None, None)
            .expect("aside")
            .with_display(usage::NO_METERS_DISPLAY)
            .with_blocking(false);
        let stored = record(&store, &id, vec![aside], now, None);
        assert!(stored.iter().all(|window| window.name() != "5 hours"));
        assert_eq!(stored[0].display(), usage::NO_METERS_DISPLAY);
        assert!(!stored[0].blocking());
    }

    #[test]
    fn d237_no_meters_aside_does_not_latch_a_missing_five_hour_window() {
        let store = Store::new();
        let id = AccountId::from_static("claude-d237-aside");
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let reset = now + Duration::from_secs(72 * 60 * 60);
        let week = |used| QuotaWindow::new("7 days", used, Some(100), Some(reset)).expect("week");
        let aside = QuotaWindow::new(usage::NO_METERS_WINDOW, 0, None, None)
            .expect("aside")
            .with_display(usage::NO_METERS_DISPLAY)
            .with_blocking(false);
        let stored = record(&store, &id, vec![aside], now, None);
        assert!(stored.iter().all(|window| window.name() != "5 hours"));

        let later = now + Duration::from_secs(60);
        let idle = record(&store, &id, vec![week(10)], later, None);
        let five = idle
            .iter()
            .find(|window| window.name() == "5 hours")
            .expect("unstarted");
        assert_eq!(five.used(), 0);
        assert!(five.resets_at().is_none());

        let risen = record(
            &store,
            &id,
            vec![week(12)],
            later + Duration::from_secs(60 * 60),
            None,
        );
        assert!(
            risen.iter().all(|window| window.name() != "5 hours"),
            "a risen week kept a 5-hour window: {risen:?}"
        );

        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join(READING_FILE);
        save_reading(&path, &stored, now).expect("save");
        let fresh = Store::new();
        let after = record(&fresh, &id, vec![week(10)], later, load_reading(&path));
        let five = after
            .iter()
            .find(|window| window.name() == "5 hours")
            .expect("unstarted after restart");
        assert_eq!(five.used(), 0);
    }

    #[test]
    fn d237_header_during_usage_does_not_refresh_the_previous_reading() {
        let store = Store::new();
        let id = AccountId::from_static("claude-d237-snapshot");
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let reset = now + Duration::from_secs(72 * 60 * 60);
        let week = |used| QuotaWindow::new("7 days", used, Some(100), Some(reset)).expect("week");
        let session = QuotaWindow::new(
            "5 hours",
            1,
            Some(100),
            Some(now + Duration::from_secs(60 * 60)),
        )
        .expect("session");
        record(&store, &id, vec![session, week(10)], now, None);
        let (prev, prev_at) = store.snapshot_prior(&id, None);
        assert_eq!(prev_at, Some(now));

        let later = now + FIVE_HOURS;
        let info = info_from_json(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","rateLimitType":"seven_day","utilization":0.2,"resetsAt":1800000000}}"#,
        );
        store.note(&id, &info, later);
        assert_eq!(store.snapshot_prior(&id, None).1, Some(later));

        let stored = store.record_usage(&id, vec![week(12)], later, &prev, prev_at);
        let five = stored
            .iter()
            .find(|window| window.name() == "5 hours")
            .expect("unstarted after a stale reading");
        assert_eq!(five.used(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn d237_reading_file_is_created_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join(READING_FILE);
        let window = QuotaWindow::new("7 days", 1, Some(100), None).expect("week");
        save_reading(&path, &[window], UNIX_EPOCH + Duration::from_secs(10)).expect("save");
        let mode = std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn d237_save_reading_reports_a_failure() {
        let dir = tempfile::tempdir().expect("dir");
        let blocker = dir.path().join("blocked");
        std::fs::write(&blocker, b"x").expect("file");
        let path = blocker.join(READING_FILE);
        let window = QuotaWindow::new("7 days", 1, Some(100), None).expect("week");
        let err = save_reading(&path, &[window], UNIX_EPOCH).expect_err("save");
        assert_eq!(err.kind(), PluginErrorKind::Io);
        assert_eq!(err.message(), "cannot store the Claude usage reading");
    }

    #[test]
    fn r5_52_with_unstarted_adds_five_hour_when_missing() {
        let windows = with_unstarted(vec![
            QuotaWindow::new("7 days", 2, Some(100), None).expect("w"),
        ]);
        assert_eq!(windows[0].name(), "5 hours");
        assert_eq!(windows[0].used(), 0);
        assert_eq!(windows[1].name(), "7 days");
    }

    #[test]
    fn r5_53_new_observation_clears_failed_reading() {
        let store = Store::new();
        let id = AccountId::from_static("claude-r5-53");
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        store.record_usage_failure(&id, &PluginError::transport("temporary"));
        let info = info_from_json(
            r#"{"type":"rate_limit_event","rate_limit_info":{"unifiedWindows":{"seven_day":{"utilization":0.1,"resetsAt":9}}}}"#,
        );
        store.note(&id, &info, now);
        assert!(store.kept_windows(&id, now).is_some());
    }

    #[test]
    fn r5_56_setup_token_quota_path_uses_kept_heard() {
        let store = Store::new();
        let id = AccountId::from_static("claude-r5-56");
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert!(store.kept_windows(&id, now).is_none());
        let info = info_from_json(
            r#"{"type":"rate_limit_event","rate_limit_info":{"unifiedWindows":{"five_hour":{"utilization":0.12,"resetsAt":1800000500}}}}"#,
        );
        store.note(&id, &info, now);
        let windows = store.kept_windows(&id, now).expect("heard");
        assert_eq!(windows[0].name(), "5 hours");
        assert_eq!(windows[0].used(), 12);
    }

    #[test]
    fn r5_54_elapsed_zeros_past_reset() {
        let reset = UNIX_EPOCH + Duration::from_secs(100);
        let windows = vec![QuotaWindow::new("5 hours", 90, Some(100), Some(reset)).expect("w")];
        let out = elapsed(&windows, reset + Duration::from_secs(1));
        assert_eq!(out[0].used(), 0);
        assert!(out[0].resets_at().is_none());
    }
}
