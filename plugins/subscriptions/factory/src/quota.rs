//! Factory plan limits and credit balance.

use hofund_plugin_api::{PluginError, QuotaWindow};
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::time::parse_rfc3339;

const LIMITS: [(&str, &str, u64); 3] = [
    ("five_hour", "5 hours", 5 * 60 * 60),
    ("weekly", "Weekly", 7 * 24 * 60 * 60),
    ("monthly", "Month", 30 * 24 * 60 * 60),
];

/// Usage facts from `/users/me`, balance, and plan limits.
pub(crate) struct Usage {
    pub(crate) windows: Vec<QuotaWindow>,
    pub(crate) balance: Option<String>,
}

/// Builds quota windows from the three Factory reads.
pub(crate) fn assemble(
    _me: Option<&Value>,
    balance: Option<&Value>,
    limits: Option<&Value>,
) -> Usage {
    let mut windows = limit_windows(limits);
    if let Some(balance) = balance
        && let Ok(Some(window)) = credits_window(balance)
    {
        windows.push(window);
    }
    let dollars = balance.and_then(balance_usd);
    Usage {
        windows,
        balance: dollars.or_else(|| credits_balance(balance)),
    }
}

/// Parses plan limits into windows.
pub(crate) fn limit_windows(limits: Option<&Value>) -> Vec<QuotaWindow> {
    let Some(Value::Object(map)) = limits else {
        return Vec::new();
    };
    let Some(list) = map.get("limits").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (typ, name, span_secs) in LIMITS {
        let Some(entry) = list.iter().find(|item| {
            item.get("type")
                .and_then(Value::as_str)
                .is_some_and(|t| t == typ)
        }) else {
            continue;
        };
        let percent = entry
            .get("percentUsed")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0)
            .clamp(0.0, 100.0);
        let used = (percent.round() as u64).min(100);
        let resets = entry
            .get("resetsAt")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339);
        if let Ok(window) = QuotaWindow::new(name, used, Some(100), resets) {
            let window = window.with_blocking(true);
            let _ = span_secs;
            out.push(window);
        }
    }
    out
}

fn balance_usd(value: &Value) -> Option<String> {
    let n = value
        .get("balance")
        .and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse().ok()))?;
    if !n.is_finite() || n < 0.0 {
        return None;
    }
    Some(usd(n / 1_000_000.0))
}

fn usd(n: f64) -> String {
    if n == 0.0 || n >= 0.01 {
        format!("${n:.2}")
    } else {
        format!("${:.4}", (n * 10_000.0).round() / 10_000.0)
    }
}

fn credits_balance(value: Option<&Value>) -> Option<String> {
    let value = value?;
    let remaining = flat_number(value, &["remaining", "available", "balance", "credits"]);
    remaining.map(|n| format!("{} credits", compact_number(n)))
}

fn credits_window(value: &Value) -> Result<Option<QuotaWindow>, PluginError> {
    let total = flat_number(value, &["total", "limit", "quota"]);
    let used = flat_number(value, &["used", "spent"]);
    let remaining = flat_number(value, &["remaining", "available"]);
    let Some(total) = total.filter(|t| *t > 0.0) else {
        return Ok(None);
    };
    let (used_pct, amount) = if let Some(used) = used {
        ((100.0 * used / total).round() as u64, used)
    } else if let Some(left) = remaining.filter(|r| *r <= total) {
        (
            (100.0 * (total - left) / total).round() as u64,
            total - left,
        )
    } else {
        return Ok(None);
    };
    let display = format!("{} / {}", compact_number(amount), compact_number(total));
    let window = QuotaWindow::new("Credits", used_pct.min(100), Some(100), None)?;
    let _ = display;
    Ok(Some(window))
}

fn flat_number(value: &Value, keys: &[&str]) -> Option<f64> {
    let mut flat = std::collections::BTreeMap::new();
    collect_numbers(value, 0, &mut flat);
    for key in keys {
        if let Some(n) = flat.get(&key.to_ascii_lowercase()) {
            return Some(*n);
        }
    }
    None
}

fn collect_numbers(value: &Value, depth: u8, out: &mut std::collections::BTreeMap<String, f64>) {
    if depth > 2 {
        return;
    }
    let Value::Object(map) = value else {
        return;
    };
    for (key, val) in map {
        match val {
            Value::Number(number) => {
                if let Some(n) = number.as_f64() {
                    out.insert(key.to_ascii_lowercase(), n);
                }
            }
            Value::String(text) => {
                if let Ok(n) = text.trim().parse::<f64>() {
                    out.insert(key.to_ascii_lowercase(), n);
                }
            }
            Value::Object(_) => collect_numbers(val, depth + 1, out),
            _ => {}
        }
    }
}

fn compact_number(n: f64) -> String {
    if !n.is_finite() {
        return "0".to_owned();
    }
    if n.fract() == 0.0 {
        return format!("{}", n as i64);
    }
    format!("{:.2}", n)
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_owned()
}

/// Turns balance-only usage into one display window when no plan limits exist.
pub(crate) fn balance_window(balance: &str) -> Result<QuotaWindow, PluginError> {
    QuotaWindow::new(format!("Balance: {balance}"), 0, None, None)
}

#[cfg(test)]
mod tests {
    use hofund_plugin_sdk::json::json;

    use super::*;

    #[test]
    fn limit_windows_follow_cline_order() {
        let limits = json!({"limits": [
            {"type": "monthly", "percentUsed": 10.0},
            {"type": "five_hour", "percentUsed": 50.0},
        ]});
        let windows = limit_windows(Some(&limits));
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].name(), "5 hours");
        assert_eq!(windows[0].used(), 50);
    }

    #[test]
    fn balance_usd_reads_micro_dollars() {
        let balance = json!({"balance": 2_500_000});
        assert_eq!(balance_usd(&balance).as_deref(), Some("$2.50"));
    }
}
