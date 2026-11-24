//! Account-rest flags for a Google sign-in.
//!
//! The router does not name a vendor. A Google sign-in plugin calls
//! [`note_google_sign_in`] so a pool quota rests the requested model and an
//! account-verification refusal rests the account. Other vendors leave the
//! error unchanged.

use hofund_plugin_api::{PluginError, PluginErrorKind};

/// Marks a model-pool quota and an account-verification refusal.
///
/// Call this only for a Google sign-in (Antigravity, Gemini, or the agy CLI).
/// The same words from another vendor must not come through here: they rest
/// the whole account.
#[must_use]
pub fn note_google_sign_in(error: PluginError) -> PluginError {
    let error = pool_quota(error);
    note_verification(error)
}

/// A model-pool sentence on a quota error, or on a 429, rests that model as quota.
fn pool_quota(error: PluginError) -> PluginError {
    let seen =
        matches!(error.kind(), PluginErrorKind::Quota) || matches!(error.status(), Some(429));
    if !seen || !is_model_pool_quota(error.message()) {
        return error;
    }
    if error.kind() == PluginErrorKind::Quota {
        return error.with_model_scope();
    }
    let status = error.status().unwrap_or(429);
    let mut quota = PluginError::quota_status(status, error.message()).with_model_scope();
    if let Some(after) = error.retry_after() {
        quota = quota.with_retry_after(after);
    }
    if let Some(after) = error.reset_after() {
        quota = quota.with_reset_after(after);
    }
    if !error.cools() {
        quota = quota.without_cooldown();
    }
    quota
}

/// Whether `message` is a model-pool quota rather than an account quota.
#[must_use]
pub fn is_model_pool_quota(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("individual quota reached")
        || lower.contains("third-party quota reached")
        || lower.contains("exhausted your capacity on this model")
}

/// Whether `message` asks for account verification.
#[must_use]
pub fn asks_for_verification(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("verification required")
        || lower.contains("validation_required")
        || lower.contains("needs to be verified")
        || lower.contains("verify your account")
        || lower.contains("account verification required")
}

fn note_verification(error: PluginError) -> PluginError {
    if !matches!(error.status(), Some(401 | 403)) || !asks_for_verification(error.message()) {
        return error;
    }
    let message = ensure_advice(error.message());
    error.with_message(message).with_verify_rest()
}

fn ensure_advice(message: &str) -> String {
    if message
        .to_ascii_lowercase()
        .contains("needs to be verified")
    {
        return message.to_owned();
    }
    let tail = match first_https(message) {
        Some(link) => format!(
            "this Google account needs to be verified: open <{link}> in a browser signed in to it, verify it, then try again"
        ),
        None => "this Google account needs to be verified: open the Antigravity app, or run `agy` in a terminal, signed in to it and do what it asks, then try again".to_owned(),
    };
    format!("{message} — {tail}")
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

#[cfg(test)]
mod tests {
    use super::{asks_for_verification, is_model_pool_quota, note_google_sign_in};
    use hofund_plugin_api::{PluginError, PluginErrorKind};

    #[test]
    fn d222_pool_words_scope_the_model_and_other_words_do_not() {
        let pooled = note_google_sign_in(PluginError::quota_status(
            429,
            "Individual quota reached. Resets in 1h.",
        ));
        assert!(pooled.scopes_model());
        assert!(!pooled.verifies_account());
        let capacity = note_google_sign_in(PluginError::quota_status(
            429,
            "You have exhausted your capacity on this model.",
        ));
        assert!(capacity.scopes_model());
        let account = note_google_sign_in(PluginError::quota_status(
            429,
            "Quota exceeded. Resets in 1h.",
        ));
        assert!(!account.scopes_model());
        assert!(is_model_pool_quota("INDIVIDUAL QUOTA REACHED"));
        assert!(!is_model_pool_quota("Quota exceeded"));
        let promoted = note_google_sign_in(PluginError::status_error(
            429,
            "You have exhausted your capacity on this model.",
        ));
        assert_eq!(promoted.kind(), PluginErrorKind::Quota);
        assert!(promoted.scopes_model());
        let rate = note_google_sign_in(PluginError::status_error(429, "Too many requests"));
        assert_eq!(rate.kind(), PluginErrorKind::Status);
        assert!(!rate.scopes_model());
    }

    #[test]
    fn d222_verification_advice_is_appended_once_and_keeps_the_link() {
        let once = note_google_sign_in(PluginError::status_error(
            403,
            "Verification Required https://accounts.example/verify",
        ));
        assert!(once.verifies_account());
        assert_eq!(once.message().matches("needs to be verified").count(), 1);
        assert!(once.message().contains("https://accounts.example/verify"));
        let twice = note_google_sign_in(once);
        assert_eq!(twice.message().matches("needs to be verified").count(), 1);
        let permission = note_google_sign_in(PluginError::status_error(
            403,
            "The caller does not have permission",
        ));
        assert!(!permission.verifies_account());
        assert!(!asks_for_verification(
            "The caller does not have permission"
        ));
    }

    #[test]
    fn d260_third_party_quota_scopes_the_model_until_the_written_reset() {
        let notice = "Third-party quota reached. Resets in 3h22m19s.";
        assert!(is_model_pool_quota(notice));
        assert!(is_model_pool_quota(&notice.to_ascii_uppercase()));
        assert!(!is_model_pool_quota("Quota exceeded. Resets in 3h22m19s."));
        let pooled = note_google_sign_in(PluginError::quota_status(429, notice));
        assert!(pooled.scopes_model());
        assert_eq!(pooled.kind(), PluginErrorKind::Quota);
        assert!(!pooled.verifies_account());
        let promoted = note_google_sign_in(PluginError::status_error(429, notice));
        assert_eq!(promoted.kind(), PluginErrorKind::Quota);
        assert!(promoted.scopes_model());
        assert_eq!(promoted.status(), Some(429));
        assert_eq!(
            crate::transport::retry_after_from_body(notice.as_bytes()),
            Some(std::time::Duration::from_secs(12_139))
        );
    }
}
