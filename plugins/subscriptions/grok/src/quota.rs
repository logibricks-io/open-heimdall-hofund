//! Subscription allowance from `/billing?format=credits`.

use std::time::SystemTime;

use hofund_plugin_api::{PluginError, QuotaWindow};
use hofund_plugin_sdk::json::{Map, Value};
use hofund_plugin_sdk::time::parse_rfc3339;

const PERIODS: [(&str, &str, u64); 3] = [
    ("DAILY", "1 day", 24 * 3600),
    ("WEEKLY", "7 days", 7 * 24 * 3600),
    ("MONTHLY", "Month", 30 * 24 * 3600),
];

/// Reads quota windows from a billing reply body.
pub(crate) fn windows(body: &[u8]) -> Result<Vec<QuotaWindow>, PluginError> {
    let root: Value = hofund_plugin_sdk::json::from_slice(body)
        .map_err(|_| PluginError::decode("the billing reply is not JSON"))?;
    let cfg = root
        .get("config")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut name = "Allowance".to_owned();
    let mut end = cfg
        .get("billingPeriodEnd")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if let Some(period) = cfg.get("currentPeriod").and_then(Value::as_object) {
        let kind = period
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim_start_matches("USAGE_PERIOD_TYPE_");
        if let Some(label) = PERIODS
            .iter()
            .find(|(key, _, _)| *key == kind)
            .map(|(_, label, _)| *label)
        {
            name = label.to_owned();
        }
        end = period.get("end").and_then(Value::as_str).map(str::to_owned);
    }
    let resets_at = end.as_deref().and_then(parse_rfc3339);
    let used = number(&cfg, "creditUsagePercent");
    let mut out = vec![window(&name, used, resets_at)?];
    let cap = number_nested(&cfg, "onDemandCap", "val");
    if cap > 0.0 {
        let on_demand_used = number_nested(&cfg, "onDemandUsed", "val");
        let percent = ((100.0 * on_demand_used) / cap).round() as u64;
        out.push(
            QuotaWindow::new("On-demand", percent, Some(100), resets_at)?.with_blocking(false),
        );
    }
    Ok(out)
}

fn window(
    name: &str,
    used: f64,
    resets_at: Option<SystemTime>,
) -> Result<QuotaWindow, PluginError> {
    let used = used.round().max(0.0) as u64;
    QuotaWindow::new(name, used, Some(100), resets_at)
}

fn number(object: &Map<String, Value>, key: &str) -> f64 {
    object.get(key).and_then(number_value).unwrap_or(0.0)
}

fn number_nested(object: &Map<String, Value>, key: &str, field: &str) -> f64 {
    object
        .get(key)
        .and_then(Value::as_object)
        .and_then(|inner| inner.get(field))
        .and_then(number_value)
        .unwrap_or(0.0)
}

fn number_value(value: &Value) -> Option<f64> {
    value.as_f64()
}
