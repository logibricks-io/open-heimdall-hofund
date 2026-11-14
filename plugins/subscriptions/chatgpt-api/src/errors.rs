//! API refusal explanations for ChatGPT API accounts.

use hofund_plugin_api::PluginError;
use hofund_plugin_sdk::json::Value;

pub(crate) const USAGE_URL: &str = "https://chatgpt.com/settings/usage";

pub(crate) fn explain(status: u16, body: &[u8]) -> Option<String> {
    let (code, param) = error_code(body)?;
    let message = match code.as_str() {
        "subscription_sharing_usage_limit_exceeded" => format!(
            "the ChatGPT plan's usage limit, shared with ChatGPT and the other apps it is used in, is reached; see {USAGE_URL}"
        ),
        "subscription_sharing_usage_unavailable" | "subscription_sharing_user_unavailable" => {
            "OpenAI can't tell the account's usage right now; try again in a moment".to_owned()
        }
        "subscription_sharing_user_not_eligible" => {
            "this ChatGPT account can't be used through OpenAI's API (its plan isn't eligible)"
                .to_owned()
        }
        "subscription_sharing_unsupported_capability" => {
            if !param.is_empty() {
                format!("a ChatGPT sign-in can't use {param} through OpenAI's API")
            } else {
                "a ChatGPT sign-in can't use part of this request through OpenAI's API".to_owned()
            }
        }
        "subscription_sharing_route_not_supported" => {
            "a ChatGPT sign-in can't use this endpoint of OpenAI's API".to_owned()
        }
        "subscription_sharing_invalid_user" => {
            "OpenAI no longer takes this ChatGPT sign-in; sign in again".to_owned()
        }
        c if c.starts_with("chatpass_v2_") => format!("ChatGPT refused the account ({c})"),
        _ if status == 401 => "OpenAI turned the ChatGPT sign-in away; sign in again".to_owned(),
        _ => return None,
    };
    Some(message)
}

pub(crate) fn classify(status: u16, body: &[u8], error: PluginError) -> PluginError {
    let Some(hint) = explain(status, body) else {
        return error;
    };
    let message = error.message().to_owned();
    error.with_message(format!("{message} — {hint}"))
}

fn error_code(body: &[u8]) -> Option<(String, String)> {
    let value: Value = hofund_plugin_sdk::json::from_slice(body).ok()?;
    let mut code = value
        .get("error")
        .and_then(|err| err.get("code"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            value
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .or_else(|| {
            value
                .get("error")
                .and_then(|err| err.get("type"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })?;
    let param = value
        .get("error")
        .and_then(|err| err.get("param"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if code.is_empty()
        && let Some(Value::String(detail)) = value.get("detail")
    {
        for prefix in ["subscription_sharing_", "chatpass_v2_"] {
            if let Some(index) = detail.find(prefix) {
                let tail = &detail[index..];
                let end = tail
                    .char_indices()
                    .find(|(_, ch)| !(ch.is_ascii_alphanumeric() || *ch == '_'))
                    .map_or(tail.len(), |(i, _)| i);
                code = tail[..end].to_owned();
                break;
            }
        }
    }
    if code.is_empty() {
        return None;
    }
    Some((code, param))
}
