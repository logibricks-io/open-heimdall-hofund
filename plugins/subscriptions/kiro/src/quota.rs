//! Usage limits from Kiro management API JSON (`usageOf` parity).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hofund_plugin_api::{HostContext, PluginError, QuotaWindow};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::{self, Map, Value};

use crate::Endpoints;
use crate::creds::{self, Stored};
use crate::models::auth_call;

const PERCENT: u64 = 100;

/// Fetches raw usage JSON for OAuth or API-key accounts.
pub(crate) async fn fetch_usage_body(
    host: &HostContext,
    endpoints: &Endpoints,
    stored: &Stored,
) -> Result<Vec<u8>, PluginError> {
    if stored.credential.method == "apikey" {
        let region = creds::kiro_region(
            creds::effective_profile_arn(&stored.credential),
            &stored.credential.region,
        );
        let base = endpoints.management.replace("{region}", &region);
        let body = json::json!({ "origin": "KIRO_CLI" }).to_string();
        let reply = auth_call(Call::post(format!("{base}/?origin=KIRO_CLI")), stored)
            .header("content-type", "application/x-amz-json-1.0")
            .header("x-amz-target", "AmazonCodeWhispererService.GetUsageLimits")
            .header("x-amzn-codewhisperer-optout", "false")
            .bytes(body.into_bytes())
            .fetch(host.http())
            .await?;
        match reply.status() {
            200..=299 => Ok(reply.body().to_vec()),
            401 | 403 => Err(PluginError::auth(
                "Kiro refused to show usage limits for this sign-in",
            )),
            status => Err(PluginError::status_error(
                status,
                "Kiro did not return usage limits",
            )),
        }
    } else {
        let url = endpoints.usage_url(
            creds::effective_profile_arn(&stored.credential),
            &stored.credential.region,
        );
        let reply = auth_call(Call::get(url), stored)
            .header("accept", "application/json")
            .fetch(host.http())
            .await?;
        match reply.status() {
            200..=299 => Ok(reply.body().to_vec()),
            401 | 403 => Err(PluginError::auth(
                "Kiro refused to show usage limits for this sign-in",
            )),
            status => Err(PluginError::status_error(
                status,
                "Kiro did not return usage limits",
            )),
        }
    }
}

pub(crate) fn windows(body: &[u8], now: SystemTime) -> Result<Vec<QuotaWindow>, PluginError> {
    let root: Value = hofund_plugin_sdk::json::from_slice(body)
        .map_err(|_| PluginError::decode("Kiro usage limits are not JSON"))?;
    let now_ms = now
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let list = root
        .get("usageBreakdownList")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let root_reset = num(&root, "nextDateReset");
    let mut out = Vec::new();
    for entry in list {
        let Some(object) = entry.as_object() else {
            continue;
        };
        let trial = object.get("freeTrialInfo");
        if let Some(t) = trial {
            let status = t
                .get("freeTrialStatus")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_ascii_uppercase();
            let limit = num(t, "usageLimitWithPrecision");
            if status == "ACTIVE" && limit > 0.0 {
                let used = num(t, "currentUsageWithPrecision");
                let reset = num(t, "freeTrialExpiry");
                out.push(credit_window(
                    "Free trial",
                    used,
                    limit,
                    secs_to_time(reset),
                    false,
                )?);
            }
        }
        let mut bonuses = Vec::new();
        if let Some(items) = object.get("bonuses").and_then(Value::as_array) {
            for bonus in items {
                let Some(b) = bonus.as_object() else {
                    continue;
                };
                let used = num_entry(b, "currentUsage");
                let limit = num_entry(b, "usageLimit");
                let expires = num_entry(b, "expiresAt");
                let status = b
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_ascii_uppercase();
                if status != "ACTIVE" || limit <= 0.0 {
                    continue;
                }
                if expires > 0.0 && (expires * 1000.0) as u128 <= now_ms {
                    continue;
                }
                let left = (limit - used).max(0.0);
                let name = b
                    .get("displayName")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .unwrap_or("Bonus credits");
                bonuses.push(bonus_window(name, used, limit, left, expires, now_ms)?);
            }
        }
        let entry = Value::Object(object.clone());
        let used = num(&entry, "currentUsageWithPrecision");
        let limit = num(&entry, "usageLimitWithPrecision");
        if limit > 0.0 {
            let name = object
                .get("displayNamePlural")
                .or_else(|| object.get("displayName"))
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .unwrap_or("Credits");
            let reset = num(&entry, "nextDateReset").max(root_reset);
            let carried = used >= limit
                && bonuses
                    .iter()
                    .any(|window| window.used() < PERCENT && window.blocking() == false);
            out.push(credit_window(
                name,
                used,
                limit,
                secs_to_time(reset),
                carried,
            )?);
        }
        out.extend(bonuses);
    }
    Ok(out)
}

fn credit_window(
    name: &str,
    used: f64,
    limit: f64,
    resets_at: Option<SystemTime>,
    aside: bool,
) -> Result<QuotaWindow, PluginError> {
    let percent = percent_of(used, limit);
    let mut window =
        QuotaWindow::new(name, percent, Some(PERCENT), resets_at)?.with_display(count(used, limit));
    if aside {
        window = window.with_blocking(false);
    }
    Ok(window)
}

