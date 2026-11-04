//! Quota usage and balance alerts: thresholds, once-per-window-run marks,
//! and notification text with masked account identifiers.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hofund_control_proto::Quota;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::quota::WindowInfo;

/// How far apart two reset times may be and still be the same window run.
const SAME_RUN: Duration = Duration::from_secs(10 * 60);
/// How long to keep marks for cards no longer read.
const MARK_KEEP: Duration = Duration::from_secs(40 * 24 * 60 * 60);

/// One alert the daemon may emit.
#[derive(Clone, Debug, PartialEq)]
pub struct QuotaAlert {
    /// Provider or subscription id.
    pub provider: String,
    /// Display name.
    pub name: String,
    /// Account id as read from the vendor.
    pub user: String,
    /// User-chosen alias when set.
    pub alias: String,
    /// Window name for a usage alert.
    pub window: String,
    /// Percent used for a usage alert.
    pub used: f64,
    /// Window reset time when known.
    pub resets_at: Option<SystemTime>,
    /// Balance text for a balance alert.
    pub balance: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AlertSettings {
    #[serde(default)]
    usage_alert: u8,
    #[serde(default)]
    balance_alert: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct AlertMark {
    at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    until: Option<u64>,
}

/// Persisted alert thresholds and fired marks beside the daemon state directory.
pub struct QuotaAlertStore {
    root: Option<PathBuf>,
    settings: Mutex<AlertSettings>,
    marks: Mutex<BTreeMap<String, AlertMark>>,
}

impl QuotaAlertStore {
    /// Loads settings and marks from `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let settings = std::fs::read(root.join("quota-alert-settings.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let marks = std::fs::read(root.join("usage-alerts.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self {
            root: Some(root),
            settings: Mutex::new(settings),
            marks: Mutex::new(marks),
        }
    }

    /// In-memory store for tests.
    #[must_use]
    pub fn in_memory() -> Self {
        Self {
            root: None,
            settings: Mutex::new(AlertSettings::default()),
            marks: Mutex::new(BTreeMap::new()),
        }
    }

    /// Current usage alert percent, or 0 when off.
    #[must_use]
    pub fn usage_alert(&self) -> u8 {
        self.settings.lock().expect("alert settings").usage_alert
    }

    /// Current balance alert amount, or 0 when off.
    #[must_use]
    pub fn balance_alert(&self) -> f64 {
        self.settings.lock().expect("alert settings").balance_alert
    }

    /// Whether any alert threshold is enabled.
    #[must_use]
    pub fn any_enabled(&self) -> bool {
        let settings = self.settings.lock().expect("alert settings");
        settings.usage_alert > 0 || settings.balance_alert > 0.0
    }

    /// Sets the usage alert percent; 0 turns it off.
    pub fn set_usage_alert(&self, percent: u8) {
        self.settings.lock().expect("alert settings").usage_alert = percent;
        self.save_settings();
    }

    /// Sets the balance alert amount; 0 turns it off.
    pub fn set_balance_alert(&self, amount: f64) {
        self.settings.lock().expect("alert settings").balance_alert = amount;
        self.save_settings();
    }

    /// Returns alerts due for `cards` at `now`, updating marks.
    pub fn check(&self, cards: &[(Quota, Vec<WindowInfo>)], now: SystemTime) -> Vec<QuotaAlert> {
        let settings = self.settings.lock().expect("alert settings").clone();
        let mut marks = self.marks.lock().expect("alert marks").clone();
        let before = serde_json::to_vec(&marks).unwrap_or_default();
        let out = due_alerts(
            cards,
            &mut marks,
            settings.usage_alert,
            settings.balance_alert,
            now,
        );
        let after = serde_json::to_vec(&marks).unwrap_or_default();
        if after != before {
            *self.marks.lock().expect("alert marks") = marks;
            self.save_marks();
        }
        out
    }

    fn save_settings(&self) {
        let Some(root) = &self.root else {
            return;
        };
        let settings = self.settings.lock().expect("alert settings");
        if let Ok(bytes) = serde_json::to_vec_pretty(&*settings) {
            let _ = std::fs::write(root.join("quota-alert-settings.json"), bytes);
        }
    }

