//! Maps Claude Code stream and process failures to [`PluginError`].

use hofund_plugin_api::{PluginError, redact_token};

use super::decode::ResultError;

/// Client-facing text when Claude Code exits before a complete turn.
pub(crate) const ENDED: &str = "Claude Code ended";

/// Extra-usage 400 wording that must not match router quota or rate-limit words.
pub(crate) const EXTRA_USAGE_SAID: &str = "extra usage is not available on this account";

const SIGN_IN_NEEDLES: &[&str] = &[
    "OAuth session expired and could not be refreshed",
    "OAuth token revoked",
    "OAuth access token has been revoked",
    crate::creds::LAPSED_REASON,
];

/// Whether `message` is a sign-in lapse the account should be marked for.
#[must_use]
pub(crate) fn is_sign_in_required(message: &str) -> bool {
    SIGN_IN_NEEDLES
        .iter()
        .any(|needle| contains_ignore_ascii_case(message, needle))
}

/// Whether a lapsed mark recorded at `captured` still applies to `current`.
///
/// A credential rotated after the run started must not inherit the refusal.
#[must_use]
pub(crate) fn should_mark_lapsed(captured: Option<u64>, current: Option<u64>) -> bool {
    match (captured, current) {
        (Some(start), Some(now)) => start == now,
        _ => false,
    }
}

/// Maps a terminal `result` error to a [`PluginError`].
#[must_use]
pub(crate) fn classify(error: &ResultError) -> PluginError {
    classify_parts(
        error.api_error_status,
        &error.message,
        error.kind.as_deref(),
    )
}

/// Maps a status, message, and optional kind string to a [`PluginError`].
#[must_use]
pub(crate) fn classify_parts(
    api_error_status: Option<u16>,
    message: &str,
    _kind: Option<&str>,
) -> PluginError {
    if is_sign_in_required(message) {
        return PluginError::status_error(bridge_status(api_error_status), message)
            .without_cooldown();
    }
    if is_extra_usage(message) {
        return PluginError::status_error(400, safe_extra_usage_message(message))
            .without_cooldown();
    }
    if looks_too_long(message) {
        return PluginError::status_error(400, message);
    }
    let quota_status = api_error_status.unwrap_or(429);
    if quota_shaped(quota_status, message) {
        return PluginError::quota_status(api_error_status.unwrap_or(429), message);
    }
    PluginError::status_error(bridge_status(api_error_status), message)
}

fn bridge_status(api_error_status: Option<u16>) -> u16 {
    match api_error_status {
        Some(status) if status >= 400 => status,
        _ => 502,
    }
}

fn is_extra_usage(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("extra usage") || lower.contains("third-party") || lower.contains("third party")
}

fn safe_extra_usage_message(message: &str) -> String {
    let lower = message.to_ascii_lowercase();
    if lower.contains("quota")
        || lower.contains("rate limit")
        || lower.contains("rate-limit")
        || lower.contains("usage limit")
        || lower.contains("exceeded")
        || lower.contains("too many requests")
    {
        EXTRA_USAGE_SAID.to_owned()
    } else {
        message.to_owned()
    }
}

fn looks_too_long(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    if lower.contains("rate limit")
        || lower.contains("rate-limit")
        || lower.contains("try again later")
        || lower.contains("per minute")
        || lower.contains("per hour")
        || lower.contains("max_tokens")
        || lower.contains("max_output_tokens")
    {
        return false;
    }
    lower.contains("prompt is too long")
        || lower.contains("prompt too long")
        || lower.contains("context_length_exceeded")
        || lower.contains("maximum context length")
        || lower.contains("context window exceeds")
}

fn quota_shaped(status: u16, message: &str) -> bool {
    hofund_plugin_sdk::transport::is_quota_error(status, message.as_bytes())
}

/// Whether `stdout` already carries a terminal `result` envelope.
#[must_use]
pub(crate) fn stream_finished(stdout: &str) -> bool {
    let mut decoder = super::decode::Decoder::new();
    stdout.lines().any(|line| {
        matches!(
            decoder.push_line(line),
            Some(super::decode::Step::ResultError(_) | super::decode::Step::Completed)
        )
    })
}

