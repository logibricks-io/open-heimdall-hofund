use std::sync::Arc;

use hofund_kit_upstream::wiremock::matchers::{method, path};
use hofund_kit_upstream::wiremock::{Mock, MockServer, ResponseTemplate};
use hofund_kit_upstream::{HttpLimits, TestHost, block_on, runtime_host};
use hofund_plugin_api::{
    BuildContext, CountOutcome, DynUpstreamFactory, HostContext, ListQuery, PluginId, ProbeQuery,
    ProtocolId, RawTable, RawValue, Request, SecretString, UpstreamProvider,
};
use hofund_plugin_sdk::json::json;
use hofund_plugin_sdk::sigv4::AwsSigner;
use hofund_plugin_sdk::{Call, wire};

use super::BedrockFactory;

const ID: &str = "AKIDEXAMPLE";
const SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
const TOKEN: &str = "FQoGZXIvYXdzEXAMPLETOKEN";
const CHAT_REPLY: &str = r#"{"id":"c1","object":"chat.completion","model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#;
const RESPONSES_REPLY: &str = r#"{"id":"r1","object":"response","status":"completed","model":"m","output":[{"type":"message","id":"o1","role":"assistant","content":[{"type":"output_text","text":"ok"}]}]}"#;
const MESSAGES_REPLY: &str = r#"{"id":"m1","type":"message","role":"assistant","model":"m","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#;
const CONVERSE_REPLY: &str = r#"{"output":{"message":{"role":"assistant","content":[{"text":"ok"}]}},"stopReason":"end_turn","usage":{"inputTokens":1,"outputTokens":1,"totalTokens":2}}"#;

fn raw(base: &str) -> RawTable {
    let mut raw = RawTable::new();
    for (key, value) in [
        ("base_url", base),
        ("access_key_id", ID),
        ("secret_access_key", SECRET),
        ("session_token", TOKEN),
    ] {
        raw.insert(key, RawValue::String(value.to_owned()));
    }
    raw
}

fn build(host: &HostContext, base: &str, protocol: ProtocolId) -> Arc<dyn UpstreamProvider> {
    DynUpstreamFactory::validate_and_build_with(
        &BedrockFactory,
        &raw(base),
        host,
        &BuildContext::new(protocol),
    )
    .expect("build")
}

fn request(model: &str) -> Request {
    let mut request: Request = hofund_plugin_sdk::json::from_value(json!({
        "model": model,
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
    }))
    .expect("request");
    request.stream = false;
    request
}

fn runtime(api_key: &str) -> (tempfile::TempDir, hofund_plugin_api::HostContext) {
    let root = tempfile::tempdir().expect("root");
    let host = runtime_host(
        PluginId::from_static("bedrock"),
        api_key,
        root.path(),
        HttpLimits::default(),
    )
    .expect("host");
    (root, host)
}

fn header<'a>(request: &'a hofund_kit_upstream::wiremock::Request, name: &str) -> &'a str {
    request
        .headers
        .iter()
        .find(|(key, _)| key.as_str() == name)
        .and_then(|(_, value)| std::str::from_utf8(value.as_bytes()).ok())
        .unwrap_or("")
}

fn assert_signed(request: &hofund_kit_upstream::wiremock::Request, service: &str) {
    let authorization = header(request, "authorization");
    assert!(
        authorization.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/"),
        "{authorization}"
    );
    assert!(
        authorization.contains(&format!("/{service}/aws4_request")),
        "{authorization}"
    );
    assert!(
        authorization.contains("SignedHeaders=host;x-amz-date;x-amz-security-token"),
        "{authorization}"
    );
    assert_eq!(header(request, "x-amz-security-token"), TOKEN);
    assert_eq!(header(request, "x-amz-date").len(), 16);
    assert_eq!(header(request, "x-api-key"), "");
}

async fn mount_post(server: &MockServer, route: &str, body: &'static str) {
    Mock::given(method("POST"))
        .and(path(route))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "application/json"))
        .mount(server)
        .await;
}

