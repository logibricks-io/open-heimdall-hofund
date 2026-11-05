//! Billing quota from WorkBuddy meters.

use hofund_plugin_api::{PluginError, QuotaWindow};
use hofund_plugin_sdk::json::Value;

pub(crate) fn windows(body: &[u8]) -> Result<Vec<QuotaWindow>, PluginError> {
    let root: Value = hofund_plugin_sdk::json::from_slice(body)
        .map_err(|_| PluginError::decode("WorkBuddy billing JSON is invalid"))?;
    let data = root.get("data").unwrap_or(&root);
    let Some(used) = data
        .get("usedCredits")
        .or_else(|| data.get("used"))
        .and_then(as_f64)
    else {
        return Ok(Vec::new());
    };
    let Some(total) = data
        .get("totalCredits")
        .or_else(|| data.get("total"))
        .and_then(as_f64)
    else {
        return Ok(Vec::new());
    };
    let percent = if total <= 0.0 {
        0
    } else {
        ((used / total) * 100.0).round() as u64
    };
    Ok(vec![QuotaWindow::new(
        "Credits",
        percent.min(100),
        Some(100),
        None,
    )?])
}

fn as_f64(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse().ok())
        .or_else(|| value.as_i64().map(|n| n as f64))
}