/// A process that ended before a complete turn, with stderr redacted.
#[must_use]
pub(crate) fn process_ended(stderr: &str, secrets: &[&str]) -> PluginError {
    let mut message = ENDED.to_owned();
    let tail = redact_all(stderr, secrets);
    let tail = tail.trim();
    if !tail.is_empty() {
        message.push_str(": ");
        message.push_str(tail);
    }
    PluginError::status_error(502, message)
}

/// A turn that ran past its deadline.
///
/// Live `runTimeout` wrapping around `LongLivedChild` is not hooked yet.
#[must_use]
#[allow(dead_code)]
pub(crate) fn timed_out(message: impl Into<String>) -> PluginError {
    PluginError::timeout(message)
}

fn redact_all(text: &str, secrets: &[&str]) -> String {
    let mut clean = text.to_owned();
    for secret in secrets {
        clean = redact_token(&clean, secret);
    }
    clean
}

fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    haystack
        .as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

#[cfg(test)]
mod tests {
    use hofund_plugin_api::PluginErrorKind;

    use super::*;

    fn result_error(status: Option<u16>, message: &str, kind: Option<&str>) -> ResultError {
        ResultError {
            message: message.to_owned(),
            api_error_status: status,
            kind: kind.map(str::to_owned),
        }
    }

    #[test]
    fn sign_in_lapse_keeps_a_400_status_and_does_not_cool() {
        let error = classify(&result_error(
            Some(400),
            "OAuth session expired and could not be refreshed",
            None,
        ));
        assert_eq!(error.status(), Some(400));
        assert_eq!(error.kind(), PluginErrorKind::Status);
        assert!(!error.cools());
        assert!(!error.skips_failover_same_plugin());
    }

    #[test]
    fn sign_in_lapse_without_a_status_becomes_502() {
        let error = classify(&result_error(
            None,
            "OAuth access token has been revoked",
            None,
        ));
        assert_eq!(error.status(), Some(502));
        assert!(!error.cools());
    }

    #[test]
    fn sign_in_lapse_does_not_force_401() {
        let error = classify(&result_error(Some(403), "OAuth token revoked", None));
        assert_eq!(error.status(), Some(403));
        assert!(!error.cools());
    }

    #[test]
    fn extra_usage_is_400_without_cooldown_and_avoids_quota_words() {
        let error = classify(&result_error(
            Some(400),
            "this organization's extra usage is not available",
            None,
        ));
        assert_eq!(error.status(), Some(400));
        assert!(!error.cools());
        assert_eq!(
            error.message(),
            "this organization's extra usage is not available"
        );
        let lower = error.message().to_ascii_lowercase();
        assert!(!lower.contains("quota"));
        assert!(!lower.contains("rate limit"));
        assert!(!lower.contains("exceeded"));
    }

    #[test]
    fn extra_usage_wording_is_replaced_when_it_names_a_quota() {
        let error = classify(&result_error(
            Some(400),
            "extra usage quota exceeded for this organization",
            None,
        ));
        assert_eq!(error.status(), Some(400));
        assert!(!error.cools());
        assert_eq!(error.message(), EXTRA_USAGE_SAID);
    }

    #[test]
    fn prompt_too_long_is_400_even_without_a_status() {
        let error = classify(&result_error(None, "Prompt is too long", None));
        assert_eq!(error.status(), Some(400));
        assert!(error.cools());
        assert_eq!(error.kind(), PluginErrorKind::Status);
    }

    #[test]
    fn a_rate_limit_result_stays_a_429_status() {
        let error = classify(&result_error(
            Some(429),
            "This request would exceed your account's rate limit. Please try again later.",
            Some("rate_limit"),
        ));
        assert_eq!(error.status(), Some(429));
        assert_eq!(error.kind(), PluginErrorKind::Status);
    }

    #[test]
    fn a_quota_shaped_result_is_quota_status() {
        let error = classify(&result_error(
            Some(429),
            "You exceeded your plan's usage limit",
            None,
        ));
        assert_eq!(error.status(), Some(429));
        assert_eq!(error.kind(), PluginErrorKind::Quota);
    }

    #[test]
    fn a_status_from_the_stream_is_kept() {
        let error = classify(&result_error(
            Some(418),
            "I'm a teapot",
            Some("server_error"),
        ));
        assert_eq!(error.status(), Some(418));
        assert_eq!(error.kind(), PluginErrorKind::Status);
        assert!(error.cools());
    }

