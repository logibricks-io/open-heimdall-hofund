//! The SDK Chat and Responses transport against the recording host.
//!
//! The SDK cannot depend on this crate, so its `TestHost` tests live here.

use hofund_plugin_api::{
    Completion, CredentialStyle, PluginError, PluginErrorKind, PluginId, ProtocolId, Request,
    SecretString,
};
use hofund_plugin_host::TestHost;
use hofund_plugin_sdk::wire::{self, Target};

const CHAT_REPLY: &str = r#"{"id":"c1","object":"chat.completion","model":"demo","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}]}"#;

fn host() -> (tempfile::TempDir, TestHost) {
    let root = tempfile::tempdir().expect("root");
    let host = TestHost::open(
        PluginId::from_static("wire-test"),
        root.path().to_path_buf(),
    );
    (root, host)
}

fn secret() -> SecretString {
    SecretString::from("test-key".to_owned())
}

async fn run(
    host: &TestHost,
    target: &Target,
    secret: Option<&SecretString>,
    inbound: ProtocolId,
) -> Result<Completion, PluginError> {
    wire::execute(
        host.context().http(),
        target,
        secret,
        Request::new("demo"),
        inbound,
    )
    .await
}

async fn raw_bytes(completion: Completion) -> Vec<u8> {
    let Completion::Raw(mut body) = completion else {
        panic!("expected raw bytes");
    };
    let mut bytes = Vec::new();
    while let Some(chunk) = body.next_chunk().await {
        bytes.extend_from_slice(&chunk.expect("chunk"));
    }
    bytes
}

#[tokio::test]
async fn chat_posts_json_to_chat_completions_under_the_base() {
    let (_root, host) = host();
    host.http().enqueue(CHAT_REPLY);
    let target = Target::new(ProtocolId::OPENAI_CHAT, "http://up.test/v1/");
    let completion = run(&host, &target, Some(&secret()), ProtocolId::OPENAI_CHAT)
        .await
        .expect("execute");
    assert_eq!(raw_bytes(completion).await, CHAT_REPLY.as_bytes());
    let calls = host.http().calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].method(), "POST");
    assert_eq!(calls[0].uri(), "http://up.test/v1/chat/completions");
    let body: serde_json::Value = serde_json::from_slice(calls[0].body()).expect("json body");
    assert_eq!(body["model"], "demo");
}

#[tokio::test]
async fn responses_posts_to_responses_under_the_base() {
    let (_root, host) = host();
    host.http().enqueue("{}");
    let target = Target::new(ProtocolId::OPENAI_RESPONSES, "http://up.test/v1");
    run(&host, &target, None, ProtocolId::OPENAI_RESPONSES)
        .await
        .expect("execute");
    let calls = host.http().calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].uri(), "http://up.test/v1/responses");
    assert!(
        calls[0].sensitive_headers().is_empty(),
        "{:?}",
        calls[0].sensitive_headers()
    );
}

#[tokio::test]
async fn an_api_key_credential_is_a_sensitive_api_key_header_without_authorization() {
    let (_root, host) = host();
    host.http().enqueue(CHAT_REPLY);
    let target = Target::new(ProtocolId::OPENAI_CHAT, "http://up.test/v1")
        .credential(CredentialStyle::ApiKey);
    run(&host, &target, Some(&secret()), ProtocolId::OPENAI_CHAT)
        .await
        .expect("execute");
    let calls = host.http().calls();
    assert_eq!(calls[0].sensitive_headers(), ["api-key"]);
}

#[tokio::test]
async fn the_default_credential_is_a_sensitive_bearer_token() {
    let (_root, host) = host();
    host.http().enqueue(CHAT_REPLY);
    let target = Target::new(ProtocolId::OPENAI_CHAT, "http://up.test/v1");
    run(&host, &target, Some(&secret()), ProtocolId::OPENAI_CHAT)
        .await
        .expect("execute");
    assert_eq!(
        host.http().calls()[0].sensitive_headers(),
        ["authorization"]
    );
}

#[tokio::test]
async fn a_user_header_is_sensitive_and_replaces_the_credential_of_the_same_name() {
    let (_root, host) = host();
    host.http().enqueue(CHAT_REPLY);
    let target = Target::new(ProtocolId::OPENAI_CHAT, "http://up.test/v1").headers(vec![
        ("Authorization".to_owned(), "Custom token".to_owned()),
        ("x-tenant".to_owned(), "blue".to_owned()),
    ]);
    run(&host, &target, Some(&secret()), ProtocolId::OPENAI_CHAT)
        .await
        .expect("execute");
    assert_eq!(
        host.http().calls()[0].sensitive_headers(),
        ["authorization", "x-tenant"]
    );
}

#[tokio::test]
async fn another_protocol_is_unsupported_and_sends_nothing() {
    let (_root, host) = host();
    let target = Target::new(ProtocolId::ANTHROPIC_MESSAGES, "http://up.test");
    let error = run(&host, &target, None, ProtocolId::ANTHROPIC_MESSAGES)
        .await
        .err()
        .expect("unsupported");
    assert_eq!(error.kind(), PluginErrorKind::Unsupported);
    assert!(host.http().calls().is_empty());
}

#[tokio::test]
async fn a_connect_failure_is_retried_once() {
    let (_root, host) = host();
    host.http().enqueue_connect("refused");
    host.http().enqueue(CHAT_REPLY);
    let target = Target::new(ProtocolId::OPENAI_CHAT, "http://up.test/v1");
    run(&host, &target, None, ProtocolId::OPENAI_CHAT)
        .await
        .expect("second attempt");
    assert_eq!(host.http().calls().len(), 2);

    let (_root, host) = self::host();
    host.http().enqueue_connect("refused");
    host.http().enqueue_connect("refused");
    host.http().enqueue(CHAT_REPLY);
    let error = run(&host, &target, None, ProtocolId::OPENAI_CHAT)
        .await
        .err()
        .expect("two attempts only");
    assert_eq!(error.kind(), PluginErrorKind::Connect);
    assert_eq!(host.http().calls().len(), 2);
}

#[tokio::test]
async fn a_timed_out_post_is_never_replayed() {
    let (_root, host) = host();
    host.http().enqueue_timeout("slow");
    host.http().enqueue(CHAT_REPLY);
    let target = Target::new(ProtocolId::OPENAI_CHAT, "http://up.test/v1");
    let error = run(&host, &target, None, ProtocolId::OPENAI_CHAT)
        .await
        .err()
        .expect("timeout");
    assert_eq!(error.kind(), PluginErrorKind::Timeout);
    assert_eq!(host.http().calls().len(), 1);
}

#[tokio::test]
async fn a_cross_protocol_unary_reply_is_decoded() {
    let (_root, host) = host();
    host.http().enqueue(CHAT_REPLY);
    let target = Target::new(ProtocolId::OPENAI_CHAT, "http://up.test/v1");
    let completion = run(&host, &target, None, ProtocolId::ANTHROPIC_MESSAGES)
        .await
        .expect("execute");
    let Completion::Unary(response) = completion else {
        panic!("expected a decoded response");
    };
    assert_eq!(response.model.as_deref(), Some("demo"));
}
