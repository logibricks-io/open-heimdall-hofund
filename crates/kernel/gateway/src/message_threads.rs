//! Server-held message threads: refuse turns the gateway cannot continue.

use axum::http::StatusCode;
use serde_json::{Value, json};

/// Whether `body` asks to continue or create a server-held thread.
#[must_use]
pub fn names_thread(body: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    let Some(thread) = value.get("thread") else {
        return false;
    };
    let Some(kind) = thread.get("type").and_then(Value::as_str) else {
        return false;
    };
    matches!(kind, "create" | "continue")
}

/// Messages API 400 for a thread the gateway cannot serve.
#[must_use]
pub fn refusal_body() -> Vec<u8> {
    json!({
        "type": "error",
        "error": {
            "type": "invalid_request_error",
            "message": "Message threads are not supported; send the full conversation instead",
            "details": {
                "error_code": "thread_unsupported_request"
            }
        }
    })
    .to_string()
    .into_bytes()
}

/// Status for [`refusal_body`].
pub const REFUSAL_STATUS: StatusCode = StatusCode::BAD_REQUEST;

#[cfg(test)]
mod tests {
    use super::{REFUSAL_STATUS, names_thread, refusal_body};
    use axum::http::StatusCode;
    use serde_json::json;

    #[test]
    fn d63_1_thread_continue_refused_with_expected_shape() {
        let body = json!({
            "model": "demo",
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "hi"}],
            "thread": {"type": "continue", "previous_message_id": "msg_1"}
        });
        assert!(names_thread(body.to_string().as_bytes()));
        let err: serde_json::Value = serde_json::from_slice(&refusal_body()).unwrap();
        assert_eq!(err["error"]["type"], "invalid_request_error");
        assert_eq!(
            err["error"]["details"]["error_code"],
            "thread_unsupported_request"
        );
        assert_eq!(REFUSAL_STATUS, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn d63_2_normal_request_unaffected() {
        let body = json!({
            "model": "demo",
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "hi"}]
        });
        assert!(!names_thread(body.to_string().as_bytes()));
        let null_thread = json!({
            "model": "demo",
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "hi"}],
            "thread": null
        });
        assert!(!names_thread(null_thread.to_string().as_bytes()));
    }
}
