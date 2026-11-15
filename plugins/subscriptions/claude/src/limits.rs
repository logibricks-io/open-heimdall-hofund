//! Allowance windows Claude Code tells in `rate_limit_event` envelopes, kept
//! per account when `/usage` is away or for setup-token accounts.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hofund_plugin_api::{AccountId, HostContext, PluginError, PluginErrorKind, QuotaWindow};

use crate::bridge::decode::RateLimitInfo;
use crate::usage;
use crate::usage_cadence::{self, QuotaIntent, ReadDecision};

/// How long stream-derived windows stand in for a failed `/usage` read.
pub(crate) const HEARD_TTL: Duration = Duration::from_secs(60 * 60);

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

    /// Keeps a successful `/usage` parse (`whole` reading).
    pub(crate) fn record_usage(
        &self,
        account: &AccountId,
        windows: Vec<QuotaWindow>,
        now: SystemTime,
    ) {
        let key = Self::key(account);
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prior = guard.get(&key);
        let heard = prior.and_then(|e| e.heard_at);
        let (tried, wait, asked) =
            prior.map_or((None, Duration::ZERO, 0), |e| (e.tried, e.wait, e.asked));
        let mut entry = Entry {
            windows: with_unstarted(windows),
            heard_at: heard,
            whole: true,
            last_err: None,
            tried: Some(tried.unwrap_or(now)),
            wait,
            asked,
        };
        sort_windows(&mut entry.windows);
        guard.insert(key, entry);
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
        store.record_usage(&id, usage, now);
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
