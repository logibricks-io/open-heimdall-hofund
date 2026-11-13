//! Connect and HTTP failure classification for a text-only Run.

use hofund_plugin_api::PluginError;
use hofund_plugin_sdk::json::Value;

/// The codec's non-cooling empty-answer wording, copied across the crate boundary.
pub(crate) const EMPTY_REPLY: &str = "an empty reply (the model answered nothing; try again)";
/// A later stall is distinct from a Run that never produced text.
pub(crate) const STALL_AFTER_TEXT: &str = "the upstream stopped sending";

const STALL_PREFIX: &str = "upstream Run ";

/// A pre-output idle or quiet deadline, including the last observed frame kind.
pub(crate) fn stall(kind: &str, last_frame: &str) -> PluginError {
    PluginError::status_error(
        502,
        format!("{STALL_PREFIX}{kind} stall; last server frame: {last_frame}"),
    )
}

/// Rewords only a transport stall after the caller has received text.
pub(crate) fn after_output(error: PluginError) -> PluginError {
    if error.kind() == hofund_plugin_api::PluginErrorKind::Timeout {
        return PluginError::status_error(
            502,
            format!("{STALL_AFTER_TEXT}: idle stall; last server frame: response body"),
        );
    }
    let Some(diagnostic) = error.message().strip_prefix(STALL_PREFIX) else {
        return error;
    };
    PluginError::status_error(502, format!("{STALL_AFTER_TEXT}: {diagnostic}"))
}

/// Maps a transport timeout that fires before any text to the idle stall wording.
pub(crate) fn timeout_before_text(error: PluginError) -> PluginError {
    if error.kind() == hofund_plugin_api::PluginErrorKind::Timeout {
        stall("idle", "response body")
    } else {
        error
    }
}

/// Cursor's refusal of a model asked for without Max Mode.
///
/// Matches `max[ _]mode[ _]required` or `requires max mode`, case-insensitive.
pub(crate) fn max_required(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("requires max mode") || lower.replace('_', " ").contains("max mode required")
}

/// Maps a Connect trailer without requiring a socket or a transport error.
pub(crate) fn trailer(payload: &[u8]) -> PluginError {
    match connect_fields(payload) {
        Some((code, message)) => classify(0, &code, &message),
        None => classify(0, "", String::from_utf8_lossy(payload).trim()),
    }
}

/// Maps an HTTP error body, preserving its status when it has no Connect code.
pub(crate) fn body(status: u16, bytes: &[u8]) -> PluginError {
    if let Some((code, message)) = connect_fields(bytes) {
        return classify(status, &code, &message);
    }
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim();
    let message = if text.is_empty() {
        status_text(status).to_owned()
    } else {
        text.to_owned()
    };
    classify(status, "", &message)
}

/// Reads Connect `code` and the best available message, including debug details.
fn connect_fields(bytes: &[u8]) -> Option<(String, String)> {
    let parsed = hofund_plugin_sdk::json::from_slice::<Value>(bytes).ok()?;
    let object = parsed.get("error").unwrap_or(&parsed);
    let code = object.get("code").and_then(Value::as_str);
    let base = object
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let detail = object
        .get("details")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|detail| {
            let pair = detail.get("debug")?.get("details")?;
            let title = pair
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let text = pair
                .get("detail")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let joined = format!("{title}: {text}");
            (joined != ": ").then_some(joined)
        })
        .collect::<Vec<_>>()
        .join("; ");
    if code.is_none() && detail.is_empty() {
        return None;
    }
    let message = if !detail.is_empty() {
        detail
    } else {
        base.to_owned()
    };
    Some((code.unwrap_or_default().to_owned(), message))
}

/// Whether the server explicitly assigned the account to another region.
pub(crate) fn regional(message: &str) -> bool {
    message.to_ascii_lowercase().contains("region")
}

