//! Account-off marking on the Messages transport production uses.
//!
//! `anthropic()` is a direct SDK target. These tests script that HTTP handle,
//! so a 400 or 403 from `api.anthropic.com` is restricted and the same body
//! from a relay is an ordinary failure.

use std::sync::Arc;

use hofund_kit_upstream::TestHost;
use hofund_plugin_api::{PluginId, ProtocolId, Request, UpstreamProvider};
use hofund_plugin_registry::anthropic;
use hofund_provider::Endpoint;

const ORG: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"This organization has been disabled."}}"#;
const REPLIES: &str = r#"{"type":"error","error":{"type":"permission_error","message":"Your account can't receive replies from Claude."}}"#;
const PLAIN: &str =
    r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad request"}}"#;
const EVENT: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"demo\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
    "event: error\n",
    "data: {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"This organization has been disabled.\"}}\n\n",
);

fn request(stream: bool) -> Request {
    serde_json::from_value(serde_json::json!({
        "model": "demo",
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
        "params": {"max_tokens": 16},
        "stream": stream,
    }))
    .expect("request")
}

fn messages(base: &str, host: &TestHost) -> Arc<dyn UpstreamProvider> {
    let mut endpoint = Endpoint::new("d234", base);
    endpoint.api_key = "test-key".to_owned();
    anthropic(endpoint, host.context()).expect("provider")
}

fn scripted() -> (tempfile::TempDir, TestHost) {
    let root = tempfile::tempdir().expect("root");
    let host = TestHost::open(PluginId::from_static("anthropic"), root.path());
    (root, host)
}

#[tokio::test]
async fn d234_official_host_400_and_403_are_restricted() {
    for (status, body) in [(400_u16, ORG), (403, REPLIES)] {
        let (_root, host) = scripted();
        host.http().enqueue_response(
            status,
            &[("content-type", "application/json")],
            body.as_bytes(),
        );
        let provider = messages("https://api.anthropic.com", &host);
        let Err(error) = provider
            .execute(request(false), ProtocolId::ANTHROPIC_MESSAGES)
            .await
        else {
            panic!("{status} should fail");
        };
        assert!(error.is_account_restricted(), "{status}");
        assert!(!error.is_ordinary_failure(), "{status}");
        assert!(!error.skips_failover_same_plugin(), "{status}");
        assert_eq!(error.status(), Some(status));
        let uri = host.http().calls()[0].uri().to_owned();
        assert!(
            uri.starts_with("https://api.anthropic.com/"),
            "{status} {uri}"
        );
    }
}

#[tokio::test]
async fn d234_relay_host_400_and_403_are_ordinary_failures() {
    for status in [400_u16, 403] {
        let (_root, host) = scripted();
        host.http().enqueue_response(
            status,
            &[("content-type", "application/json")],
            ORG.as_bytes(),
        );
        let provider = messages("https://relay.example.com/v1", &host);
        let Err(error) = provider
            .execute(request(false), ProtocolId::ANTHROPIC_MESSAGES)
            .await
        else {
            panic!("{status} should fail");
        };
        assert!(!error.is_account_restricted(), "{status}");
        assert!(error.is_ordinary_failure(), "{status}");
        assert_eq!(error.status(), Some(status));
        let uri = host.http().calls()[0].uri().to_owned();
        assert!(
            uri.starts_with("https://relay.example.com/"),
            "{status} {uri}"
        );
    }
}

#[tokio::test]
async fn d234_plain_400_and_official_500_stay_unmarked() {
    let (_root, host) = scripted();
    host.http().enqueue_response(
        400,
        &[("content-type", "application/json")],
        PLAIN.as_bytes(),
    );
    let provider = messages("https://api.anthropic.com", &host);
    let Err(error) = provider
        .execute(request(false), ProtocolId::ANTHROPIC_MESSAGES)
        .await
    else {
        panic!("plain 400 should fail");
    };
    assert!(!error.is_account_restricted());
    assert!(!error.is_ordinary_failure());
    assert_eq!(error.status(), Some(400));

    let (_root, host) = scripted();
    host.http()
        .enqueue_response(500, &[("content-type", "application/json")], ORG.as_bytes());
    let provider = messages("https://api.anthropic.com", &host);
    let Err(error) = provider
        .execute(request(false), ProtocolId::ANTHROPIC_MESSAGES)
        .await
    else {
        panic!("500 should fail");
    };
    assert!(!error.is_account_restricted());
    assert!(!error.is_ordinary_failure());
    assert_eq!(error.status(), Some(500));
}

#[tokio::test]
async fn d234_count_uses_the_same_host_mark() {
    let (_root, host) = scripted();
    host.http().enqueue_response(
        403,
        &[("content-type", "application/json")],
        REPLIES.as_bytes(),
    );
    let provider = messages("https://api.anthropic.com/v1", &host);
    let Err(error) = provider
        .count_input(request(false), ProtocolId::ANTHROPIC_MESSAGES)
        .await
    else {
        panic!("count should fail");
    };
    assert!(error.is_account_restricted());
    assert!(
        host.http().calls()[0]
            .uri()
            .starts_with("https://api.anthropic.com/")
    );

    let (_root, host) = scripted();
    host.http()
        .enqueue_response(400, &[("content-type", "application/json")], ORG.as_bytes());
    let provider = messages("https://relay.example.com", &host);
    let Err(error) = provider
        .count_input(request(false), ProtocolId::ANTHROPIC_MESSAGES)
        .await
    else {
        panic!("relay count should fail");
    };
    assert!(error.is_ordinary_failure());
    assert!(!error.is_account_restricted());
}

#[tokio::test]
async fn d234_in_band_error_event_before_content_follows_the_host() {
    let (_root, host) = scripted();
    host.http().enqueue_response(
        200,
        &[("content-type", "text/event-stream")],
        EVENT.as_bytes(),
    );
    let provider = messages("https://api.anthropic.com", &host);
    let Err(error) = provider
        .execute(request(true), ProtocolId::ANTHROPIC_MESSAGES)
        .await
    else {
        panic!("the error event should fail the call");
    };
    assert!(error.is_account_restricted());
    assert!(!error.is_ordinary_failure());
    // `invalid_request_error` is read as 400. The reply itself was 200.
    assert_eq!(error.status(), Some(400));

    let (_root, host) = scripted();
    host.http().enqueue_response(
        200,
        &[("content-type", "text/event-stream")],
        EVENT.as_bytes(),
    );
    let provider = messages("https://relay.example.com", &host);
    let Err(error) = provider
        .execute(request(true), ProtocolId::ANTHROPIC_MESSAGES)
        .await
    else {
        panic!("the relay error event should fail the call");
    };
    assert!(error.is_ordinary_failure());
    assert!(!error.is_account_restricted());
}

#[tokio::test]
async fn d234_unary_200_error_body_follows_the_host() {
    for (base, restricted) in [
        ("https://api.anthropic.com", true),
        ("https://relay.example.com", false),
    ] {
        let (_root, host) = scripted();
        host.http()
            .enqueue_response(200, &[("content-type", "application/json")], ORG.as_bytes());
        let provider = messages(base, &host);
        let Err(error) = provider
            .execute(request(false), ProtocolId::ANTHROPIC_MESSAGES)
            .await
        else {
            panic!("{base} should fail");
        };
        assert_eq!(error.is_account_restricted(), restricted, "{base}");
        assert_eq!(error.is_ordinary_failure(), !restricted, "{base}");
        assert_eq!(error.status(), Some(400), "{base}");
        assert!(host.http().calls()[0].uri().starts_with(base), "{base}");
    }
}