    fn save_marks(&self) {
        let Some(root) = &self.root else {
            return;
        };
        let marks = self.marks.lock().expect("alert marks");
        if let Ok(mut bytes) = serde_json::to_vec(&*marks) {
            bytes.push(b'\n');
            let _ = std::fs::write(root.join("usage-alerts.json"), bytes);
        }
    }
}

fn due_alerts(
    cards: &[(Quota, Vec<WindowInfo>)],
    marks: &mut BTreeMap<String, AlertMark>,
    pct: u8,
    bal: f64,
    now: SystemTime,
) -> Vec<QuotaAlert> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let now_secs = secs(now);

    for (quota, windows) in cards {
        if !quota.error.is_empty() || quota.as_of.is_some() {
            for key in marks.keys() {
                if key.starts_with(&alert_prefix(quota)) {
                    seen.insert(key.clone());
                }
            }
            continue;
        }
        for window in alert_windows(windows) {
            if !window.info.blocking {
                continue;
            }
            let key = window_mark_key(quota, &window);
            seen.insert(key.clone());
            let resets = window.info.reset(now);
            if pct == 0 || window.info.span.used < f64::from(pct) {
                marks.remove(&key);
                continue;
            }
            if let Some(mark) = marks.get(&key)
                && same_run(mark.until.map(from_secs), resets)
            {
                continue;
            }
            marks.insert(
                key,
                AlertMark {
                    at: now_secs,
                    until: resets.map(secs),
                },
            );
            out.push(QuotaAlert {
                provider: quota.provider.clone(),
                name: quota.name.clone(),
                user: quota.user.clone(),
                alias: quota.alias.clone(),
                window: window.info.span.name.clone(),
                used: window.info.span.used,
                resets_at: resets,
                balance: String::new(),
            });
        }
        if quota.balance.is_empty() {
            continue;
        }
        let key = format!("{}b", alert_prefix(quota));
        seen.insert(key.clone());
        let Some(amount) = balance_number(&quota.balance) else {
            continue;
        };
        if bal <= 0.0 || amount > bal {
            marks.remove(&key);
            continue;
        }
        if marks.contains_key(&key) {
            continue;
        }
        marks.insert(
            key,
            AlertMark {
                at: now_secs,
                until: None,
            },
        );
        out.push(QuotaAlert {
            provider: quota.provider.clone(),
            name: quota.name.clone(),
            user: quota.user.clone(),
            alias: quota.alias.clone(),
            window: String::new(),
            used: 0.0,
            resets_at: None,
            balance: quota.balance.clone(),
        });
    }

    marks.retain(|key, mark| {
        if seen.contains(key) {
            return true;
        }
        now_secs.saturating_sub(mark.at) <= MARK_KEEP.as_secs()
    });
    out
}

struct AlertWindow<'a> {
    info: &'a WindowInfo,
    dedupe: String,
}

fn alert_windows<'a>(windows: &'a [WindowInfo]) -> Vec<AlertWindow<'a>> {
    let mut out = Vec::new();
    let mut pools = HashSet::new();
    for info in windows {
        if !info.span.name.is_empty() {
            let dedupe = if info.span.pool.is_empty() {
                format!(
                    "{}|{}",
                    info.span.name,
                    info.scope_model.as_deref().unwrap_or("")
                )
            } else {
                format!("pool:{}", info.span.pool)
            };
            if !info.span.pool.is_empty() && !pools.insert(dedupe.clone()) {
                continue;
            }
            out.push(AlertWindow { info, dedupe });
        }
    }
    out
}

fn window_mark_key(quota: &Quota, window: &AlertWindow<'_>) -> String {
    format!(
        "{}w|{}",
        alert_prefix(quota),
        window.dedupe.replace('|', "/")
    )
}

fn alert_prefix(quota: &Quota) -> String {
    format!("{}|{}|", quota.provider, quota.user.to_ascii_lowercase())
}

