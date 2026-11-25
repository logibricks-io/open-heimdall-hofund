use std::sync::Arc;

use hofund_kit_upstream::wiremock::matchers::{method, path};
use hofund_kit_upstream::wiremock::{Mock, MockServer, ResponseTemplate};
use hofund_kit_upstream::{HttpLimits, TestHost, block_on, runtime_host};
use hofund_plugin_api::{
    BuildContext, DynUpstreamFactory, HostContext, ListQuery, PluginId, ProtocolId, RawTable,
    RawValue, Request, SecretString, UpstreamFactory, UpstreamProvider,
};
use hofund_plugin_sdk::json::json;
use hofund_plugin_sdk::sigv4::AwsSigner;
use hofund_plugin_sdk::{Call, wire};

use super::MantleFactory;

const ID: &str = "AKIDEXAMPLE";
const SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
const TOKEN: &str = "FQoGZXIvYXdzEXAMPLETOKEN";
const CHAT_REPLY: &str = r#"{"id":"c1","object":"chat.completion","model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#;
const RESPONSES_REPLY: &str = r#"{"id":"r1","object":"response","status":"completed","model":"m","output":[{"type":"message","id":"o1","role":"assistant","content":[{"type":"output_text","text":"ok"}]}]}"#;
const MESSAGES_REPLY: &str = r#"{"id":"m1","type":"message","role":"assistant","model":"m","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#;

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
        &MantleFactory,
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
        PluginId::from_static("bedrock-mantle"),
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

fn assert_signed(request: &hofund_kit_upstream::wiremock::Request) {
    let authorization = header(request, "authorization");
    assert!(
        authorization.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/"),
        "{authorization}"
    );
    assert!(
        authorization.contains("/bedrock-mantle/aws4_request"),
        "{authorization}"
    );
    assert!(
        authorization.contains("SignedHeaders=host;x-amz-date;x-amz-security-token"),
        "{authorization}"
    );
    assert!(header(request, "x-amz-security-token") == TOKEN);
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
fn d266_chat_responses_messages_and_model_list_sign_on_loopback() {
    block_on(async {
        let server = MockServer::start().await;
        mount_post(&server, "/v1/chat/completions", CHAT_REPLY).await;
        mount_post(&server, "/openai/v1/responses", RESPONSES_REPLY).await;
        mount_post(&server, "/anthropic/v1/messages", MESSAGES_REPLY).await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"data":[{"id":"openai.gpt-oss-120b"}]}"#),
            )
            .mount(&server)
            .await;
        let (_root, host) = runtime("");
        let chat = build(
            &host,
            &format!("{}/v1", server.uri()),
            ProtocolId::OPENAI_CHAT,
        );
        chat.execute(request("openai.gpt-oss-120b"), ProtocolId::OPENAI_CHAT)
            .await
            .expect("chat");
        chat.list_models(&ListQuery::new()).await.expect("list");
        for (protocol, base, model) in [
            (
                ProtocolId::OPENAI_RESPONSES,
                format!("{}/v1", server.uri()),
                "openai.gpt-5.5",
            ),
            (
                ProtocolId::ANTHROPIC_MESSAGES,
                format!("{}/anthropic", server.uri()),
                "anthropic.claude",
            ),
        ] {
            build(&host, &base, protocol)
                .execute(request(model), protocol)
                .await
                .expect("execute");
        }
        let received = server.received_requests().await.expect("requests");
        assert_eq!(received.len(), 4);
        for request in &received {
            assert_signed(request);
        }
    });
}

#[test]
fn d266_both_route_retry_requests_are_signed() {
    block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_string(
                r#"{"error":{"message":"The model `newmaker.model` isn't supported on this route"}}"#,
            ))
            .mount(&server)
            .await;
        mount_post(&server, "/openai/v1/chat/completions", CHAT_REPLY).await;
        let (_root, host) = runtime("");
        build(
            &host,
            &format!("{}/v1", server.uri()),
            ProtocolId::OPENAI_CHAT,
        )
        .execute(request("newmaker.model"), ProtocolId::OPENAI_CHAT)
        .await
        .expect("retry");
        let received = server.received_requests().await.expect("requests");
        assert_eq!(received.len(), 2);
        for request in &received {
            assert_signed(request);
        }
    });
}

