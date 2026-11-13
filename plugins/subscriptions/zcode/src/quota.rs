//! GLM coding plan usage from the monitor quota endpoint.

use std::path::Path;

use hofund_plugin_api::{AccountId, HostContext, PluginError, QuotaWindow, SecretString};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::{self, Value};
use hofund_plugin_sdk::oauth::{TokenCache, secret_text};

use crate::APP_VERSION;
use crate::http_auth::{
    auth_for_http_status, key_refused_code, parse_envelope_code, refusal_from_monitor_body,
};
use crate::session::{self, Stored};
use crate::{Endpoints, gift, team};

pub(crate) async fn read(
    host: &HostContext,
    endpoints: &Endpoints,
    account: &AccountId,
    stored: &Stored,
    device_file: &Path,
    team_keys: &TokenCache<SecretString>,
) -> Result<Vec<QuotaWindow>, PluginError> {
    if stored.auth.api_key.is_none() && !stored.auth.team() {
        return Ok(gift::read(host, endpoints, stored, device_file)
            .await?
            .windows);
    }
    let root = monitor_root(&stored.auth.base);
    let body = if stored.auth.team() {
        let url = format!("{root}/api/monitor/usage/quota/limit?type=2");
        let headers = team::team_headers(&stored.auth.base, &stored.auth.org, &stored.auth.project);
        let header_refs: Vec<(&str, &str)> = headers
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect();
        fetch_monitor(
            host,
            account,
            stored,
            team_keys,
            &url,
            &stored.api_key,
            &header_refs,
            device_file,
        )
        .await?
    } else {
        let plan_key = stored.auth.api_key.as_ref().ok_or_else(|| {
            PluginError::auth("the stored Z Code sign-in has no plan key for quota")
        })?;
        let url = format!("{root}/api/monitor/usage/quota/limit");
        fetch_monitor(
            host,
            account,
            stored,
            team_keys,
            &url,
            plan_key,
            &[],
            device_file,
        )
        .await?
    };
    let mut windows = windows_from_monitor(&body)?;
    if stored.auth.jwt.is_some()
        && let Ok(balance) = gift::read(host, endpoints, stored, device_file).await
    {
        let trial_models: Vec<String> = balance
            .all_models
            .iter()
            .map(|model| gift::gift_id(model).to_ascii_lowercase())
            .collect();
        if !trial_models.is_empty() {
            windows = windows
                .into_iter()
                .map(|window| window.with_excluded_models(trial_models.clone()))
                .collect();
        }
        windows.extend(balance.windows);
    }
    Ok(windows)
}

#[allow(clippy::too_many_arguments)]
async fn fetch_monitor(
    host: &HostContext,
    account: &AccountId,
    stored: &Stored,
    team_keys: &TokenCache<SecretString>,
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
        let error = auth_for_http_status(reply.status(), "Z Code quota request failed");
        if matches!(error.kind(), hofund_plugin_api::PluginErrorKind::Auth)
            && stored.auth.team()
            && stored.auth.api_key.is_none()
        {
            session::drop_team_key_cache(team_keys, account);
        }
        return Err(error);
    }
    let body: Value = json::from_slice(reply.body())
        .map_err(|_| PluginError::decode("Z Code quota reply is not JSON"))?;
    if let Some(error) = refusal_from_monitor_body(&body) {
        return Err(error);
    }
    if let Some(code) = parse_envelope_code(body.get("code"))
        && code != 0
        && code != 200
    {
        let msg = body
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or("Z Code quota request failed");
        if key_refused_code(code) {
            return Err(PluginError::auth(msg));
        }
        return Err(PluginError::decode(msg));
    }
    Ok(body)
}

fn monitor_root(anthropic_base: &str) -> &'static str {
    if anthropic_base.contains("bigmodel.cn") {
        "https://open.bigmodel.cn"
    } else {
        "https://api.z.ai"
    }
}

pub(crate) fn windows_from_monitor(body: &Value) -> Result<Vec<QuotaWindow>, PluginError> {
    if let Some(error) = refusal_from_monitor_body(body) {
        return Err(error);
    }
    if let Some(code) = parse_envelope_code(body.get("code"))
        && code != 0
        && code != 200
    {
        let msg = body
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or("Z Code quota request failed");
        if key_refused_code(code) {
            return Err(PluginError::auth(msg));
        }
        return Err(PluginError::decode(msg));
    }
    let data = body.get("data");
    if data.is_none() || data.is_some_and(Value::is_null) {
        return Err(PluginError::decode("Z Code quota reply has no usage data"));
    }
    if let Some(success) = body.get("success").and_then(Value::as_bool)
        && !success
    {
        let msg = body
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or("Z Code quota request failed");
        return Err(PluginError::decode(msg));
    }
    let limits = data
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
    use hofund_plugin_api::PluginErrorKind;
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
    fn a2d_21_quota_missing_data_is_an_error() {
        let body = json!({"success": true, "code": 200});
        let error = windows_from_monitor(&body).expect_err("no data");
        assert_eq!(error.kind(), PluginErrorKind::Decode);
    }

    #[test]
    fn a2d_22_quota_refused_code_is_auth() {
        let body = json!({"success": false, "code": 1001, "msg": "key refused"});
        let error = windows_from_monitor(&body).expect_err("refused");
        assert_eq!(error.kind(), PluginErrorKind::Auth);
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
