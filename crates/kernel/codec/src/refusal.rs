use hofund_ir::{StopReason, StreamEvent};
use serde_json::Value;

/// Whether an unmodeled stream event is a provider's safety refusal.
#[must_use]
pub fn refusal_error(event: &StreamEvent) -> bool {
    let StreamEvent::Unknown(event) = event else {
        return false;
    };
    refusal_reason(&event.payload).is_some()
}

pub(crate) fn refusal_reason(value: &Value) -> Option<StopReason> {
    let object = value.as_object()?;
    let error = object
        .get("error")
        .and_then(Value::as_object)
        .or_else(|| {
            object
                .get("response")
                .and_then(Value::as_object)?
                .get("error")
                .and_then(Value::as_object)
        })
        .unwrap_or(object);
    let code = error
        .get("code")
        .or_else(|| error.get("type"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if code == "refusal" {
        return Some(StopReason::Refusal);
    }
    if matches!(
        code,
        "content_filter"
            | "content_policy_violation"
            | "content_policy_rejected"
            | "sensitive"
            | "data_inspection_failed"
            | "DataInspectionFailed"
            | "SAFETY"
            | "PROHIBITED_CONTENT"
            | "BLOCKLIST"
            | "SPII"
            | "IMAGE_SAFETY"
    ) || code.ends_with("SensitiveContentDetected")
        || policy_code(code)
        || code == "invalid_prompt" && flagged(message)
        || code == "1301" && message.contains("\u{654f}\u{611f}")
        || message.trim() == "The request was rejected because it was considered high risk"
    {
        return Some(StopReason::ContentFilter);
    }
    None
}

fn policy_code(code: &str) -> bool {
    code.strip_suffix("_policy").is_some_and(|prefix| {
        !prefix.is_empty() && prefix.bytes().all(|byte| byte.is_ascii_lowercase())
    })
}

fn flagged(message: &str) -> bool {
    let lower = message.to_lowercase();
    lower.contains("flagged") || lower.contains("usage polic")
}

#[cfg(test)]
mod tests {
    use hofund_ir::{StreamEvent, UnknownEvent};
    use serde_json::json;

    use super::{refusal_error, refusal_reason};
    use hofund_ir::StopReason;

    #[test]
    fn b9_sensitive_content_refusals_are_classified() {
        for payload in [
            json!({"error":{"code":"1301","message":"\u{654f}\u{611f} content"}}),
            json!({"error":{"code":"data_inspection_failed"}}),
            json!({"code":"DataInspectionFailed"}),
            json!({"error":{"code":"OutputTextSensitiveContentDetected"}}),
            json!({"error":{"code":"InputTextSensitiveContentDetected"}}),
            json!({"response":{"error":{"code":"bio_policy"}}}),
        ] {
            assert_eq!(
                refusal_reason(&payload),
                Some(StopReason::ContentFilter),
                "{payload}"
            );
            let event = StreamEvent::Unknown(UnknownEvent {
                name: "error".to_owned(),
                payload,
            });
            assert!(refusal_error(&event));
        }
        assert_eq!(
            refusal_reason(&json!({"error":{"code":"1301","message":"rate limited"}})),
            None
        );
        assert_eq!(
            refusal_reason(&json!({"error":{"code":"1113","message":"no funds"}})),
            None
        );
    }
}