fn bonus_window(
    name: &str,
    used: f64,
    limit: f64,
    _left: f64,
    _expires: f64,
    _now_ms: u128,
) -> Result<QuotaWindow, PluginError> {
    Ok(
        QuotaWindow::new(name, percent_of(used, limit), Some(PERCENT), None)?
            .with_display(count(used, limit))
            .with_blocking(false),
    )
}

fn percent_of(used: f64, limit: f64) -> u64 {
    if limit <= 0.0 {
        return 0;
    }
    ((PERCENT as f64 * used / limit)
        .round()
        .clamp(0.0, PERCENT as f64)) as u64
}

fn count(used: f64, limit: f64) -> String {
    format!("{} / {}", trim_amount(used), go_g(limit))
}

fn trim_amount(value: f64) -> String {
    let rounded = format!("{value:.2}");
    rounded
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_owned()
}

fn go_g(value: f64) -> String {
    if !value.is_finite() {
        return "0".to_owned();
    }
    let exp = value.log10().floor() as i32;
    if exp < -4 || exp >= 6 {
        let exponential = format!("{value:e}");
        let (mantissa, exponent) = exponential.split_once('e').unwrap_or(("0", "0"));
        let sign = if exponent.starts_with('-') { "-" } else { "+" };
        let digits = exponent.trim_start_matches(['+', '-']);
        let padded = format!("{digits:0>2}");
        format!("{mantissa}e{sign}{padded}")
    } else {
        format!("{value}")
    }
}

fn num(object: &Value, key: &str) -> f64 {
    object.get(key).and_then(number_value).unwrap_or(0.0)
}

fn num_entry(object: &Map<String, Value>, key: &str) -> f64 {
    object.get(key).and_then(number_value).unwrap_or(0.0)
}

fn number_value(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        _ => None,
    }
}

fn secs_to_time(secs: f64) -> Option<SystemTime> {
    if secs <= 0.0 {
        return None;
    }
    UNIX_EPOCH.checked_add(Duration::from_secs(secs.trunc() as u64))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn r51d_k_free_trial_and_credits() {
        let body = br#"{"subscriptionInfo":{"subscriptionTitle":"KIRO FREE"},"usageBreakdownList":[{"displayName":"Credit","currentUsageWithPrecision":12.5,"usageLimitWithPrecision":50,"nextDateReset":1790000000,"freeTrialInfo":{"freeTrialStatus":"ACTIVE","currentUsageWithPrecision":100,"usageLimitWithPrecision":500,"freeTrialExpiry":1791000000}}]}"#;
        let windows = windows(body, at(1)).expect("windows");
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].name(), "Free trial");
        assert_eq!(windows[0].display(), "100 / 500");
        assert_eq!(windows[1].name(), "Credit");
        assert_eq!(windows[1].display(), "12.5 / 50");
        assert!(windows[1].blocking());
    }

    #[test]
    fn r51d_k_bonus_after_month_and_aside_when_carried() {
        let body = br#"{"nextDateReset":1796083200.0,"usageBreakdownList":[{"displayName":"Credit","currentUsageWithPrecision":48.97,"usageLimitWithPrecision":984.0,"nextDateReset":1796083200.0,"bonuses":[{"bonusType":"FIRST_UPGRADE_TRIAL","currentUsage":0.0,"displayName":"Trial credits","expiresAt":4100000000.5,"status":"ACTIVE","usageLimit":500.0}]}]}"#;
        let parsed = super::windows(body, at(1_800_000_000)).expect("windows");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].name(), "Credit");
        assert!(parsed[0].blocking());
        assert_eq!(parsed[1].name(), "Trial credits");
        assert!(!parsed[1].blocking());
        let spent = br#"{"usageBreakdownList":[{"currentUsageWithPrecision":984,"usageLimitWithPrecision":984.0,"bonuses":[{"displayName":"Trial credits","currentUsage":10.0,"expiresAt":4100000000.5,"status":"ACTIVE","usageLimit":500.0}]}]}"#;
        let carried = super::windows(spent, at(1_800_000_000)).expect("carried");
        assert_eq!(carried.len(), 2);
        assert!(!carried[0].blocking());
        assert!(!carried[1].blocking());
    }

    #[test]
    fn r51d_k_inactive_bonus_omitted() {
        let body = br#"{"usageBreakdownList":[{"currentUsageWithPrecision":10,"usageLimitWithPrecision":100,"bonuses":[{"status":"EXPIRED","currentUsage":0,"usageLimit":500}]}]}"#;
        let windows = windows(body, at(1)).expect("windows");
        assert_eq!(windows.len(), 1);
    }

    #[test]
    fn r51d_k_over_limit_display_keeps_fractions() {
        let body = br#"{"usageBreakdownList":[{"displayName":"Vibe","currentUsageWithPrecision":1.005,"usageLimitWithPrecision":1000000}]}"#;
        let windows = windows(body, at(1)).expect("windows");
        assert_eq!(windows[0].display(), "1 / 1e+06");
        assert_eq!(windows[0].used(), 0);
    }
}
