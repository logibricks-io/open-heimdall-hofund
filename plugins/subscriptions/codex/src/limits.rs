//! What the Codex backend says of the account's usage limits while it
//! answers: the `x-codex-primary-*` and `x-codex-secondary-*` headers, the
//! `x-codex-credits-*` headers, and a `codex.rate_limits` event ahead of the
//! reply. Each window becomes a quota observation, so a spent window rests
//! the account before the usage page is read again.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hofund_plugin_api::HostContext;
use hofund_plugin_sdk::HeaderMap;
use hofund_plugin_sdk::json::{self, Value};

/// How far into a reply its `codex.rate_limits` event is looked for: it
/// comes ahead of the reply.
pub(crate) const SCAN: usize = 64 << 10;

/// A whole window in basis points: 100 percent.
pub(crate) const BASIS: u64 = 10_000;

/// One usage window the backend reported.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Limit {
    /// Percent of the window used.
    pub(crate) used_percent: f64,
    /// How long until the window resets, when the backend said.
    pub(crate) resets_in: Option<Duration>,
}

impl Limit {
    /// Basis points used, within `0..=BASIS`.
    pub(crate) fn used(&self) -> u64 {
        basis_points(self.used_percent)
    }
}

/// `percent` in basis points, within `0..=BASIS`. Not a number is 0.
pub(crate) fn basis_points(percent: f64) -> u64 {
    let points = (percent * 100.0).round();
    if points.is_nan() || points <= 0.0 {
        0
    } else if points >= 10_000.0 {
        BASIS
    } else {
        // In range and whole: the cast is exact.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let points = points as u64;
        points
    }
}

/// The time until the window resets: `reset_at` (Unix seconds) when it is
/// set, else `reset_after` seconds. `None` when neither is positive or the
/// reset has passed.
fn resets_in(reset_at: Option<i64>, reset_after: Option<i64>, now: SystemTime) -> Option<Duration> {
    if let Some(at) = reset_at.filter(|at| *at > 0) {
        let at = UNIX_EPOCH + Duration::from_secs(at.unsigned_abs());
        return at.duration_since(now).ok().filter(|left| !left.is_zero());
    }
    reset_after
        .filter(|after| *after > 0)
        .map(|after| Duration::from_secs(after.unsigned_abs()))
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
}

/// The windows the `x-codex-primary-*` and `x-codex-secondary-*` headers
/// tell. One that does not say how long it runs is left out.
pub(crate) fn header_limits(headers: &HeaderMap, now: SystemTime) -> Vec<Limit> {
    let mut out = Vec::new();
    for which in ["primary", "secondary"] {
        let name = |field: &str| format!("x-codex-{which}-{field}");
        let Some(used) = header(headers, &name("used-percent")).and_then(|v| v.parse::<f64>().ok())
        else {
            continue;
        };
        let minutes = header(headers, &name("window-minutes")).and_then(|v| v.parse::<i64>().ok());
        if minutes.is_none_or(|minutes| minutes <= 0) {
            continue;
        }
        let reset_at = header(headers, &name("reset-at")).and_then(|v| v.parse().ok());
        let reset_after =
            header(headers, &name("reset-after-seconds")).and_then(|v| v.parse().ok());
        out.push(Limit {
            used_percent: used,
            resets_in: resets_in(reset_at, reset_after, now),
        });
    }
    out
}

/// Whether the `x-codex-credits-*` headers say the account goes on past
/// its allowance on credits.
pub(crate) fn header_credits(headers: &HeaderMap) -> bool {
    let flag = |name: &str| header(headers, name).is_some_and(|v| v.eq_ignore_ascii_case("true"));
    goes_on_credits(
        flag("x-codex-credits-has-credits"),
        flag("x-codex-credits-unlimited"),
        header(headers, "x-codex-credits-balance"),
    )
}

/// Whether an account goes on past its allowance on credits: they are
/// unlimited, or it has some. A balance that reads as a number and is
/// spent (0 or less) is none left, whatever `has` says.
pub(crate) fn goes_on_credits(has: bool, unlimited: bool, balance: Option<&str>) -> bool {
    if unlimited {
        return true;
    }
    if !has {
        return false;
    }
    balance
        .and_then(|balance| balance.trim().parse::<f64>().ok())
        .is_none_or(|balance| balance > 0.0)
}

/// What a `codex.rate_limits` event says.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Event {
    pub(crate) limits: Vec<Limit>,
    pub(crate) credits: Option<bool>,
}

/// The `codex.rate_limits` event in one `data:` payload. `None` for any
/// other payload.
fn rate_limits_event(data: &[u8]) -> Option<Value> {
    let event: Value = json::from_slice(data).ok()?;
    (event.get("type").and_then(Value::as_str) == Some("codex.rate_limits")).then_some(event)
}

/// The windows a `codex.rate_limits` event tells. `None` for a limit other
/// than the account's own (`metered_limit_name` naming another).
pub(crate) fn event_limits(event: &Value, now: SystemTime) -> Option<Event> {
    let limits = event
        .get("rate_limits")
        .filter(|limits| limits.is_object())?;
    let metered = event
        .get("metered_limit_name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .replace('-', "_");
    if !metered.is_empty() && metered != "codex" {
        return None;
    }
    let mut out = Event::default();
    for which in ["primary", "secondary"] {
        let Some(window) = limits.get(which).filter(|window| window.is_object()) else {
            continue;
        };
        let Some(used) = window.get("used_percent").and_then(Value::as_f64) else {
            continue;
        };
        if window
            .get("window_minutes")
            .and_then(Value::as_i64)
            .is_none_or(|minutes| minutes <= 0)
        {
            continue;
        }
        out.limits.push(Limit {
            used_percent: used,
            resets_in: resets_in(
                window.get("reset_at").and_then(Value::as_i64),
                window.get("reset_after_seconds").and_then(Value::as_i64),
                now,
            ),
        });
    }
    out.credits = event
        .get("credits")
        .filter(|credits| credits.is_object())
        .map(|credits| {
            let flag = |name: &str| credits.get(name).and_then(Value::as_bool) == Some(true);
            let balance = match credits.get("balance") {
                Some(Value::String(text)) => Some(text.clone()),
                Some(Value::Number(number)) => Some(number.to_string()),
                _ => None,
            };
            goes_on_credits(flag("has_credits"), flag("unlimited"), balance.as_deref())
        });
    Some(out)
}

/// Records each window in `limits` as an observation: 10 000 basis points,
/// what is left of them, and when the window resets.
pub(crate) fn observe(host: &HostContext, limits: &[Limit], credits: bool) {
    for limit in limits {
        let mut observation = host
            .observe()
            .observation()
            .with_limit(BASIS)
            .with_remaining(BASIS - limit.used())
            .with_spending_credits(credits);
        if let Some(after) = limit.resets_in {
            observation = observation.with_window_reset(after);
        }
        // A rejected observation leaves the call as it is.
        let _ = host.observe().observe(observation);
    }
}

/// Looks at one `data:` payload for the `codex.rate_limits` event and
/// records what it says. Returns `true` once the event was seen, which ends
/// the look.
pub(crate) fn watch(host: &HostContext, data: &[u8], header_credits: bool) -> bool {
    if !contains(data, br#""codex.rate_limits""#) {
        return false;
    }
    let Some(event) = rate_limits_event(data) else {
        return false;
    };
    if let Some(event) = event_limits(&event, host.clock().now()) {
        observe(host, &event.limits, event.credits.unwrap_or(header_credits));
    }
    true
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}