#[test]
fn d266_api_key_wins_for_requests_and_model_list() {
    let root = tempfile::tempdir().expect("root");
    let host = TestHost::open(PluginId::from_static("bedrock-mantle"), root.path());
    host.secrets().insert(
        &PluginId::from_static("bedrock-mantle"),
        "api_key",
        "ABSK-test",
    );
    let provider = build(
        host.context(),
        "https://proxy.test/v1",
        ProtocolId::OPENAI_CHAT,
    );
    host.http().enqueue(CHAT_REPLY);
    block_on(provider.execute(request("openai.gpt-oss-120b"), ProtocolId::OPENAI_CHAT))
        .expect("chat");
    host.http()
        .enqueue(r#"{"data":[{"id":"openai.gpt-oss-120b"}]}"#);
    block_on(provider.list_models(&ListQuery::new())).expect("list");
    let calls = host.http().calls();
    assert_eq!(calls.len(), 2);
    for call in calls {
        assert_eq!(call.header("x-amz-date"), None);
        assert!(
            !call
                .sensitive_headers()
                .iter()
                .any(|name| name == "x-amz-security-token")
        );
        assert!(
            call.sensitive_headers()
                .iter()
                .any(|name| name == "authorization")
        );
    }
}

#[test]
fn d266_fixed_clock_matches_the_mantle_list_signature() {
    block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;
        let (_root, host) = runtime("");
        let signer = AwsSigner::new("us-east-1", ID, SecretString::from(SECRET.to_owned()))
            .service("bedrock-mantle")
            .session_token(SecretString::from(TOKEN.to_owned()))
            .fixed(1_795_552_519, "bedrock-mantle.us-east-1.api.aws");
        let call = wire::signed_call(
            Call::get("https://bedrock-mantle.us-east-1.api.aws/v1/models"),
            &signer,
        )
        .expect("signed")
        .with_uri(format!("{}/v1/models", server.uri()));
        call.open_raw(host.http(), 1).await.expect("sent");
        let received = server.received_requests().await.expect("requests");
        assert_eq!(
            header(&received[0], "authorization"),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20261124/us-east-1/bedrock-mantle/aws4_request, SignedHeaders=host;x-amz-date;x-amz-security-token, Signature=703c844c7c4a7a9e5b7202d3f808c466ef8e0acdb4232a1816be16085b78f1eb"
        );
    });
}

#[test]
fn d266_scope_defaults_and_the_schema_exposes_aws_options() {
    let config =
        UpstreamFactory::validate(&MantleFactory, &raw("https://proxy.test/v1")).expect("config");
    let signer = super::mantle_signer(&config)
        .expect("signer")
        .expect("complete credentials");
    assert_eq!(signer.region(), "us-east-1");
    let config = UpstreamFactory::validate(
        &MantleFactory,
        &raw("https://bedrock-mantle.eu-west-2.api.aws/v1"),
    )
    .expect("regional config");
    let signer = super::mantle_signer(&config)
        .expect("regional signer")
        .expect("complete credentials");
    assert_eq!(signer.region(), "eu-west-2");
    let schema = UpstreamFactory::config_schema(&MantleFactory);
    let names = schema.field_names();
    for name in [
        "region",
        "access_key_id",
        "secret_access_key",
        "session_token",
    ] {
        assert!(names.contains(&name), "{name}");
    }
}

