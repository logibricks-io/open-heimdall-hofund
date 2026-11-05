//! Map Zed cloud failures to plugin errors the router understands.

use hofund_plugin_api::PluginError;
use hofund_plugin_sdk::json::Value;

/// Whether a reply says the model token should be minted again.
#[must_use]
pub(crate) fn token_stale(status: u16, headers: &[(String, String)]) -> bool {
    if status == 401 {
        return true;
    }
    headers.iter().any(|(name, _)| {
        name.eq_ignore_ascii_case("x-zed-expired-token")
            || name.eq_ignore_ascii_case("x-zed-outdated-token")
    })
}

/// HTTP status implied by a failure code string from Zed's stream or JSON body.
#[must_use]
pub(crate) fn failed_status(code: &str) -> u16 {
    for prefix in ["upstream_http_", "http_"] {
        if let Some(rest) = code.strip_prefix(prefix)
            && let Ok(n) = rest.parse::<u16>()
            && (400..600).contains(&n)
        {
            return n;
        }
    }
    let lower = code.to_ascii_lowercase();
    if lower.contains("rate_limit") {
        return 429;
    }
    if lower.contains("overloaded") {
        return 529;
    }
    if lower.contains("billing") || lower.contains("payment") {
        return 402;
    }
    if lower.contains("context_length") {
        return 400;
    }
    502
}

fn parse_json(text: &str) -> Option<Value> {
    hofund_plugin_sdk::json::from_str(text).ok()
}

/// A short message from an error JSON body or raw text.
#[must_use]
pub(crate) fn failure_message(status: u16, body: &str) -> String {
    if let Some(j) = parse_json(body) {
        if let Some(msg) = j.get("message").and_then(Value::as_str) {
            return msg.to_owned();
        }
        if let Some(err) = j.get("error").and_then(Value::as_str) {
            return err.to_owned();
        }
        if let Some(code) = j.get("code").and_then(Value::as_str) {
            return code.to_owned();
        }
    }
    let trimmed = body.trim();
    if !trimmed.is_empty() && trimmed.len() < 500 {
        return trimmed.to_owned();
    }
    if status == 402 {
        return "payment required: this Zed account has no plan that calls models, or its allowance is used up".to_owned();
    }
    match status {
        400 => "bad request".to_owned(),
        401 => "unauthorized".to_owned(),
        403 => "forbidden".to_owned(),
        404 => "not found".to_owned(),
        429 => "too many requests".to_owned(),
        502 => "bad gateway".to_owned(),
        529 => "overloaded".to_owned(),
        _ => "upstream error".to_owned(),
    }
}

/// Status and message for a refused HTTP reply, as the reference maps it.
#[must_use]
pub(crate) fn refusal(status: u16, body: &str) -> (u16, String) {
    if status == 402 {
        return (
            402,
            "payment required — this account's plan doesn't include Zed's hosted models, or its allowance is used up (see zed.dev/account)".to_owned(),
        );
    }
    if status == 401 {
        return (401, "the sign-in was refused — sign in again".to_owned());
    }
    let j = parse_json(body).unwrap_or(Value::Null);
    let mut code = status;
    if let Some(up) = j.get("upstream_status").and_then(Value::as_u64)
        && (400..600).contains(&up)
    {
        code = up as u16;
    } else if let Some(raw) = j.get("code").and_then(Value::as_str) {
        let mapped = failed_status(raw);
        if mapped != 502 {
            code = mapped;
        }
    }
    if !(400..600).contains(&code) {
        code = 502;
    }
    let mut msg = failure_message(status, body);
    if let Some(secs) = j.get("retry_after").and_then(Value::as_u64)
        && secs > 0
    {
        msg.push_str(&format!(" (retry after {secs}s)"));
    }
    (code, msg)
}

/// Turns a classified HTTP status into a plugin error kind.
pub(crate) fn map_http(status: u16, message: impl Into<String>) -> PluginError {
    let message = message.into();
    match status {
        401 | 403 => PluginError::status_error(status, message),
        402 => PluginError::credit_status(402, message),
        429 => PluginError::status_error(status, message),
        other => PluginError::status_error(other, message),
    }
}

/// Sign-in refused while minting a model token.
pub(crate) fn sign_in_expired(who: &str) -> PluginError {
    PluginError::auth(format!(
        "{who}: the Zed sign-in has expired — sign in again"
    ))
}