#[test]
fn d266_all_runtime_routes_sign_on_a_loopback_server() {
    block_on(async {
        let server = MockServer::start().await;
        mount_post(&server, "/openai/v1/chat/completions", CHAT_REPLY).await;
        mount_post(&server, "/openai/v1/responses", RESPONSES_REPLY).await;
        mount_post(&server, "/anthropic/v1/messages", MESSAGES_REPLY).await;
        mount_post(&server, "/model/demo/converse", CONVERSE_REPLY).await;
        let (_root, host) = runtime("");
        for (protocol, base, model) in [
            (
                ProtocolId::OPENAI_CHAT,
                format!("{}/openai/v1", server.uri()),
                "openai.gpt-oss-120b-1:0",
            ),
            (
                ProtocolId::OPENAI_RESPONSES,
                format!("{}/openai/v1", server.uri()),
                "openai.gpt-oss-120b-1:0",
            ),
            (
                ProtocolId::ANTHROPIC_MESSAGES,
                format!("{}/anthropic", server.uri()),
                "anthropic.claude",
            ),
            (ProtocolId::BEDROCK_CONVERSE, server.uri(), "demo"),
        ] {
            build(&host, &base, protocol)
                .execute(request(model), ProtocolId::OPENAI_CHAT)
                .await
                .expect("execute");
        }
        let received = server.received_requests().await.expect("requests");
        assert_eq!(received.len(), 4);
        for request in &received {
            assert_signed(request, "bedrock");
        }
    });
}

#[test]
fn d266_probes_and_message_count_sign_without_an_api_key() {
    let root = tempfile::tempdir().expect("root");
    let host = TestHost::open(PluginId::from_static("bedrock"), root.path());
    for (protocol, base, reply) in [
        (
            ProtocolId::OPENAI_CHAT,
            "https://proxy.test/openai/v1",
            "{}",
        ),
        (
            ProtocolId::OPENAI_RESPONSES,
            "https://proxy.test/openai/v1",
            "{}",
        ),
        (
            ProtocolId::ANTHROPIC_MESSAGES,
            "https://proxy.test/anthropic",
            "{}",
        ),
        (ProtocolId::BEDROCK_CONVERSE, "https://proxy.test", "{}"),
    ] {
        let provider = build(host.context(), base, protocol);
        host.http().enqueue(reply);
        block_on(provider.probe(&ProbeQuery::new(protocol, "demo").expect("probe")))
            .expect("probe reply");
    }
    let messages = build(
        host.context(),
        "https://proxy.test/anthropic",
        ProtocolId::ANTHROPIC_MESSAGES,
    );
    host.http().enqueue(r#"{"input_tokens":7}"#);
    let counted = block_on(messages.count_input(request("demo"), ProtocolId::ANTHROPIC_MESSAGES))
        .expect("count");
    assert!(!matches!(counted, CountOutcome::Unsupported));
    let calls = host.http().calls();
    assert_eq!(calls.len(), 5);
    for call in calls {
        let names = call.sensitive_headers();
        assert!(names.iter().any(|name| name == "authorization"));
        assert!(names.iter().any(|name| name == "x-amz-security-token"));
        assert!(!names.iter().any(|name| name == "x-api-key"));
    }
}

#[test]
fn d266_an_api_key_wins_on_compatibility_routes() {
    let root = tempfile::tempdir().expect("root");
    let host = TestHost::open(PluginId::from_static("bedrock"), root.path());
    host.secrets()
        .insert(&PluginId::from_static("bedrock"), "api_key", "ABSK-test");
    for (protocol, base, reply) in [
        (
            ProtocolId::OPENAI_CHAT,
            "https://proxy.test/openai/v1",
            CHAT_REPLY,
        ),
        (
            ProtocolId::ANTHROPIC_MESSAGES,
            "https://proxy.test/anthropic",
            MESSAGES_REPLY,
        ),
    ] {
        let provider = build(host.context(), base, protocol);
        host.http().enqueue(reply);
        block_on(provider.execute(request("demo"), protocol)).expect("execute");
    }
    let calls = host.http().calls();
    assert_eq!(calls.len(), 2);
    assert!(
        !calls[0]
            .sensitive_headers()
            .iter()
            .any(|name| name == "x-amz-security-token")
    );
    assert_eq!(calls[0].header("x-amz-date"), None);
    assert!(
        calls[1]
            .sensitive_headers()
            .iter()
            .any(|name| name == "x-api-key")
    );
    assert!(
        !calls[1]
            .sensitive_headers()
            .iter()
            .any(|name| name == "authorization")
    );
    assert_eq!(calls[1].header("x-amz-date"), None);
}

#[test]
fn d266_fixed_clock_matches_reference_signatures() {
    block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;
        let host = runtime("").1;
        let signer = AwsSigner::new("eu-west-1", ID, SecretString::from(SECRET.to_owned()))
            .session_token(SecretString::from(TOKEN.to_owned()))
            .fixed(1_795_552_519, "bedrock-runtime.eu-west-1.amazonaws.com");
        let body = r#"{"model":"openai.gpt-oss-120b-1:0","stream":true}"#;
        let call = wire::signed_call(
            Call::post(
                "https://bedrock-runtime.eu-west-1.amazonaws.com/openai/v1/chat/completions",
            )
            .json(body),
            &signer,
        )
        .expect("signed")
        .with_uri(format!("{}/openai/v1/chat/completions", server.uri()));
        call.open_raw(host.http(), 1).await.expect("sent");

        let invoke_body = r#"{"prompt":"hi"}"#;
        let invoke = AwsSigner::new("us-west-2", ID, SecretString::from(SECRET.to_owned()))
            .session_token(SecretString::from(TOKEN.to_owned()))
            .fixed(1_795_552_519, "bedrock-runtime.us-west-2.amazonaws.com");
        let call = wire::signed_call(
            Call::post("https://bedrock-runtime.us-west-2.amazonaws.com/model/anthropic.claude-v2%3A1/invoke-with-response-stream").json(invoke_body),
            &invoke,
        )
        .expect("signed")
        .with_uri(format!("{}/model/anthropic.claude-v2%3A1/invoke-with-response-stream", server.uri()));
        call.open_raw(host.http(), 1).await.expect("sent");

        let received = server.received_requests().await.expect("requests");
        assert_eq!(
            header(&received[0], "authorization"),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20261124/eu-west-1/bedrock/aws4_request, SignedHeaders=host;x-amz-date;x-amz-security-token, Signature=9e43ae46521e59b3521125dc5ed327d424d6cba8f6af769466dd4ff6da00e824"
        );
        assert_eq!(
            header(&received[1], "authorization"),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20261124/us-west-2/bedrock/aws4_request, SignedHeaders=host;x-amz-date;x-amz-security-token, Signature=2d4120d69f2a9f83f954ba680be35bf8a351420d792d65159e946e9297ee2e42"
        );
    });
}