#[test]
fn d266_list_errors_redact_the_access_secret_and_session_token() {
    let root = tempfile::tempdir().expect("root");
    let host = TestHost::open(PluginId::from_static("bedrock-mantle"), root.path());
    let provider = build(
        host.context(),
        "https://proxy.test/v1",
        ProtocolId::OPENAI_CHAT,
    );
    host.http().enqueue_response(
        401,
        &[],
        format!(r#"{{"error":{{"message":"{SECRET} {TOKEN}"}}}}"#),
    );
    let error = block_on(provider.list_models(&ListQuery::new())).expect_err("list error");
    let text = error.to_string();
    assert!(!text.contains(SECRET));
    assert!(!text.contains(TOKEN));
}

#[test]
fn d266_execute_retry_and_count_errors_redact_credentials_on_every_route() {
    const API_KEY: &str = "ABSK-test-secret";

    fn assert_clean(error: &hofund_plugin_api::PluginError, api_key: Option<&str>) {
        let text = error.to_string();
        assert!(!text.contains(SECRET));
        assert!(!text.contains(TOKEN));
        if let Some(api_key) = api_key {
            assert!(!text.contains(api_key));
        }
        assert_eq!(error.retry_after(), Some(std::time::Duration::from_secs(7)));
    }

    fn body(api_key: Option<&str>) -> String {
        format!(
            r#"{{"message":"canonical request {SECRET} {TOKEN} {}"}}"#,
            api_key.unwrap_or("")
        )
    }

    fn route_refusal() -> String {
        format!("The model isn't supported on this route: {SECRET} {TOKEN}")
    }

    let root = tempfile::tempdir().expect("root");
    let host = TestHost::open(PluginId::from_static("bedrock-mantle"), root.path());
    for (index, protocol) in [
        ProtocolId::OPENAI_CHAT,
        ProtocolId::OPENAI_RESPONSES,
        ProtocolId::ANTHROPIC_MESSAGES,
    ]
    .into_iter()
    .enumerate()
    {
        let base = if protocol == ProtocolId::ANTHROPIC_MESSAGES {
            "https://proxy.test/anthropic"
        } else {
            "https://proxy.test/v1"
        };
        let provider = build(host.context(), base, protocol);
        let status = if index % 2 == 0 { 401 } else { 403 };
        host.http()
            .enqueue_response(status, &[("retry-after", "7")], body(None));
        let error = block_on(provider.execute(request("openai.gpt-oss-120b"), protocol))
            .map(drop)
            .expect_err("refused");
        assert_eq!(error.status(), Some(status));
        assert_clean(&error, None);
    }
    let messages = build(
        host.context(),
        "https://proxy.test/anthropic",
        ProtocolId::ANTHROPIC_MESSAGES,
    );
    host.http()
        .enqueue_response(403, &[("retry-after", "7")], body(None));
    let error = block_on(messages.count_input(request("demo"), ProtocolId::ANTHROPIC_MESSAGES))
        .expect_err("count refused");
    assert_clean(&error, None);

    let chat = build(
        host.context(),
        "https://proxy.test/v1",
        ProtocolId::OPENAI_CHAT,
    );
    host.http()
        .enqueue_response(400, &[], format!(r#"{{"message":"{}"}}"#, route_refusal()));
    host.http()
        .enqueue_response(403, &[("retry-after", "7")], body(None));
    let error = block_on(chat.execute(request("newmaker.model"), ProtocolId::OPENAI_CHAT))
        .map(drop)
        .expect_err("second route refused");
    assert_clean(&error, None);

    host.http().enqueue_response(
        400,
        &[("retry-after", "7")],
        format!(r#"{{"message":"{}"}}"#, route_refusal()),
    );
    host.http().enqueue_response(
        400,
        &[("retry-after", "7")],
        format!(r#"{{"message":"{}"}}"#, route_refusal()),
    );
    let error = block_on(chat.execute(request("newmaker.other"), ProtocolId::OPENAI_CHAT))
        .map(drop)
        .expect_err("both routes refused");
    assert_clean(&error, None);

    host.secrets()
        .insert(&PluginId::from_static("bedrock-mantle"), "api_key", API_KEY);
    for protocol in [
        ProtocolId::OPENAI_CHAT,
        ProtocolId::OPENAI_RESPONSES,
        ProtocolId::ANTHROPIC_MESSAGES,
    ] {
        let base = if protocol == ProtocolId::ANTHROPIC_MESSAGES {
            "https://proxy.test/anthropic"
        } else {
            "https://proxy.test/v1"
        };
        let provider = build(host.context(), base, protocol);
        host.http()
            .enqueue_response(401, &[("retry-after", "7")], body(Some(API_KEY)));
        let error = block_on(provider.execute(request("openai.gpt-oss-120b"), protocol))
            .map(drop)
            .expect_err("refused");
        assert_clean(&error, Some(API_KEY));
    }
    let messages = build(
        host.context(),
        "https://proxy.test/anthropic",
        ProtocolId::ANTHROPIC_MESSAGES,
    );
    host.http()
        .enqueue_response(403, &[("retry-after", "7")], body(Some(API_KEY)));
    let error = block_on(messages.count_input(request("demo"), ProtocolId::ANTHROPIC_MESSAGES))
        .expect_err("count refused");
    assert_clean(&error, Some(API_KEY));
}

#[test]
fn d266_incomplete_access_key_pairs_fail_compatibility_builds() {
    for (key, expected) in [
        ("access_key_id", "Mantle access key id is empty"),
        ("secret_access_key", "Mantle secret access key is empty"),
    ] {
        let mut config = raw("https://proxy.test/v1");
        config.insert(key, RawValue::String(String::new()));
        let config = UpstreamFactory::validate(&MantleFactory, &config).expect("config");
        let error = super::mantle_signer(&config).expect_err("incomplete pair");
        assert!(error.to_string().contains(expected));
    }
}
