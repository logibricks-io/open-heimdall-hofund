//! ChatGPT account refusals on the Codex backend relay.

use hofund_plugin_sdk::HeaderMap;
use hofund_plugin_sdk::json::{self, Value};
use hofund_plugin_sdk::transport::{is_credit_error, is_quota_error};

/// Header listing fallback models as `provider/model`, comma-separated.
pub(crate) const FALLBACKS_HEADER: &str = "x-hofund-codex-fallbacks";

/// Parses fallback model ids from the request.
pub(crate) fn fallbacks_from(headers: &HeaderMap) -> Vec<String> {
    headers
        .get(FALLBACKS_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(|text| {
            text.split(',')
                .map(str::trim)
                .filter(|part| part.contains('/'))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Whether a relayed ChatGPT refusal is the account's, not the turn's.
#[must_use]
pub(crate) fn codex_account_refusal(
    status: u16,
    _headers: &HeaderMap,
    body: &[u8],
    stream: bool,
) -> bool {
    if status == 401 {
        return false;
    }
    if vendor_overloaded(status, &said(body)) {
        return true;
    }
    if status == 403 && body_contains(body, "usage_not_included") {
        return true;
    }
    if is_quota_error(status, body) || is_credit_error(status, body) {
        return true;
    }
    if stream {
        return stream_account_refusal(body);
    }
    false
}

fn stream_account_refusal(body: &[u8]) -> bool {
    let text = String::from_utf8_lossy(body);
    for line in text.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let Ok(event) = json::from_str::<Value>(data.trim()) else {
            continue;
        };
        let kind = event.get("type").and_then(Value::as_str).unwrap_or_default();
        if matches!(kind, "response.failed" | "error") {
            let message = event
                .pointer("/response/error/message")
                .or_else(|| event.pointer("/error/message"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let code = event
                .pointer("/response/error/code")
                .or_else(|| event.pointer("/error/code"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            if code == "usage_limit_reached" || is_quota_error(429, message.as_bytes()) {
                return true;
            }
        }
    }
    false
}

fn said(body: &[u8]) -> String {
    String::from_utf8_lossy(body).into_owned()
}

fn body_contains(body: &[u8], needle: &str) -> bool {
    String::from_utf8_lossy(body)
        .to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

fn vendor_overloaded(status: u16, message: &str) -> bool {
    status == 529
        || (status >= 500 && message.to_ascii_lowercase().contains("overloaded"))
}

/// Rewrites `body` to use `model` as the routed model id.
pub(crate) fn with_model(body: &[u8], model: &str) -> Option<Vec<u8>> {
    let mut value: Value = json::from_slice(body).ok()?;
    value
        .as_object_mut()?
        .insert("model".to_owned(), Value::String(model.to_owned()));
    Some(value.to_string().into_bytes())
}
