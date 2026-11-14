//! Qoder HTTP and SSE failure classification (queue code 10605, quota, request codes).

use std::time::Duration;

use hofund_plugin_api::PluginError;
use hofund_plugin_sdk::json::Value;

const QUEUED_CODE: &str = "10605";
const BUSY_WAIT_SECS: u64 = 30;

const QUOTA_CODES: &[&str] = &[
    "110", "112", "113", "114", "115", "116", "117", "118", "119", "122",
];

const REQUEST_CODES: &[&str] = &[
    "406", "416", "430", "80411", "80412", "100400", "100401", "100403",
];

/// Parsed queue state from a refusal or status reply.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct QueueState {
    pub is_queued: Option<bool>,
    pub service_available: Option<bool>,
    pub model_key: Option<String>,
    pub queue_type: Option<String>,
    pub queue_count: Option<f64>,
    pub retry_after_seconds: Option<f64>,
}

/// A classified Qoder failure before it becomes a [`PluginError`].
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Classified {
    pub status: u16,
    pub message: String,
    pub retry_after: Option<u64>,
    pub queue: Option<QueueState>,
    pub lapsed: bool,
}

impl Classified {
    #[must_use]
    pub fn into_error(self) -> PluginError {
        let mut error = if self.status == 429 {
            PluginError::quota_status(self.status, self.message)
        } else {
            PluginError::status_error(self.status, self.message)
        };
        if let Some(secs) = self.retry_after {
            error = error.with_retry_after(Duration::from_secs(secs));
        }
        error
    }

    #[must_use]
    pub fn is_queue(&self) -> bool {
        self.status == 503 && self.queue.is_some()
    }
}

/// Classifies a Qoder error body the way the reference plugin does.
#[must_use]
pub(crate) fn classify(status: u16, body: &str) -> Classified {
    let status = status.clamp(400, 599);
    let raw = body.trim().trim_start_matches('\u{feff}');
    let mut message = raw.to_owned();
    let mut code = String::new();
    let mut queue = None;

    if let Ok(mut value) = hofund_plugin_sdk::json::from_str::<Value>(raw) {
        let outer_code = value.get("code").map(string_code).unwrap_or_default();
        if matches!(outer_code.as_str(), "" | "401" | "403")
            && let Some(inner_text) = value.get("message").and_then(Value::as_str)
            && let Ok(inner) = hofund_plugin_sdk::json::from_str::<Value>(inner_text.trim())
            && !inner.is_array()
            && string_code(inner.get("code").unwrap_or(&Value::Null)) == QUEUED_CODE
        {
            value = inner;
        }
        code = value.get("code").map(string_code).unwrap_or_default();
        if let Some(text) = value.get("message").and_then(Value::as_str) {
            message = text.to_owned();
        }
        if let Some(details) = value.get("details") {
            let details = if let Some(text) = details.as_str() {
                hofund_plugin_sdk::json::from_str(text.trim()).ok()
            } else {
                Some(details.clone())
            };
            if let Some(inner) = details
                && let Some(extra) = inner.pointer("/error/message").and_then(Value::as_str)
            {
                message = format!("{message}: {extra}");
            }
        }
        if let Some(object) = value.get("queue").and_then(Value::as_object) {
            queue = Some(queue_from_value(&Value::Object(object.clone())));
        } else if let Some(text) = value.get("message").and_then(Value::as_str)
            && text.trim().starts_with('{')
            && let Ok(parsed) = hofund_plugin_sdk::json::from_str::<Value>(text.trim())
            && queue_like(&parsed)
        {
            queue = Some(queue_from_value(&parsed));
        }
    }

    if message.is_empty() {
        message = http_status_phrase(status);
    }

    if code == QUEUED_CODE
        || queue
            .as_ref()
            .is_some_and(|state| state.is_queued == Some(true))
        || queue
            .as_ref()
            .is_some_and(|state| state.service_available == Some(false))
    {
        let state = queue.unwrap_or_default();
        let wait = retry_after_secs(state.retry_after_seconds);
        let model = state
            .model_key
            .as_ref()
            .map(|key| format!(" for {key}"))
            .unwrap_or_default();
        let ahead = state
            .queue_count
            .filter(|count| *count > 0.0)
            .map(|count| format!(", {} ahead", count.floor()))
            .unwrap_or_default();
        let tier = state.queue_type.as_ref().map_or_else(
            || {
                if ahead.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", ahead.trim_start_matches(", "))
                }
            },
            |kind| format!(" (this plan's queue: {kind}{ahead})"),
        );
        let what = if state.is_queued == Some(true) {
            "queued the request"
        } else {
            "is busy"
        };
        return Classified {
            status: 503,
            message: format!("Qoder {what}{model}{tier}; try again in {wait}s"),
            retry_after: Some(wait),
            queue: Some(state),
            lapsed: false,
        };
    }

    if QUOTA_CODES.contains(&code.as_str()) {
        return Classified {
            status: 429,
            message: format!("usage limit reached: {message}"),
            retry_after: None,
            queue: None,
            lapsed: false,
        };
    }

    if REQUEST_CODES.contains(&code.as_str()) {
        return Classified {
            status: 400,
            message: format!("Qoder refused the request ({code}): {message}"),
            retry_after: None,
            queue: None,
            lapsed: false,
        };
    }

    if upstream_failed(&message) {
        return Classified {
            status: 502,
            message,
            retry_after: None,
            queue: None,
            lapsed: false,
        };
    }

    if status == 401 || status == 403 {
        let said = if !message.is_empty() && message != http_status_phrase(status) {
            format!(
                " (Qoder said {status}: {})",
                message.chars().take(300).collect::<String>()
            )
        } else {
            String::new()
        };
        return Classified {
            status: 401,
            message: format!("the sign-in lapsed — sign in again{said}"),
            retry_after: None,
            queue: None,
            lapsed: true,
        };
    }

    if status == 429 || message.to_ascii_lowercase().contains("quota") {
        let seen = format!(
            "Qoder replied HTTP {status}{}",
            if code.is_empty() {
                String::new()
            } else {
                format!(", code {code}")
            }
        );
        let body = if !raw.is_empty() && raw != message {
            format!(": {}", raw.chars().take(400).collect::<String>())
        } else {
            String::new()
        };
        return Classified {
            status: 429,
            message: format!("usage limit reached: {message} ({seen}{body})"),
            retry_after: None,
            queue: None,
            lapsed: false,
        };
    }

    Classified {
        status,
        message,
        retry_after: None,
        queue: None,
        lapsed: false,
    }
}

