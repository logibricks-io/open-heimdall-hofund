//! Usage windows from dashboard period replies.

use std::time::{Duration, UNIX_EPOCH};

use hofund_plugin_api::{PluginError, QuotaWindow};
use hofund_plugin_sdk::json::Value;

/// Reads the three standard included-usage windows from a period reply.
pub(crate) fn period_windows(body: &Value) -> Result<Vec<QuotaWindow>, PluginError> {
    let usage = body
        .get("planUsage")
        .ok_or_else(|| PluginError::decode("Cursor sent no plan usage"))?;
    let resets = billing_reset(body.get("billingCycleEnd"));
    let auto = percent(usage, "autoPercentUsed");
    let api = percent(usage, "apiPercentUsed");
    let total = percent(usage, "totalPercentUsed");
    let mut windows = Vec::new();
    if let Some(used) = auto {
        windows.push(window("Cursor Models", used, resets)?);
    }
    if let Some(used) = api {
        windows.push(window("Other Models", used, resets)?);
    }
    if let Some(used) = total {
        windows.push(window("Total", used, resets)?.with_blocking(false));
    }
    Ok(windows)
}

fn window(
    name: &str,
    used_percent: u64,
    resets: Option<std::time::SystemTime>,
) -> Result<QuotaWindow, PluginError> {
    QuotaWindow::new(name, used_percent, Some(100), resets)
}

fn percent(usage: &Value, key: &str) -> Option<u64> {
    usage
        .get(key)
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
        .map(|value| value.clamp(0.0, 100.0).round() as u64)
}

fn billing_reset(value: Option<&Value>) -> Option<std::time::SystemTime> {
    let text = value.and_then(Value::as_str)?;
    if let Ok(ms) = text.parse::<i64>()
        && ms > 0
    {
        return Some(UNIX_EPOCH + Duration::from_millis(ms as u64));
    }
    hofund_plugin_sdk::time::parse_rfc3339(text)
}

#[cfg(test)]
mod tests {
    use hofund_plugin_sdk::json::json;

    use super::period_windows;

    #[test]
    fn parses_three_pools() {
        let body = json!({
            "billingCycleEnd": "1792833042000",
            "planUsage": {
                "autoPercentUsed": 12.5,
                "apiPercentUsed": 40.0,
                "totalPercentUsed": 20.0
            }
        });
        let windows = period_windows(&body).expect("windows");
        assert_eq!(windows.len(), 3);
        assert_eq!(windows[0].name(), "Cursor Models");
        assert_eq!(windows[0].used(), 13);
        assert_eq!(windows[1].name(), "Other Models");
        assert_eq!(windows[2].name(), "Total");
    }
}