#[test]
fn d266_scope_covers_fips_china_mantle_and_the_default() {
    use super::converse::{compatibility_region, host_scope};

    for (host, region, service) in [
        (
            "bedrock-runtime-fips.us-east-1.amazonaws.com",
            "us-east-1",
            "bedrock",
        ),
        (
            "bedrock-runtime.cn-north-1.amazonaws.com.cn",
            "cn-north-1",
            "bedrock",
        ),
        (
            "bedrock-mantle.ap-southeast-2.api.aws",
            "ap-southeast-2",
            "bedrock-mantle",
        ),
    ] {
        assert_eq!(
            host_scope(host),
            Some((region.to_owned(), service)),
            "{host}"
        );
    }
    assert_eq!(
        host_scope("bedrock-runtime.eu-west-1.amazonaws.com.evil.test"),
        None
    );
    assert_eq!(
        compatibility_region("", "https://proxy.test/base").expect("default"),
        "us-east-1"
    );
}

#[test]
fn d266_static_model_list_needs_no_credentials_or_network() {
    let root = tempfile::tempdir().expect("root");
    let host = TestHost::open(PluginId::from_static("bedrock"), root.path());
    let provider = build(
        host.context(),
        "https://proxy.test/openai/v1",
        ProtocolId::OPENAI_CHAT,
    );
    let list = block_on(provider.list_models(&ListQuery::new())).expect("list");
    assert!(!list.models().is_empty());
    assert!(host.http().calls().is_empty());
}

#[test]
fn d266_probe_errors_redact_the_access_secret_and_session_token() {
    let root = tempfile::tempdir().expect("root");
    let host = TestHost::open(PluginId::from_static("bedrock"), root.path());
    let provider = build(
        host.context(),
        "https://proxy.test/openai/v1",
        ProtocolId::OPENAI_CHAT,
    );
    host.http().enqueue_response(
        401,
        &[],
        format!(r#"{{"error":{{"message":"{SECRET} {TOKEN}"}}}}"#),
    );
    let report =
        block_on(provider.probe(&ProbeQuery::new(ProtocolId::OPENAI_CHAT, "demo").expect("probe")));
    let text = match report {
        Ok(report) => format!("{report:?}"),
        Err(error) => error.to_string(),
    };
    assert!(!text.contains(SECRET));
    assert!(!text.contains(TOKEN));
}