fn same_run(was: Option<SystemTime>, now: Option<SystemTime>) -> bool {
    match (was, now) {
        (None, None) => true,
        (Some(was), Some(now)) => {
            was.duration_since(now)
                .map(|delta| delta < SAME_RUN)
                .unwrap_or(false)
                || now
                    .duration_since(was)
                    .map(|delta| delta < SAME_RUN)
                    .unwrap_or(false)
        }
        _ => false,
    }
}

/// Parses a vendor balance string into a numeric amount.
#[must_use]
pub fn balance_number(text: &str) -> Option<f64> {
    if text.contains('%') {
        return None;
    }
    static NUM: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let num = NUM.get_or_init(|| Regex::new(r"-?\d[\d,]*(?:\.\d+)?").expect("balance regex"));
    let found = num.find(text)?;
    let raw = found.as_str().replace(',', "");
    let mut value = raw.parse::<f64>().ok()?;
    if !value.is_finite() {
        return None;
    }
    let rest = text[found.end()..].trim();
    if rest.starts_with('k') || rest.starts_with('K') {
        value *= 1_000.0;
    } else if rest.starts_with('M') {
        value *= 1_000_000.0;
    }
    Some(value)
}

/// Masks an account id for alert text: alias when set, else redact email-like ids.
#[must_use]
pub fn mask_account(alias: &str, user: &str) -> String {
    if !alias.is_empty() {
        return alias.to_owned();
    }
    mask_user_id(user)
}

fn mask_user_id(user: &str) -> String {
    let trimmed = user.trim();
    if let Some((local, domain)) = trimmed.split_once('@') {
        let chars: Vec<char> = local.chars().collect();
        if chars.len() <= 2 {
            return format!("***@{domain}");
        }
        let prefix: String = chars[..2].iter().collect();
        let suffix: String = chars[chars.len().saturating_sub(2)..].iter().collect();
        return format!("{prefix}****{suffix}@{domain}");
    }
    let chars: Vec<char> = trimmed.chars().collect();
    if chars.len() <= 4 {
        return "*".repeat(chars.len());
    }
    let prefix: String = chars[..2].iter().collect();
    let suffix: String = chars[chars.len() - 2..].iter().collect();
    format!("{prefix}{}{suffix}", "*".repeat(chars.len() - 4))
}

/// English notification title and body for one alert.
#[must_use]
pub fn alert_text(alert: &QuotaAlert, balance_threshold: f64, now: SystemTime) -> (String, String) {
    let mut title = alert.name.clone();
    let shown = mask_account(&alert.alias, &alert.user);
    if !shown.is_empty() {
        title.push_str(" · ");
        title.push_str(&shown);
    }
    if alert.window.is_empty() {
        let line = format_balance_threshold(balance_threshold);
        let body = format!(
            "Balance down to {} (alert at {line})",
            mask_balance_phrase(&alert.balance)
        );
        return (title, body);
    }
    let pct = format!("{:.0}%", alert.used.round());
    let mut body = format!("{}: {} used", alert.window, pct);
    if let Some(at) = alert.resets_at {
        body.push_str(", resets ");
        body.push_str(&reset_clock(at, now));
    }
    (title, body)
}

fn format_balance_threshold(value: f64) -> String {
    let text = format!("{value}");
    text.trim_end_matches(".0").to_owned()
}

fn mask_balance_phrase(balance: &str) -> String {
    balance_number(balance).map_or_else(
        || balance.to_owned(),
        |value| {
            if balance.contains('$') {
                format!("${value}")
            } else if balance.contains("credit") {
                format!("{value} credits")
            } else {
                value.to_string()
            }
        },
    )
}

fn reset_clock(at: SystemTime, now: SystemTime) -> String {
    let format_local = |time: SystemTime| {
        let Ok(duration) = time.duration_since(UNIX_EPOCH) else {
            return String::new();
        };
        let seconds = duration.as_secs();
        let day = seconds / 86_400;
        let now_day = now
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() / 86_400)
            .unwrap_or(0);
        let time_of_day = seconds % 86_400;
        let hour = time_of_day / 3600;
        let minute = (time_of_day % 3600) / 60;
        let clock = format!("{hour:02}:{minute:02}");
        match day.saturating_sub(now_day) {
            0 => clock,
            1 => format!("tomorrow {clock}"),
            _ => clock,
        }
    };
    format_local(at)
}

