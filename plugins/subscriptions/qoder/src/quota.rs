//! Qoder plan and quota windows.

use hofund_plugin_api::{PluginError, QuotaWindow};
use hofund_plugin_sdk::json::Value;

pub(crate) fn windows(body: &[u8]) -> Result<Vec<QuotaWindow>, PluginError> {
    let root: Value = hofund_plugin_sdk::json::from_slice(body)
        .map_err(|_| PluginError::decode("Qoder usage JSON is invalid"))?;
    let quota = root
        .pointer("/data/quota")
        .or_else(|| root.get("quota"))
        .or_else(|| root.pointer("/quota"));
    let Some(quota) = quota else {
        return Ok(Vec::new());
    };
    let total = number_field(quota, &["total", "limit"]).unwrap_or(100);
    let used = number_field(quota, &["used", "consumed"]).unwrap_or(0);
    let percent = if total == 0 {
        0
    } else {
        ((used as f64 / total as f64) * 100.0).round() as u64
    };
    let window = QuotaWindow::new("Credits", percent.min(100), Some(100), None)?;
    Ok(vec![window])
}

fn number_field(value: &Value, keys: &[&str]) -> Option<u64> {
    for key in keys {
        if let Some(number) = value.get(*key).and_then(as_u64) {
            return Some(number);
        }
    }
    None
}

fn as_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|n| u64::try_from(n).ok()))
        .or_else(|| value.as_f64().map(|n| n.round() as u64))
}