fn classify(status: u16, code: &str, message: &str) -> PluginError {
    let lower = message.to_ascii_lowercase();
    let message = if message.trim().is_empty() {
        "upstream Run failed".to_owned()
    } else {
        message.trim().to_owned()
    };
    if regional(&message) {
        return PluginError::status_error(403, message);
    }
    if code == "permission_denied" {
        return PluginError::status_error(403, message);
    }
    if code == "unauthenticated" || status == 401 || lower.contains("expired") {
        return PluginError::status_error(401, format!("{message}; sign in to Cursor again"));
    }
    if code == "resource_exhausted" || lower.contains("quota") || lower.contains("usage limit") {
        return PluginError::quota_status(429, format!("usage limit reached: {message}"));
    }
    if lower.contains("rate limit") {
        return PluginError::status_error(429, message);
    }
    if lower.contains("too long")
        || lower.contains("context length")
        || lower.contains("too many tokens")
    {
        return PluginError::status_error(
            400,
            format!("input is too long for the model's context: {message}"),
        );
    }
    if code == "invalid_argument" {
        let error = PluginError::status_error(400, message.clone());
        return if unserved_model(&message) {
            error
        } else {
            error.without_cooldown()
        };
    }
    if code == "unavailable" {
        return PluginError::status_error(503, message);
    }
    if !code.is_empty() {
        return PluginError::status_error(502, message);
    }
    PluginError::status_error(status, message)
}

fn unserved_model(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("model_not_found")
        || lower.contains("no such model")
        || lower.contains("unknown model")
        || lower.contains("invalid model")
        || lower.contains("unsupported model")
        || (lower.contains("model")
            && [
                "not supported",
                "not accessible",
                "not available",
                "not found",
                "not enabled",
                "not allowed",
                "does not exist",
                "doesn't exist",
            ]
            .into_iter()
            .any(|words| lower.contains(words)))
}

fn status_text(status: u16) -> &'static str {
    match status {
        400 => "Bad Request",
        401 => "Unauthorized",
        402 => "Payment Required",
        403 => "Forbidden",
        404 => "Not Found",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        413 => "Payload Too Large",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "upstream Run failed",
    }
}

#[cfg(test)]
mod tests {
    use hofund_plugin_api::PluginErrorKind;

    use super::{body, classify, trailer};

    #[test]
    fn connect_failures_follow_the_documented_order() {
        let regional = classify(401, "unauthenticated", "account REGION is not served here");
        assert_eq!(regional.status(), Some(403));
        assert_eq!(regional.kind(), PluginErrorKind::Status);
        assert!(
            regional
                .message()
                .contains("account REGION is not served here")
        );

        let denied = classify(401, "permission_denied", "denied by policy");
        assert_eq!(denied.status(), Some(403));
        assert!(denied.message().contains("denied by policy"));

        for (status, code, message) in [
            (400, "unauthenticated", "token rejected"),
            (400, "", "trial expired"),
            (401, "", "unauthorized"),
        ] {
            let error = classify(status, code, message);
            assert_eq!(error.status(), Some(401));
            assert_eq!(error.kind(), PluginErrorKind::Status);
            assert!(error.message().contains(message));
            assert!(error.message().contains("sign in to Cursor again"));
        }
    }

    #[test]
    fn quota_rate_limit_and_context_overflow_are_distinct() {
        for (code, message) in [
            ("resource_exhausted", "slow down"),
            ("", "quota exhausted"),
            ("invalid_argument", "usage limit reached for this plan"),
        ] {
            let error = classify(400, code, message);
            assert_eq!(error.status(), Some(429));
            assert_eq!(error.kind(), PluginErrorKind::Quota);
            assert!(error.message().starts_with("usage limit reached: "));
            assert!(error.message().contains(message));
        }

        let rate = classify(400, "resource_exhausted", "rate limit and quota");
        assert_eq!(rate.kind(), PluginErrorKind::Quota, "quota wins first");
        let rate = classify(429, "", "rate limit; try later");
        assert_eq!(rate.status(), Some(429));
        assert_eq!(rate.kind(), PluginErrorKind::Status);

        for message in [
            "prompt is too long",
            "context length exceeded",
            "too many tokens in the prompt",
        ] {
            let error = classify(400, "invalid_argument", message);
            assert_eq!(error.status(), Some(400));
            assert!(
                error
                    .message()
                    .starts_with("input is too long for the model's context: ")
            );
            assert!(error.message().contains(message));
        }
    }

