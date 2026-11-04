//! GLM coding plan usage from the monitor quota endpoint.

use std::path::Path;

use hofund_plugin_api::{HostContext, PluginError, QuotaWindow, SecretString};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::{self, Value};
use hofund_plugin_sdk::oauth::secret_text;

use crate::session::Stored;
use crate::team;
use crate::APP_VERSION;

pub(crate) async fn read(
    host: &HostContext,
    stored: &Stored,
    device_file: &Path,
) -> Result<Vec<QuotaWindow>, PluginError> {
    let root = monitor_root(&stored.auth.base);
    if stored.auth.team() {
        let url = format!("{root}/api/monitor/usage/quota/limit?type=2");
        let headers = team::team_headers(&stored.auth.base, &stored.auth.org, &stored.auth.project);
        let header_refs: Vec<(&str, &str)> = headers
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect();
        let body = fetch_monitor(host, &url, &stored.api_key, &header_refs, device_file).await?;
        return windows_from_monitor(&body);
    }
    let plan_key =
        stored.auth.api_key.as_ref().ok_or_else(|| {
            PluginError::auth("the stored Z Code sign-in has no plan key for quota")
        })?;
    let url = format!("{root}/api/monitor/usage/quota/limit");
    let body = fetch_monitor(host, &url, plan_key, &[], device_file).await?;
    windows_from_monitor(&body)
}

async fn fetch_monitor(
    host: &HostContext,
    url: &str,
    auth: &SecretString,
    extra_headers: &[(&str, &str)],
    device_file: &Path,
) -> Result<Value, PluginError> {
    let _ = device_file;
    let mut call = Call::get(url)
        .header("accept", "application/json")
        .header("user-agent", format!("ZCode/{APP_VERSION}"))
        .sensitive_header("authorization", secret_text(auth));
    for (name, value) in extra_headers {
        call = call.header(*name, *value);
    }
    let reply = call.fetch(host.http()).await?;
    if !(200..300).contains(&reply.status()) {
        return Err(PluginError::status_error(
            reply.status(),
            format!("Z Code quota request failed (HTTP {})", reply.status()),
        ));
    }
    json::from_slice(reply.body())
        .map_err(|_| PluginError::decode("Z Code quota reply is not JSON"))
}

fn monitor_root(anthropic_base: &str) -> &'static str {
    if anthropic_base.contains("bigmodel.cn") {
        "https://open.bigmodel.cn"
    } else {
        "https://api.z.ai"
    }
}

pub(crate) fn windows_from_monitor(body: &Value) -> Result<Vec<QuotaWindow>, PluginError> {
    if let Some(success) = body.get("success").and_then(Value::as_bool)
        && !success
    {
        let msg = body
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or("Z Code quota request failed");
        return Err(PluginError::decode(msg));
    }
    let limits = body
        .get("data")
        .and_then(|data| data.get("limits"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for row in limits {
        let Some(object) = row.as_object() else {
            continue;
        };
        let unit = object.get("unit").and_then(Value::as_i64).unwrap_or(0);
        let kind = object.get("type").and_then(Value::as_str).unwrap_or("");
        let used_pct = object
            .get("percentage")
            .and_then(|value| value.as_f64().or_else(|| value.as_i64().map(|n| n as f64)))
            .map(|value| value.round() as u64)
            .unwrap_or(0);
        let resets_at = object
            .get("nextResetTime")
            .and_then(parse_reset_ms)
            .map(|ms| std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms));
        let (name, blocking) = if kind.eq_ignore_ascii_case("TIME_LIMIT") {
            ("MCP · Month".to_owned(), false)
        } else if unit == 3 {
            let hours = object
                .get("number")
                .and_then(Value::as_i64)
                .filter(|n| *n > 0)
                .unwrap_or(5);
            (format!("{hours} hours"), true)
        } else if unit == 6 {
            ("7 days".to_owned(), true)
        } else {
            ("Allowance".to_owned(), true)
        };
        let window =
            QuotaWindow::new(name, used_pct, Some(100), resets_at)?.with_blocking(blocking);
        out.push(window);
    }
    Ok(out)
}

fn parse_reset_ms(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number.as_u64().or_else(|| {
            number
                .as_f64()
                .filter(|value| *value > 0.0)
                .map(|value| value as u64)
        }),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{monitor_root, windows_from_monitor};
    use crate::{BIGMODEL_ANTHROPIC_BASE, ZAI_ANTHROPIC_BASE};
    use hofund_plugin_sdk::json::json;

    #[test]
    fn monitor_root_matches_plan_site() {
        assert_eq!(
            monitor_root(BIGMODEL_ANTHROPIC_BASE),
            "https://open.bigmodel.cn"
        );
        assert_eq!(monitor_root(ZAI_ANTHROPIC_BASE), "https://api.z.ai");
    }

    #[test]
    fn monitor_parses_non_zero_percentage() {
        let body = json!({
            "success": true,
            "data": {
                "level": "pro",
                "limits": [
                    {"type": "TOKENS_LIMIT", "unit": 3, "number": 5, "percentage": 42.0, "nextResetTime": 1_758_800_000_000_i64},
                    {"type": "TIME_LIMIT", "unit": 5, "number": 1, "percentage": 3.0, "nextResetTime": 1_758_800_000_000_i64}
                ]
            }
        });
        let windows = windows_from_monitor(&body).expect("parse");
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].name(), "5 hours");
        assert_eq!(windows[0].used(), 42);
        assert!(windows[0].blocking());
        assert!(!windows[1].blocking());
    }
}
