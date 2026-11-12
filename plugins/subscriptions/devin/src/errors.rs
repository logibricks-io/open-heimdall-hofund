//! GetChatMessage error classification.

use hofund_plugin_api::PluginError;
use hofund_plugin_sdk::json::Value;

use crate::connect::chat::ConnectError;

pub(crate) fn failure(status: u16, body: &[u8]) -> Option<PluginError> {
    let text = String::from_utf8_lossy(body);
    let trimmed = text.trim();
    let mut code = String::new();
    let mut message = trimmed.to_owned();
    if let Ok(value) = hofund_plugin_sdk::json::from_str::<Value>(trimmed) {
        let object = value
            .get("error")
            .filter(|error| error.is_object())
            .unwrap_or(&value);
        code = object
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if !code.is_empty() {
            message = object
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or(&code)
                .to_owned();
        }
    }
    if message.is_empty() {
        message = format!("HTTP {status}");
    }
    map(status, &code, &message)
}

pub(crate) fn connect(error: &ConnectError) -> PluginError {
    map(200, &error.code, &error.message)
        .unwrap_or_else(|| PluginError::status_error(502, error.message.clone()))
}

fn map(status: u16, code: &str, message: &str) -> Option<PluginError> {
    let lower = message.to_ascii_lowercase();
    if code == "unauthenticated" {
        return Some(PluginError::status_error(
            401,
            format!("{message} — sign in to Devin again"),
        ));
    }
    if code == "resource_exhausted" {
        return Some(PluginError::quota_status(
            429,
            format!("usage limit reached: {message}"),
        ));
    }
    if lower.contains("quota") {
        return Some(PluginError::quota_status(
            429,
            format!("usage limit reached: {message}"),
        ));
    }
    if lower.contains("rate limit") {
        return Some(PluginError::status_error(429, message));
    }
    if lower.contains("content policy") {
        return Some(PluginError::status_error(400, message));
    }
    if lower.contains("too long")
        || lower.contains("context length")
        || lower.contains("too many tokens")
    {
        return Some(PluginError::status_error(
            400,
            format!("input is too long for the model's context: {message}"),
        ));
    }
    if code == "invalid_argument" {
        return Some(PluginError::status_error(400, message).without_cooldown());
    }
    if code == "unavailable" {
        return Some(PluginError::status_error(503, message));
    }
    if !code.is_empty() {
        return Some(PluginError::status_error(502, message));
    }
    (status >= 300).then(|| PluginError::status_error(status, message))
}

#[cfg(test)]
mod tests {
    use hofund_plugin_api::PluginErrorKind;

    use super::failure;

    #[test]
    fn body_codes_take_precedence_over_http_status() {
        let unauthenticated =
            failure(200, br#"{"code":"unauthenticated","message":"expired"}"#).expect("401");
        assert_eq!(unauthenticated.kind(), PluginErrorKind::Status);
        assert_eq!(unauthenticated.status(), Some(401));
        assert!(unauthenticated.message().contains("sign in to Devin again"));

        let exhausted = failure(
            401,
            br#"{"code":"resource_exhausted","message":"rate limit now"}"#,
        )
        .expect("429");
        assert_eq!(exhausted.kind(), PluginErrorKind::Quota);
        assert_eq!(exhausted.status(), Some(429));
        assert!(exhausted.message().starts_with("usage limit reached:"));
    }

    #[test]
    fn plain_rate_limit_is_not_a_quota_rest() {
        let error = failure(429, b"plain rate limit; retry shortly").expect("429");
        assert_eq!(error.kind(), PluginErrorKind::Status);
        assert!(!error.message().starts_with("usage limit reached:"));
        let quota = failure(500, b"daily quota used").expect("quota");
        assert_eq!(quota.kind(), PluginErrorKind::Quota);
    }

    #[test]
    fn remaining_connect_and_http_rows_keep_their_shapes() {
        let long = failure(400, b"context length exceeded").expect("long");
        assert_eq!(long.status(), Some(400));
        assert!(
            long.message()
                .starts_with("input is too long for the model's context:")
        );
        let invalid =
            failure(400, br#"{"code":"invalid_argument","message":"bad"}"#).expect("invalid");
        assert!(!invalid.cools());
        assert_eq!(
            failure(500, br#"{"code":"unavailable","message":"down"}"#)
                .expect("503")
                .status(),
            Some(503)
        );
        assert_eq!(
            failure(500, br#"{"code":"internal","message":"broken"}"#)
                .expect("502")
                .status(),
            Some(502)
        );
        assert_eq!(
            failure(402, b"payment required").expect("402").status(),
            Some(402)
        );
        assert!(failure(200, b"{}").is_none());
    }
}