/// Reads queue state from a `/queue/status` reply, as the reference client does.
#[must_use]
pub(crate) fn queue_state(body: &str) -> Option<QueueState> {
    let root = hofund_plugin_sdk::json::from_str::<Value>(body.trim()).ok()?;
    find_queue_state(&root)
}

fn find_queue_state(root: &Value) -> Option<QueueState> {
    let mut stack = vec![root.clone()];
    let mut seen = 0usize;
    while let Some(value) = stack.pop() {
        if !value.is_object() {
            continue;
        }
        seen += 1;
        if seen > 64 {
            break;
        }
        let state = queue_from_value(&value);
        if state.is_queued.is_some() || state.service_available.is_some() {
            return Some(state);
        }
        for key in ["data", "result", "message", "body"] {
            match value.get(key) {
                Some(Value::Object(_)) => stack.push(value.get(key).expect("key").clone()),
                Some(Value::String(text)) => {
                    if let Ok(parsed) = hofund_plugin_sdk::json::from_str(text.trim()) {
                        stack.push(parsed);
                    }
                }
                _ => {}
            }
        }
    }
    None
}

fn queue_from_value(value: &Value) -> QueueState {
    QueueState {
        is_queued: bool_field(value.get("isQueued")),
        service_available: bool_field(value.get("serviceAvailable")),
        model_key: value
            .get("modelKey")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_owned),
        queue_type: value
            .get("queueType")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_owned),
        queue_count: value.get("queueCount").and_then(Value::as_f64),
        retry_after_seconds: number_field(value.get("retryAfterSeconds")),
    }
}

fn queue_like(value: &Value) -> bool {
    value.get("isQueued").is_some()
        || value.get("serviceAvailable").is_some()
        || value.get("retryAfterSeconds").is_some()
}

