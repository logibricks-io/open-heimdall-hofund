//! Factory plan limits from `/api/billing/limits`.

use hofund_plugin_api::{PluginError, QuotaWindow};
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::time::parse_rfc3339;

struct WindowSpec {
    name: &'static str,
}

const STANDARD: [WindowSpec; 3] = [
    WindowSpec { name: "5 hours" },
    WindowSpec { name: "7 days" },
    WindowSpec { name: "30 days" },
];

fn pool_windows(
    pool: Option<&Value>,
    prefix: &str,
    core: bool,
    extra_funded: bool,
    out: &mut Vec<QuotaWindow>,
) -> Result<(), PluginError> {
    let Some(Value::Object(map)) = pool else {
        return Ok(());
    };
    for spec in STANDARD {
        let key = match spec.name {
            "5 hours" => "fiveHour",
            "7 days" => "weekly",
            _ => "monthly",
        };
        let Some(entry) = map.get(key) else {
            continue;
        };
        let used = entry
            .get("usedPercent")
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
            .clamp(0.0, 100.0)
            .round() as u64;
        let resets = window_end(entry.get("windowEnd"));
        let name = if prefix.is_empty() {
            spec.name.to_owned()
        } else {
            format!("{prefix}{}", spec.name)
        };
        let window =
            QuotaWindow::new(name, used.min(100), Some(100), resets)?.with_blocking(!extra_funded);
        let window = if core {
            window.with_models(crate::models::core_models())
        } else {
            window.with_excluded_models(crate::models::core_models())
        };
        out.push(window);
    }
    Ok(())
}

fn window_end(value: Option<&Value>) -> Option<std::time::SystemTime> {
    let value = value?;
    if let Some(text) = value.as_str() {
        return parse_rfc3339(text);
    }
    if let Some(ms) = value.as_u64().or_else(|| value.as_f64().map(|n| n as u64)) {
        return std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_millis(ms));
    }
    None
}

fn extra_funded(value: &Value) -> bool {
    value
        .get("extraUsageAllowed")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        && value
            .get("extraUsageBalanceCents")
            .and_then(Value::as_f64)
            .is_some_and(|cents| cents > 0.0)
}

/// Parses Factory billing limits into quota windows.
pub(crate) fn windows_from_limits(value: &Value) -> Result<Vec<QuotaWindow>, PluginError> {
    let limits = value.get("limits").unwrap_or(value);
    let funded = extra_funded(value);
    let mut out = Vec::new();
    pool_windows(
        limits.get("standard").or_else(|| limits.get("Standard")),
        "",
        false,
        funded,
        &mut out,
    )?;
    pool_windows(
        limits.get("core").or_else(|| limits.get("Core")),
        "Droid Core · ",
        true,
        funded,
        &mut out,
    )?;
    if let Some(extra) = value
        .get("extraUsageBalanceCents")
        .and_then(Value::as_f64)
        .filter(|cents| *cents > 0.0)
    {
        let display = format!("${:.2}", extra / 100.0);
        out.push(
            QuotaWindow::new("Extra usage", 0, None, None)?
                .with_blocking(false)
                .with_display(display),
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use hofund_plugin_sdk::json::json;

    use super::*;

    #[test]
    fn limit_windows_follow_factory_shape() {
        let limits = json!({
            "limits": {
                "standard": {
                    "fiveHour": {"usedPercent": 50.0, "windowEnd": "2030-01-01T00:00:00Z"},
                    "monthly": {"usedPercent": 10.0, "windowEnd": "2030-02-01T00:00:00Z"},
                }
            }
        });
        let windows = windows_from_limits(&limits).expect("windows");
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].name(), "5 hours");
        assert_eq!(windows[0].used(), 50);
        assert!(windows[0].blocking());
        assert_eq!(windows[1].name(), "30 days");
        assert_eq!(windows[1].used(), 10);
    }

    #[test]
    fn r52_core_windows_block_only_core_models() {
        let limits = json!({
            "limits": {
                "core": {
                    "fiveHour": {"usedPercent": 100.0, "windowEnd": "2030-01-01T00:00:00Z"},
                }
            }
        });
        let windows = windows_from_limits(&limits).expect("windows");
        assert_eq!(windows.len(), 1);
        assert!(windows[0].blocking());
        assert_eq!(windows[0].models(), crate::models::core_models());
    }

    #[test]
    fn a2b_38_extra_usage_makes_standard_non_blocking() {
        let limits = json!({
            "extraUsageAllowed": true,
            "extraUsageBalanceCents": 500.0,
            "limits": {
                "standard": {
                    "fiveHour": {"usedPercent": 100.0, "windowEnd": "2030-01-01T00:00:00Z"},
                }
            }
        });
        let windows = windows_from_limits(&limits).expect("windows");
        assert_eq!(windows.len(), 2);
        assert!(!windows[0].blocking());
        assert_eq!(windows[0].excluded_models(), crate::models::core_models());
        assert_eq!(windows[1].name(), "Extra usage");
        assert_eq!(windows[1].display(), "$5.00");
        assert!(!windows[1].blocking());
    }
}
