//! Request headers the CLI backend expects.

use hofund_plugin_api::SecretString;
use hofund_plugin_sdk::oauth::secret_text;

/// Default client version when the installed CLI is not queried.
pub(crate) const CLIENT_VERSION: &str = "1.0.41";

/// Headers besides Authorization for an authenticated API call.
pub(crate) fn client(token: &SecretString) -> Vec<(String, String)> {
    let mut headers = client_only();
    headers.insert(
        0,
        (
            "authorization".to_owned(),
            format!("Bearer {}", secret_text(token)),
        ),
    );
    headers
}

/// Headers the CLI sends on each request, without Authorization.
pub(crate) fn client_only() -> Vec<(String, String)> {
    let version = CLIENT_VERSION;
    let os = if std::env::consts::OS == "macos" {
        "macos"
    } else if std::env::consts::OS == "windows" {
        "windows"
    } else {
        std::env::consts::OS
    };
    let arch = if std::env::consts::ARCH == "aarch64" {
        "aarch64"
    } else if std::env::consts::ARCH == "x86_64" {
        "x86_64"
    } else {
        std::env::consts::ARCH
    };
    vec![
        ("x-grok-client-version".to_owned(), version.to_owned()),
        ("x-xai-token-auth".to_owned(), "xai-grok-cli".to_owned()),
        (
            "x-grok-client-identifier".to_owned(),
            "grok-shell".to_owned(),
        ),
        ("x-grok-client-mode".to_owned(), "headless".to_owned()),
        (
            "user-agent".to_owned(),
            format!("grok-shell/{version} ({os}; {arch})"),
        ),
    ]
}

/// Loop-detection headers the CLI sends on Responses streams.
pub(crate) const LOOP_CHECKS: [(&str, &str); 2] = [
    ("x-grok-doom-loop-check", "1024"),
    ("x-grok-exact-repetition-check", "64"),
];