fn bool_field(value: Option<&Value>) -> Option<bool> {
    match value {
        Some(Value::Bool(flag)) => Some(*flag),
        Some(Value::String(text)) => match text.as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn number_field(value: Option<&Value>) -> Option<f64> {
    match value {
        None | Some(Value::Null) => None,
        Some(Value::Number(number)) => number.as_f64(),
        Some(Value::String(text)) if text.is_empty() => None,
        Some(Value::String(text)) => text.parse().ok(),
        _ => None,
    }
}

fn string_code(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_i64().map(|number| number.to_string()))
        .or_else(|| value.as_u64().map(|number| number.to_string()))
        .unwrap_or_default()
}

fn retry_after_secs(seconds: Option<f64>) -> u64 {
    seconds
        .filter(|value| value.is_finite() && *value > 0.0)
        .map(|value| value.ceil() as u64)
        .unwrap_or(BUSY_WAIT_SECS)
}

fn upstream_failed(message: &str) -> bool {
    message
        .to_ascii_lowercase()
        .contains("error in upstream response")
}

fn http_status_phrase(status: u16) -> String {
    match status {
        400 => "Bad Request".to_owned(),
        401 => "Unauthorized".to_owned(),
        403 => "Forbidden".to_owned(),
        404 => "Not Found".to_owned(),
        429 => "Too Many Requests".to_owned(),
        500 => "Internal Server Error".to_owned(),
        502 => "Bad Gateway".to_owned(),
        503 => "Service Unavailable".to_owned(),
        other => format!("HTTP {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{QUEUED_CODE, QueueState, classify, queue_state};

    #[test]
    fn r54e_queue_fixture_is_busy_with_plan_queue() {
        let body = include_str!("fixtures/queued-10605.json");
        let failure = classify(403, body);
        assert_eq!(failure.status, 503);
        assert_eq!(failure.retry_after, Some(30));
        assert_eq!(
            failure.message,
            "Qoder is busy for efficient (this plan's queue: p3); try again in 30s"
        );
        assert!(!failure.lapsed);
    }

    #[test]
    fn r54e_queued_request_wording_and_ceil_wait() {
        use hofund_plugin_sdk::json::json;
        let body = json!({
            "code": QUEUED_CODE,
            "message": json!({
                "isQueued": true,
                "modelKey": "ultimate",
                "retryAfterSeconds": 12.2
            })
            .to_string()
        })
        .to_string();
        let failure = classify(403, &body);
        assert_eq!(failure.status, 503);
        assert_eq!(failure.retry_after, Some(13));
        assert_eq!(
            failure.message,
            "Qoder queued the request for ultimate; try again in 13s"
        );
    }

    #[test]
    fn r54e_queue_state_under_429_keeps_wait() {
        use hofund_plugin_sdk::json::json;
        let body = json!({
            "code": "",
            "message": json!({
                "serviceAvailable": false,
                "retryAfterSeconds": 5
            })
            .to_string()
        })
        .to_string();
        let failure = classify(429, &body);
        assert_eq!(failure.status, 503);
        assert_eq!(failure.retry_after, Some(5));
    }

    #[test]
    fn r54e_quota_codes_stay_usage_limit_not_queue() {
        for code in [
            "110", "112", "113", "114", "115", "116", "117", "118", "119", "122",
        ] {
            let body = format!(r#"{{"code":"{code}","message":"credits used up"}}"#);
            let failure = classify(403, &body);
            assert_eq!(failure.status, 429, "{code}");
            assert_eq!(failure.message, "usage limit reached: credits used up");
        }
    }

    #[test]
    fn r54e_request_codes_are_400_not_lapsed() {
        for code in [
            "406", "416", "430", "80411", "80412", "100400", "100401", "100403",
        ] {
            let failure = classify(403, &format!(r#"{{"code":"{code}","message":"no"}}"#));
            assert_eq!(failure.status, 400, "{code}");
            assert!(
                failure
                    .message
                    .starts_with(&format!("Qoder refused the request ({code}):"))
            );
            assert!(!failure.lapsed);
        }
    }

    #[test]
    fn r54e_login_code_and_empty_body_lapse() {
        let failure = classify(403, r#"{"code":"105","message":"login expired"}"#);
        assert_eq!(failure.status, 401);
        assert!(failure.lapsed);
        assert!(failure.message.contains("sign-in lapsed"));
        let empty = classify(403, "");
        assert_eq!(empty.status, 401);
        assert!(empty.lapsed);
    }

    #[test]
    fn r54e_explicit_codes_beat_wrapped_queue_text() {
        use hofund_plugin_sdk::json::json;
        let queued = include_str!("fixtures/queued-10605.json");
        for (code, status) in [("105", 401), ("110", 429), ("406", 400)] {
            let body = json!({ "code": code, "message": queued }).to_string();
            assert_eq!(classify(403, &body).status, status, "{code}");
        }
        assert_eq!(
            classify(403, r#"{"message":"{\"code\":\"10605\""}"#).status,
            401
        );
        assert_eq!(
            classify(403, r#"{"message":"[{\"code\":\"10605\"}]"}"#).status,
            401
        );
    }

    #[test]
    fn r54e_queue_state_parses_nested_data() {
        let parsed =
            queue_state(r#"{"data":{"isQueued":true,"queueType":"p3","retryAfterSeconds":"7"}}"#)
                .expect("state");
        assert_eq!(
            parsed,
            QueueState {
                is_queued: Some(true),
                service_available: None,
                model_key: None,
                queue_type: Some("p3".to_owned()),
                queue_count: None,
                retry_after_seconds: Some(7.0),
            }
        );
    }
}
