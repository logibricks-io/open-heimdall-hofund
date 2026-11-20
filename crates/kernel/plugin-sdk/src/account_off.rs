//! Sentences an account uses when its vendor has turned it off.
//!
//! The subscription bridge and the Messages transport share this list. The
//! Messages transport also knows the configured host: `api.anthropic.com` is
//! an account restriction, and any other host is an ordinary failure so the
//! group still moves on. The router never matches these sentences.

use hofund_plugin_api::{PluginError, PluginErrorKind};

/// Official Messages host. A relay that speaks the same protocol is not this.
const OFFICIAL_MESSAGES_HOST: &str = "api.anthropic.com";

/// Whether `message` is one of the turned-off-account sentences.
///
/// Either apostrophe, or "cannot". Case is ignored. A literal `\u2019` escape
/// is folded the same way as the character.
#[must_use]
pub fn account_off_words(message: &str) -> bool {
    let folded = fold_account_off(message);
    if folded.contains("this organization has been disabled")
        || folded.contains("organization has disabled claude subscription access")
        || folded.contains("oauth authentication is currently not allowed for this organization")
    {
        return true;
    }
    let cannot = folded.replace("can't", "cannot");
    cannot.contains("account is on hold and cannot use claude")
        || cannot.contains("account cannot receive replies from claude")
        || cannot.contains("account cannot receive responses from claude")
}

/// Marks a Messages failure the vendor turned the account off for.
///
/// `stream_event` is set when the failure arrived inside a successful reply
/// as an error event, so the status was inferred. A real HTTP status of 500
/// or higher stays unmarked. An official host rests as restricted. Any other
/// host fails over as an ordinary failure. Quota, credit, and errors that do
/// not cool are left as they are.
#[must_use]
pub fn messages_account_off(base_url: &str, error: PluginError, stream_event: bool) -> PluginError {
    if !error.cools() || error.is_credit() || error.kind() == PluginErrorKind::Quota {
        return error;
    }
    if !account_off_words(error.message()) {
        return error;
    }
    let Some(status) = error.status() else {
        return error;
    };
    let client_error = (400..500).contains(&status);
    if !client_error && !stream_event {
        return error;
    }
    if official_messages_host(base_url) {
        error.account_restricted()
    } else {
        error.ordinary_failure()
    }
}

fn official_messages_host(base_url: &str) -> bool {
    host_of(base_url).is_some_and(|host| host.eq_ignore_ascii_case(OFFICIAL_MESSAGES_HOST))
}

fn host_of(base_url: &str) -> Option<&str> {
    let rest = base_url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let hostport = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = if let Some(inner) = hostport.strip_prefix('[') {
        inner.split(']').next()?
    } else {
        hostport.split(':').next()?
    };
    if host.is_empty() { None } else { Some(host) }
}

fn fold_account_off(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    for ch in message.chars() {
        let ch = if ch == '\u{2019}' { '\'' } else { ch };
        out.extend(ch.to_lowercase());
    }
    out.replace("\\u{2019}", "'").replace("\\u2019", "'")
}

#[cfg(test)]
mod tests {
    use super::{account_off_words, messages_account_off, official_messages_host};
    use hofund_plugin_api::PluginError;

    fn variants(message: &str) -> Vec<String> {
        let mut out = vec![message.to_owned(), message.to_ascii_uppercase()];
        if message.contains("can't") {
            out.push(message.replace("can't", "can\u{2019}t"));
            out.push(message.replace("can't", "cannot"));
            out.push(message.replace("can't", "can\\u2019t"));
        }
        out
    }

    #[test]
    fn d234_each_phrase_matches_either_apostrophe() {
        let phrases = [
            "This organization has been disabled.",
            "Your account is on hold and can't use Claude Code.",
            "Your organization has disabled Claude subscription access",
            "OAuth authentication is currently not allowed for this organization.",
            "Your account can't receive replies from Claude.",
            "Your account can't receive responses from Claude.",
        ];
        for phrase in phrases {
            for message in variants(phrase) {
                assert!(account_off_words(&message), "{message}");
            }
        }
        for message in [
            "Third-party apps now draw from your extra usage, not your plan limits.",
            "unable to respond to this request, which appears to violate our Usage Policy",
            "plain failure",
        ] {
            assert!(!account_off_words(message), "{message}");
        }
    }

    #[test]
    fn d234_official_host_edges() {
        for base in [
            "https://api.anthropic.com",
            "https://api.anthropic.com/v1",
            "https://api.anthropic.com:443",
            "https://user:secret@api.anthropic.com/v1",
            "https://API.ANTHROPIC.COM",
        ] {
            assert!(official_messages_host(base), "{base}");
        }
        for base in [
            "https://relay.example.com",
            "http://127.0.0.1:9",
            "https://api.anthropic.com.evil.example",
            "https://not-api.anthropic.com",
            "not a url",
            "",
        ] {
            assert!(!official_messages_host(base), "{base}");
        }
    }

    #[test]
    fn d234_http_and_stream_marks_follow_the_host() {
        let message = "This organization has been disabled.";
        for status in [400_u16, 403] {
            let official = messages_account_off(
                "https://api.anthropic.com/v1",
                PluginError::status_error(status, message),
                false,
            );
            assert!(official.is_account_restricted(), "{status}");
            assert!(!official.is_ordinary_failure(), "{status}");
            assert_eq!(official.status(), Some(status));

            let relay = messages_account_off(
                "https://relay.example.com",
                PluginError::status_error(status, message),
                false,
            );
            assert!(!relay.is_account_restricted(), "{status}");
            assert!(relay.is_ordinary_failure(), "{status}");
        }
        let server = messages_account_off(
            "https://api.anthropic.com",
            PluginError::status_error(500, message),
            false,
        );
        assert!(!server.is_account_restricted());
        assert!(!server.is_ordinary_failure());

        let event = messages_account_off(
            "https://api.anthropic.com",
            PluginError::status_error(502, message),
            true,
        );
        assert!(event.is_account_restricted());
        assert_eq!(event.status(), Some(502));
        let relay_event = messages_account_off(
            "https://relay.example.com/v1",
            PluginError::status_error(502, message),
            true,
        );
        assert!(relay_event.is_ordinary_failure());
        assert!(!relay_event.is_account_restricted());
    }

    #[test]
    fn d234_quota_credit_and_a_quiet_error_stay_unmarked() {
        let message = "This organization has been disabled.";
        let quota = messages_account_off(
            "https://api.anthropic.com",
            PluginError::quota_status(429, message),
            false,
        );
        assert_eq!(quota.kind(), hofund_plugin_api::PluginErrorKind::Quota);
        assert!(!quota.is_account_restricted());
        let credit = messages_account_off(
            "https://api.anthropic.com",
            PluginError::credit_status(402, message),
            true,
        );
        assert!(credit.is_credit());
        assert!(!credit.is_account_restricted());
        let quiet = messages_account_off(
            "https://api.anthropic.com",
            PluginError::status_error(400, message).without_cooldown(),
            false,
        );
        assert!(!quiet.cools());
        assert!(!quiet.is_account_restricted());
        assert!(!quiet.is_ordinary_failure());
    }
}
