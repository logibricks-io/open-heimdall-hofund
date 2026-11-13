//! Account-level error classification.

use std::time::Duration;

use hofund_plugin_api::PluginError;
use hofund_plugin_sdk::json::Value;

pub(crate) const RESTRICTED_HINT: &str = "WorkBuddy restricted this account for unusual activity; it stays out until WorkBuddy lifts that, which an appeal (Submit feedback in WorkBuddy) asks for";
const RESTRICTED_CODE: i64 = 11140;
const RESTRICTED_COOLDOWN: Duration = Duration::from_secs(2 * 60 * 60);

pub(crate) fn classify(error: PluginError) -> PluginError {
    let Some(status) = error.status() else {
        return error;
    };
    if !restricted(status, error.message().as_bytes()) {
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

fn restricted(status: u16, body: &[u8]) -> bool {
    if status < 400 {
        return false;
    }
    if let Ok(root) = hofund_plugin_sdk::json::from_slice::<Value>(body) {
        for container in std::iter::once(&root).chain(
            ["data", "error"]
                .into_iter()
                .filter_map(|key| root.get(key)),
        ) {
            if container.get("code").and_then(restriction_code).is_some() {
                return true;
            }
        }
    }
    let text = String::from_utf8_lossy(body);
    text.to_ascii_lowercase()
        .contains("account has been restricted due to unusual activity")
        || text.contains("(11140)")
}

fn restriction_code(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number)
            if number.as_i64() == Some(RESTRICTED_CODE)
                || number.as_f64() == Some(RESTRICTED_CODE as f64) =>
        {
            Some(RESTRICTED_CODE)
        }
        Value::String(text) if text == "11140" => Some(RESTRICTED_CODE),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use hofund_plugin_api::PluginError;

    use super::{RESTRICTED_HINT, classify, restricted};

    #[test]
    fn r55_restriction_code_shapes_rest_the_account() {
        for body in [
            r#"{"code":11140,"msg":"request illegal"}"#,
            r#"{"code":"11140","msg":"request illegal"}"#,
            r#"{"data":{"code":11140},"msg":"request illegal"}"#,
            r#"{"data":{"code":"11140"},"msg":"request illegal"}"#,
            r#"{"error":{"code":11140,"message":"request illegal"}}"#,
            r#"{"error":{"code":"11140","message":"request illegal"}}"#,
        ] {
            assert!(restricted(403, body.as_bytes()));
            let error = classify(PluginError::status_error(403, body));
            assert_eq!(error.status(), Some(403));
            assert_eq!(
                error.retry_after(),
                Some(std::time::Duration::from_secs(2 * 60 * 60))
            );
            assert!(error.message().ends_with(RESTRICTED_HINT));
        }
    }

    #[test]
    fn r55_restriction_requires_an_error_and_an_exact_code() {
        assert!(!restricted(200, br#"{"code":11140}"#));
        assert!(!restricted(403, br#"{"code":111400}"#));
        assert!(!restricted(403, br#"{"error":{"code":"011140"}}"#));
        assert!(!restricted(403, br#"{"requestId":"11140"}"#));
        assert!(restricted(403, br#"{"code":11140.0}"#));
    }
}
