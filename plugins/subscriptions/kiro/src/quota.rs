//! Usage limits from Kiro management API JSON.

use hofund_plugin_api::{PluginError, QuotaWindow};
use hofund_plugin_sdk::json::Value;

pub(crate) fn windows(body: &[u8]) -> Result<Vec<QuotaWindow>, PluginError> {
    let root: Value = hofund_plugin_sdk::json::from_slice(body)
        .map_err(|_| PluginError::decode("Kiro usage limits are not JSON"))?;
    let list = root
        .get("usageBreakdownList")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for entry in list {
        let Some(object) = entry.as_object() else {
            continue;
        };
        let name = object
            .get("displayName")
            .and_then(Value::as_str)
            .unwrap_or("Usage");
        let used = object
            .get("currentUsageWithPrecision")
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
            .round() as u64;
        let limit = object
            .get("usageLimitWithPrecision")
            .and_then(Value::as_f64)
            .unwrap_or(100.0)
            .round() as u64;
        let percent = if limit > 0 {
            ((used as f64 / limit as f64) * 100.0).round() as u64
        } else {
            0
        };
        let window = QuotaWindow::new(name, percent.min(100), Some(100), None)?;
        out.push(window.with_blocking(true));
    }
    Ok(out)
}
