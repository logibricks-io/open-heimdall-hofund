//! Map vendor HTTP and envelope refusals to auth errors.

use std::time::Duration;

use hofund_plugin_api::PluginError;
use hofund_plugin_sdk::json::Value;

pub(crate) const RESTRICTED_HINT: &str = "Z Code risk control blocked this account for unusual activity (code 3012); it rests for two hours while other providers can serve the request. If the Z Code app fails on this account too, only Z Code support can lift the block";
const RESTRICTED_COOLDOWN: Duration = Duration::from_secs(2 * 60 * 60);

pub(crate) fn classify_upstream_error(error: PluginError) -> PluginError {
    if error.status() != Some(405)
        || !error
            .message()
            .split(|character: char| !character.is_ascii_digit())
            .any(|digits| digits == "3012")
    {
        return error;
    }
    let message = if error.message().contains(RESTRICTED_HINT) {
        error.message().to_owned()
    } else {
        format!("{} — {RESTRICTED_HINT}", error.message())
    };
    error
        .with_message(message)
        .with_retry_after(RESTRICTED_COOLDOWN)
}

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
