//! Quota and verification replies from the agy bridge.

use hofund_plugin_api::PluginError;
use hofund_plugin_sdk::google_rest::{asks_for_verification, is_model_pool_quota};
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::note_google_sign_in;

/// A refusal found in `text`, or `None` when the text is a normal reply.
#[must_use]
pub(crate) fn classify_output(text: &str) -> Option<PluginError> {
    text.lines()
        .map(str::trim)
        .find_map(|line| classify_line(line).or_else(|| classify_raw(line)))
}

fn classify_line(line: &str) -> Option<PluginError> {
    let value = hofund_plugin_sdk::json::from_str::<Value>(line).ok()?;
    if let Some(kind) = value.get("error").and_then(Value::as_str) {
        let message = value.get("message").and_then(Value::as_str).unwrap_or(line);
        let link = value.get("link").and_then(Value::as_str).unwrap_or("");
        let status = value
            .get("status")
            .and_then(Value::as_u64)
            .and_then(|status| u16::try_from(status).ok());
        return Some(match kind {
            "quota" => quota_error(message),
            "verify" => verify_error(message, link),
            _ => {
                let status = status.unwrap_or(400);
                note_google_sign_in(PluginError::status_error(status, message))
            }
        });
    }
    phrase_fields(&value)
}

/// A pool-quota or verification sentence in a success field is still a rest.
fn phrase_fields(value: &Value) -> Option<PluginError> {
    let text = value.get("text").and_then(Value::as_str);
    let nested = value
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str);
    for field in [text, nested].into_iter().flatten() {
        if let Some(error) = classify_phrase(field) {
            return Some(error);
        }
    }
    None
}

fn classify_phrase(text: &str) -> Option<PluginError> {
    if asks_for_verification(text) {
        return Some(verify_error(text, first_https(text).unwrap_or("")));
    }
    if is_model_pool_quota(text) {
        return Some(quota_error(text));
    }
    None
}

fn classify_raw(line: &str) -> Option<PluginError> {
    if line.is_empty() || line.starts_with('{') {
        return None;
    }
    if asks_for_verification(line) {
        return Some(verify_error(line, first_https(line).unwrap_or("")));
    }
    if is_model_pool_quota(line) || line.to_ascii_lowercase().contains("quota") {
        return Some(quota_error(line));
    }
    None
}

pub(crate) fn quota_error(message: &str) -> PluginError {
    let mut error = PluginError::quota_status(
        429,
        format!("agy bridge: Antigravity quota reached: {message}"),
    );
    if let Some(after) = hofund_plugin_sdk::transport::retry_after_from_body(message.as_bytes()) {
        error = error.with_reset_after(after).with_retry_after(after);
    }
    note_google_sign_in(error)
}

pub(crate) fn verify_error(raw: &str, link: &str) -> PluginError {
    let quoted = raw.replace('"', "'");
    let advice = if link.starts_with("https://") {
        format!(
            "this Google account needs to be verified: open <{link}> in a browser signed in to it, verify it, then try again"
        )
    } else {
        "this Google account needs to be verified: open the Antigravity app, or run `agy` in a terminal, signed in to it and do what it asks, then try again".to_owned()
    };
    let message = format!(
        "agy bridge: Google wants this account verified before Antigravity serves it again (agy: \"{quoted}\") — {advice}"
    );
    note_google_sign_in(PluginError::status_error(403, message))
}

fn first_https(text: &str) -> Option<&str> {
    let start = text.find("https://")?;
    let rest = &text[start..];
    let end = rest
        .find(|character: char| {
            character.is_whitespace()
                || matches!(character, '"' | '\'' | '<' | '>' | ')' | ']' | ',')
        })
        .unwrap_or(rest.len());
    Some(&rest[..end])
}
