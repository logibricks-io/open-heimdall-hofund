//! Map vendor HTTP and envelope refusals to auth errors.

use hofund_plugin_api::PluginError;
use hofund_plugin_sdk::json::Value;

pub(crate) fn auth_for_http_status(status: u16, context: &str) -> PluginError {
    if status == 401 || status == 403 {
        return PluginError::auth(format!("{context} (HTTP {status})"));
    }
    PluginError::status_error(status, format!("{context} (HTTP {status})"))
}

pub(crate) fn parse_envelope_code(value: Option<&Value>) -> Option<i64> {
    match value {
        Some(Value::Number(number)) => number.as_i64(),
        Some(Value::String(text)) => text.trim().trim_matches('"').parse().ok(),
        _ => None,
    }
}

pub(crate) fn key_refused_code(code: i64) -> bool {
    (1000..=1005).contains(&code)
}

pub(crate) fn refusal_from_monitor_body(body: &Value) -> Option<PluginError> {
    if body.get("success").and_then(Value::as_bool) != Some(false) {
        return None;
    }
    let code = parse_envelope_code(body.get("code")).unwrap_or(0);
    let msg = body
        .get("msg")
        .and_then(Value::as_str)
        .unwrap_or("Z Code refused the sign-in");
    if key_refused_code(code) {
        Some(PluginError::auth(msg))
    } else {
        Some(PluginError::decode(msg))
    }
}