    #[test]
    fn remaining_connect_codes_keep_the_server_text() {
        let invalid = classify(400, "invalid_argument", "bad Run shape");
        assert_eq!(invalid.status(), Some(400));
        assert_eq!(invalid.message(), "bad Run shape");
        assert!(!invalid.cools());

        let unserved = classify(400, "invalid_argument", "model is not available");
        assert_eq!(unserved.status(), Some(400));
        assert_eq!(unserved.message(), "model is not available");
        assert!(unserved.cools());

        let unavailable = classify(500, "unavailable", "temporarily unavailable");
        assert_eq!(unavailable.status(), Some(503));
        assert_eq!(unavailable.message(), "temporarily unavailable");

        let deadline = classify(504, "deadline_exceeded", "append timed out");
        assert_eq!(deadline.status(), Some(502));
        assert_eq!(deadline.message(), "append timed out");
    }

    #[test]
    fn trailer_json_supports_the_error_wrapper_and_debug_details() {
        let error = trailer(
            br#"{"error":{"code":"invalid_argument","message":"bad input","details":[{"debug":{"details":{"title":"Prompt title","detail":"more detail"}}}]}}"#,
        );
        assert_eq!(error.status(), Some(400));
        assert!(error.message().contains("Prompt title: more detail"));
    }

    #[test]
    fn http_body_reads_debug_details_for_classification() {
        let error = body(
            401,
            br#"{"error":{"code":"unknown","message":"request failed","details":[{"debug":{"details":{"title":"Routing","detail":"account is in another region"}}}]}}"#,
        );
        assert_eq!(error.status(), Some(403));
        assert!(error.message().contains("region"));
        assert!(!super::regional("request failed"));
    }

    #[test]
    fn unserved_model_phrases_cool_and_nearby_wording_does_not() {
        for message in [
            "model_not_found",
            "no such model",
            "unknown model",
            "invalid model",
            "unsupported model",
            "model is not supported",
            "model is not accessible",
            "model is not available",
            "model is not found",
            "model is not enabled",
            "model is not allowed",
            "model does not exist",
            "model doesn't exist",
        ] {
            let error = classify(400, "invalid_argument", message);
            assert_eq!(error.status(), Some(400), "{message}");
            assert_eq!(error.message(), message, "{message}");
            assert!(error.cools(), "{message}");
        }

        for message in [
            "the feature is not available",
            "model name rejected",
            "bad Run shape",
        ] {
            let error = classify(400, "invalid_argument", message);
            assert_eq!(error.status(), Some(400), "{message}");
            assert!(!error.cools(), "{message}");
        }
    }

    #[test]
    fn empty_http_bodies_use_the_status_text() {
        for (status, text) in [
            (400, "Bad Request"),
            (401, "Unauthorized"),
            (402, "Payment Required"),
            (403, "Forbidden"),
            (404, "Not Found"),
            (408, "Request Timeout"),
            (409, "Conflict"),
            (410, "Gone"),
            (413, "Payload Too Large"),
            (422, "Unprocessable Entity"),
            (429, "Too Many Requests"),
            (500, "Internal Server Error"),
            (501, "Not Implemented"),
            (502, "Bad Gateway"),
            (503, "Service Unavailable"),
            (504, "Gateway Timeout"),
        ] {
            let error = body(status, b"");
            assert_eq!(error.status(), Some(status), "{status}");
            assert!(
                error.message().contains(text),
                "{status}: {}",
                error.message()
            );
        }
    }

    #[test]
    fn http_failures_without_a_connect_code_keep_the_http_status() {
        for (status, bytes, expected) in [
            (429, b"".as_slice(), "Too Many Requests"),
            (402, b"payment required".as_slice(), "payment required"),
            (503, b"not json".as_slice(), "not json"),
        ] {
            let error = body(status, bytes);
            assert_eq!(error.status(), Some(status));
            assert_eq!(error.kind(), PluginErrorKind::Status);
            assert!(error.message().contains(expected));
        }
    }
}