fn secs(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn from_secs(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

#[cfg(test)]
mod tests {
    use super::{
        QuotaAlertStore, alert_text, balance_number, due_alerts, mask_account, mask_user_id,
    };
    use crate::quota::{WindowInfo, quota_time};
    use hofund_control_proto::{Quota, QuotaSpan};
    use std::time::{Duration, UNIX_EPOCH};

    fn card(
        provider: &str,
        user: &str,
        windows: Vec<WindowInfo>,
        balance: &str,
    ) -> (Quota, Vec<WindowInfo>) {
        (
            Quota {
                provider: provider.to_owned(),
                name: provider.to_owned(),
                user: user.to_owned(),
                balance: balance.to_owned(),
                windows: windows.iter().map(|info| info.span.clone()).collect(),
                ..Quota::default()
            },
            windows,
        )
    }

    fn window(
        name: &str,
        used: f64,
        reset: Option<std::time::SystemTime>,
        blocking: bool,
    ) -> WindowInfo {
        let mut span = QuotaSpan {
            name: name.to_owned(),
            used,
            remaining: (100.0 - used).max(0.0),
            ..QuotaSpan::default()
        };
        if let Some(at) = reset {
            span.resets_at = Some(quota_time(at));
        }
        WindowInfo::new(span).with_blocking(blocking)
    }

    #[test]
    fn al1_1_balance_number_parses_vendor_text() {
        assert_eq!(balance_number("$12.34"), Some(12.34));
        assert_eq!(balance_number("12.3k credits"), Some(12_300.0));
        assert_eq!(balance_number("45%"), None);
    }

    #[test]
    fn al1_2_mask_account_uses_alias_or_redacts_user() {
        assert_eq!(mask_account("work", "secret@example.test"), "work");
        assert_eq!(mask_user_id("secret@example.test"), "se****et@example.test");
    }

    #[test]
    fn al1_3_usage_alert_fires_once_per_window_run() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000);
        let reset = now + Duration::from_secs(3600);
        let cards = vec![card(
            "sub",
            "a@x.io",
            vec![window("Weekly", 85.0, Some(reset), true)],
            "",
        )];
        let mut marks = Default::default();
        let first = due_alerts(&cards, &mut marks, 80, 0.0, now);
        assert_eq!(first.len(), 1);
        let second = due_alerts(&cards, &mut marks, 80, 0.0, now + Duration::from_secs(60));
        assert!(second.is_empty());
        let new_run = reset + Duration::from_secs(601);
        let cards = vec![card(
            "sub",
            "a@x.io",
            vec![window("Weekly", 85.0, Some(new_run), true)],
            "",
        )];
        let third = due_alerts(&cards, &mut marks, 80, 0.0, new_run);
        assert_eq!(third.len(), 1);
    }

    #[test]
    fn al1_4_set_aside_windows_are_excluded() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000);
        let cards = vec![card(
            "sub",
            "a@x.io",
            vec![window("On demand", 95.0, None, false)],
            "",
        )];
        let mut marks = Default::default();
        assert!(due_alerts(&cards, &mut marks, 80, 0.0, now).is_empty());
    }

    #[test]
    fn al1_5_balance_alert_fires_once_until_topped_up() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000);
        let cards = vec![card("p", "a@x.io", vec![], "4 credits")];
        let mut marks = Default::default();
        assert_eq!(due_alerts(&cards, &mut marks, 0, 5.0, now).len(), 1);
        assert!(due_alerts(&cards, &mut marks, 0, 5.0, now).is_empty());
        let topped = vec![card("p", "a@x.io", vec![], "6 credits")];
        assert!(due_alerts(&topped, &mut marks, 0, 5.0, now).is_empty());
        let low = vec![card("p", "a@x.io", vec![], "3 credits")];
        assert_eq!(due_alerts(&low, &mut marks, 0, 5.0, now).len(), 1);
    }

    #[test]
    fn al1_13_thresholds_are_inclusive() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000);
        let reset = Some(now + Duration::from_secs(3600));
        let at = vec![card(
            "sub",
            "a@x.io",
            vec![window("Weekly", 80.0, reset, true)],
            "",
        )];
        let below = vec![card(
            "sub",
            "b@x.io",
            vec![window("Weekly", 79.5, reset, true)],
            "",
        )];
        let mut marks = Default::default();
        assert_eq!(due_alerts(&at, &mut marks, 80, 0.0, now).len(), 1);
        assert!(due_alerts(&below, &mut marks, 80, 0.0, now).is_empty());
        let equal = vec![card("p", "c@x.io", vec![], "5 credits")];
        let above = vec![card("p", "d@x.io", vec![], "5.5 credits")];
        assert_eq!(due_alerts(&equal, &mut marks, 0, 5.0, now).len(), 1);
        assert!(due_alerts(&above, &mut marks, 0, 5.0, now).is_empty());
    }

    #[test]
    fn al1_14_usage_alert_rearms_after_dropping_below() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000);
        let reset = Some(now + Duration::from_secs(3600));
        let high = vec![card(
            "sub",
            "a@x.io",
            vec![window("Weekly", 90.0, reset, true)],
            "",
        )];
        let low = vec![card(
            "sub",
            "a@x.io",
            vec![window("Weekly", 10.0, reset, true)],
            "",
        )];
        let mut marks = Default::default();
        assert_eq!(due_alerts(&high, &mut marks, 80, 0.0, now).len(), 1);
        assert!(due_alerts(&low, &mut marks, 80, 0.0, now).is_empty());
        assert_eq!(due_alerts(&high, &mut marks, 80, 0.0, now).len(), 1);
    }

    #[test]
    fn al1_15_failed_or_cached_cards_do_not_alert() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000);
        let reset = Some(now + Duration::from_secs(3600));
        let mut failed = card(
            "sub",
            "a@x.io",
            vec![window("Weekly", 95.0, reset, true)],
            "1 credits",
        );
        failed.0.error = "sign-in expired".to_owned();
        let mut cached = card(
            "sub",
            "b@x.io",
            vec![window("Weekly", 95.0, reset, true)],
            "1 credits",
        );
        cached.0.as_of = Some(quota_time(now));
        let mut marks = Default::default();
        assert!(due_alerts(&[failed, cached], &mut marks, 80, 5.0, now).is_empty());
        assert!(marks.is_empty());
    }

    #[test]
    fn al1_16_mask_user_id_handles_non_ascii() {
        assert_eq!(
            mask_user_id("\u{e9}lodie@example.test"),
            "\u{e9}l****ie@example.test"
        );
        assert_eq!(
            mask_user_id("\u{e9}\u{e8}\u{ea}\u{eb}\u{ec}"),
            "\u{e9}\u{e8}*\u{eb}\u{ec}"
        );
        assert_eq!(mask_user_id("abcdef"), "ab**ef");
        assert_eq!(mask_user_id("abcd"), "****");
    }

    #[test]
    fn al1_6_alert_store_persists_settings() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = QuotaAlertStore::new(dir.path());
        store.set_usage_alert(80);
        store.set_balance_alert(5.0);
        let again = QuotaAlertStore::new(dir.path());
        assert_eq!(again.usage_alert(), 80);
        assert!((again.balance_alert() - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn al1_7_alert_text_masks_account_in_title() {
        let alert = super::QuotaAlert {
            provider: "sub".to_owned(),
            name: "Sub".to_owned(),
            user: "secret@example.test".to_owned(),
            alias: String::new(),
            window: "Weekly".to_owned(),
            used: 85.0,
            resets_at: None,
            balance: String::new(),
        };
        let (title, body) = alert_text(&alert, 0.0, UNIX_EPOCH);
        assert!(title.contains("se****et@example.test"));
        assert!(body.contains("Weekly"));
    }
}
