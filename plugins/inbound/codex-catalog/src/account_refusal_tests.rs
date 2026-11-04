//! Direct tests for ChatGPT account refusal detection on the Codex relay.

use hofund_plugin_sdk::HeaderMap;

use super::account_refusal::codex_account_refusal;

#[test]
fn d68_account_refusal_401_with_quota_shape_is_not_account_refusal() {
    let body = br#"{"error":{"code":"usage_limit_reached","message":"used up"}}"#;
    assert!(!codex_account_refusal(401, &HeaderMap::new(), body, false));
}

#[test]
fn d68_account_refusal_403_usage_not_included_counts() {
    let body = br#"{"error":{"message":"usage_not_included for this plan"}}"#;
    assert!(codex_account_refusal(403, &HeaderMap::new(), body, false));
}

#[test]
fn d68_account_refusal_stream_usage_limit_counts() {
    let body = concat!(
        "data: ",
        r#"{"type":"response.failed","response":{"error":{"code":"usage_limit_reached"}}}"#,
        "\n\n"
    );
    assert!(codex_account_refusal(
        200,
        &HeaderMap::new(),
        body.as_bytes(),
        true
    ));
}
