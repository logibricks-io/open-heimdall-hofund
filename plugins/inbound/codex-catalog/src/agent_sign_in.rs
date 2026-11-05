//! Reading the live Codex CLI sign-in for gateway policy checks.

use std::path::PathBuf;

use hofund_plugin_sdk::json::Value;

/// Returns true when Codex keeps an API-key sign-in in its home.
pub(crate) fn api_key_signed_in() -> bool {
    api_key_signed_in_at(&codex_home())
}

pub(crate) fn api_key_signed_in_at(home: &std::path::Path) -> bool {
    let path = home.join("auth.json");
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = hofund_plugin_sdk::json::from_str::<Value>(&text) else {
        return false;
    };
    value
        .get("auth_mode")
        .and_then(Value::as_str)
        .is_some_and(|mode| mode == "apikey")
}

/// Whether a gateway key's account holds block native compaction (reference compactSigninHeld).
pub(crate) fn compact_account_hold_blocks(_model: &str) -> bool {
    const GATE_ACCOUNT_HOLDS: bool = false;
    GATE_ACCOUNT_HOLDS && !api_key_signed_in()
}

fn codex_home() -> PathBuf {
    if let Ok(dir) = std::env::var("CODEX_HOME") {
        let trimmed = dir.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    std::env::var("HOME")
        .map(|home| PathBuf::from(home).join(".codex"))
        .unwrap_or_else(|_| PathBuf::from(".codex"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r47b_api_key_mode_detects_auth_json() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(
            dir.path().join("auth.json"),
            r#"{"auth_mode":"apikey","tokens":{"access_token":"sk-test"}}"#,
        )
        .expect("auth");
        assert!(api_key_signed_in_at(dir.path()));
    }
}