    #[test]
    fn a_status_less_failure_is_502() {
        let error = classify(&result_error(None, "something went wrong", None));
        assert_eq!(error.status(), Some(502));
    }

    #[test]
    fn process_ended_redacts_stderr_secrets() {
        let error = process_ended(
            "token sk-ant-oat-secretvalue is bad\nmore",
            &["sk-ant-oat-secretvalue"],
        );
        assert_eq!(error.status(), Some(502));
        assert!(error.message().starts_with(ENDED));
        assert!(error.message().contains("[redacted]"));
        assert!(!error.message().contains("sk-ant-oat-secretvalue"));
    }

    #[test]
    fn a_timeout_uses_the_timeout_kind() {
        let error = timed_out("the Claude Code run did not finish in time");
        assert_eq!(error.kind(), PluginErrorKind::Timeout);
    }

    #[test]
    fn a_rotated_login_version_is_not_lapsed() {
        assert!(should_mark_lapsed(Some(1), Some(1)));
        assert!(!should_mark_lapsed(Some(1), Some(2)));
        assert!(!should_mark_lapsed(Some(1), None));
        assert!(!should_mark_lapsed(None, Some(1)));
    }

    #[test]
    fn is_sign_in_required_matches_the_listed_strings() {
        assert!(is_sign_in_required(
            "failed: OAuth session expired and could not be refreshed"
        ));
        assert!(!is_sign_in_required("plain server error"));
    }

    #[test]
    fn a_completed_result_is_a_finished_stream() {
        assert!(stream_finished(
            r#"{"type":"result","is_error":false,"result":"ok"}"#
        ));
        assert!(stream_finished(
            r#"{"type":"result","is_error":true,"result":"no"}"#
        ));
        assert!(!stream_finished(
            r#"{"type":"assistant","message":{"content":[]}}"#
        ));
    }

    #[test]
    fn a_non_error_status_from_the_stream_is_treated_as_502() {
        for status in [0, 200, 399] {
            let error = classify(&result_error(Some(status), "something went wrong", None));
            assert_eq!(error.status(), Some(502), "status {status}");
        }
    }

    #[test]
    fn third_party_wording_is_also_extra_usage() {
        for message in [
            "this account cannot use third-party billing",
            "this account cannot use third party billing",
        ] {
            let error = classify(&result_error(Some(400), message, None));
            assert_eq!(error.status(), Some(400), "{message}");
            assert!(!error.cools(), "{message}");
        }
    }

    #[test]
    fn extra_usage_replaces_each_quota_shaped_word() {
        for message in [
            "extra usage quota for this organization",
            "extra usage hit a rate limit for this organization",
            "extra usage hit a rate-limit for this organization",
            "extra usage usage limit for this organization",
            "extra usage exceeded for this organization",
            "extra usage too many requests for this organization",
        ] {
            let error = classify(&result_error(Some(400), message, None));
            assert_eq!(error.status(), Some(400), "{message}");
            assert!(!error.cools(), "{message}");
            assert_eq!(error.message(), EXTRA_USAGE_SAID, "{message}");
        }
    }

    #[test]
    fn each_too_long_phrase_is_400() {
        for message in [
            "prompt too long for this request",
            "context_length_exceeded for this request",
            "maximum context length for this request",
            "context window exceeds the allowed size",
        ] {
            let error = classify(&result_error(None, message, None));
            assert_eq!(error.status(), Some(400), "{message}");
            assert_eq!(error.kind(), PluginErrorKind::Status, "{message}");
        }
    }

    #[test]
    fn a_too_long_phrase_does_not_win_when_the_message_is_a_rate_limit() {
        for message in [
            "Prompt is too long because of the rate limit",
            "Prompt is too long because of the rate-limit",
            "Prompt is too long; try again later",
            "Prompt is too long per minute",
            "Prompt is too long per hour",
            "Prompt is too long: max_tokens",
            "Prompt is too long: max_output_tokens",
        ] {
            let error = classify(&result_error(None, message, None));
            assert_ne!(error.status(), Some(400), "{message}");
        }
    }
}
