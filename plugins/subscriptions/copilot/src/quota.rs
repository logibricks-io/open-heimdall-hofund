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

/// Reads `YYYY-MM-DD`, or RFC 3339; see [`hofund_plugin_sdk::time::parse_rfc3339`].
pub(crate) fn parse_time(text: &str) -> Option<SystemTime> {
    hofund_plugin_sdk::time::parse_rfc3339(text)
}
