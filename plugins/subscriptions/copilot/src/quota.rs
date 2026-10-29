//! Copilot quota from `copilot_internal/user`.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hofund_plugin_api::{PluginError, QuotaWindow};
use hofund_plugin_sdk::json::{self, Map, Value};

/// The quota snapshots read, in order, with their window names.
const WINDOWS: [(&str, &str); 3] = [
    ("chat", "Chat requests"),
    ("completions", "Completions"),
    ("premium_interactions", "Premium requests"),
];

const PREMIUM: &str = "premium_interactions";
const COMPLETIONS: &str = "completions";

/// Reads the quota windows from a `copilot_internal/user` reply.
///
/// Usage is entitlement minus remaining, or 100 minus the percent left. A
/// window with no entitlement or no usable numbers is skipped. Unlimited
/// windows come last, with no limit. Completions never block. Premium
/// requests do not block when overage is permitted on a seat the user pays
/// for: a business or enterprise seat is managed and still blocks.
pub(crate) fn windows(body: &[u8]) -> Result<Vec<QuotaWindow>, PluginError> {
    let Ok(Value::Object(object)) = json::from_slice::<Value>(body) else {
        return Err(PluginError::decode(
            "the Copilot quota reply is not a JSON object",
        ));
    };
    let managed = object
        .get("copilot_plan")
        .and_then(Value::as_str)
        .is_some_and(|plan| {
            plan.eq_ignore_ascii_case("business") || plan.eq_ignore_ascii_case("enterprise")
        });
    let reset = object
        .get("quota_reset_date_utc")
        .and_then(Value::as_str)
        .and_then(parse_time)
        .or_else(|| {
            object
                .get("quota_reset_date")
                .and_then(Value::as_str)
                .and_then(parse_time)
        });
    let snapshots = object.get("quota_snapshots").and_then(Value::as_object);
    let mut limited = Vec::new();
    let mut unlimited = Vec::new();
    for (id, name) in WINDOWS {
        let Some(snapshot) = snapshots
            .and_then(|all| all.get(id))
            .and_then(Value::as_object)
        else {
            continue;
        };
        let overage = snapshot
            .get("overage_permitted")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let blocking = match id {
            COMPLETIONS => false,
            PREMIUM => !(overage && !managed),
            _ => true,
        };
        let resets_at = number(snapshot, "quota_reset_at")
            .filter(|secs| *secs > 0.0)
            .map(|secs| UNIX_EPOCH + Duration::from_secs_f64(secs))
            .or(reset);
        let is_unlimited = snapshot
            .get("unlimited")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if is_unlimited {
            let exhausted =
                id == PREMIUM && snapshot.get("has_quota").and_then(Value::as_bool) == Some(false);
            if exhausted {
                limited.push(
                    QuotaWindow::new(name, 100, Some(100), resets_at)?.with_blocking(blocking),
                );
            } else {
                unlimited.push(QuotaWindow::new(name, 0, None, None)?.with_blocking(false));
            }
            continue;
        }
        let Some((used, limit)) = usage(snapshot) else {
            continue;
        };
        limited.push(QuotaWindow::new(name, used, Some(limit), resets_at)?.with_blocking(blocking));
    }
    limited.extend(unlimited);
    Ok(limited)
}

/// Used and limit of one snapshot, or `None` to skip it.
fn usage(snapshot: &Map<String, Value>) -> Option<(u64, u64)> {
    let entitlement = number(snapshot, "entitlement")?;
    if entitlement <= 0.0 {
        return None;
    }
    if let Some(remaining) = number(snapshot, "quota_remaining") {
        let used = (entitlement - remaining).clamp(0.0, entitlement);
        return Some((whole(used), whole(entitlement)));
    }
    let percent = number(snapshot, "percent_remaining")?;
    Some((whole((100.0 - percent).clamp(0.0, 100.0)), 100))
}

/// A finite number, given as a JSON number or a numeric string.
fn number(object: &Map<String, Value>, key: &str) -> Option<f64> {
    let value = match object.get(key)? {
        Value::Number(number) => number.as_f64()?,
        Value::String(text) => text.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    value.is_finite().then_some(value)
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "callers pass a finite, non-negative value"
)]
fn whole(value: f64) -> u64 {
    value.round() as u64
}

/// Reads `YYYY-MM-DD`, or RFC 3339 `YYYY-MM-DDTHH:MM:SS[.frac](Z|+HH:MM)`.
/// A date alone is midnight UTC.
pub(crate) fn parse_time(text: &str) -> Option<SystemTime> {
    let text = text.trim();
    let date = text.get(..10)?;
    let days = days_from_civil(field(date, 0, 4)?, field(date, 5, 7)?, field(date, 8, 10)?)?;
    if date.as_bytes()[4] != b'-' || date.as_bytes()[7] != b'-' {
        return None;
    }
    let mut seconds = days.checked_mul(86_400)?;
    let rest = &text[10..];
    if !rest.is_empty() {
        let time = rest.strip_prefix(['T', 't', ' '])?;
        let clock = time.get(..8)?;
        if clock.as_bytes()[2] != b':' || clock.as_bytes()[5] != b':' {
            return None;
        }
        let (hour, minute, second) = (
            field(clock, 0, 2)?,
            field(clock, 3, 5)?,
            field(clock, 6, 8)?,
        );
        if hour > 23 || minute > 59 || second > 60 {
            return None;
        }
        seconds += i64::from(hour * 3600 + minute * 60 + second);
        let mut zone = &time[8..];
        if let Some(fraction) = zone.strip_prefix('.') {
            let digits = fraction.bytes().take_while(u8::is_ascii_digit).count();
            if digits == 0 {
                return None;
            }
            zone = &fraction[digits..];
        }
        seconds -= offset(zone)?;
    }
    let seconds = u64::try_from(seconds).ok()?;
    UNIX_EPOCH.checked_add(Duration::from_secs(seconds))
}

/// The zone offset in seconds east of UTC.
fn offset(zone: &str) -> Option<i64> {
    if zone == "Z" || zone == "z" {
        return Some(0);
    }
    let sign = match zone.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let rest = &zone[1..];
    if rest.len() != 5 || rest.as_bytes()[2] != b':' {
        return None;
    }
    let (hours, minutes) = (field(rest, 0, 2)?, field(rest, 3, 5)?);
    if hours > 23 || minutes > 59 {
        return None;
    }
    Some(sign * i64::from(hours * 3600 + minutes * 60))
}

fn field(text: &str, from: usize, to: usize) -> Option<u32> {
    let part = text.get(from..to)?;
    if !part.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    part.parse().ok()
}

/// Days since 1970-01-01 for a proleptic Gregorian date.
fn days_from_civil(year: u32, month: u32, day: u32) -> Option<i64> {
    let leap = (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400);
    let month_days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let index = usize::try_from(month.checked_sub(1)?).ok()?;
    if day == 0 || day > *month_days.get(index)? {
        return None;
    }
    let year = i64::from(year) - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let shifted = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Some(era * 146_097 + day_of_era - 719_468)
}
