use std::sync::Arc;

use hofund_kit_upstream::{TestHost, block_on};
use hofund_plugin_api::{
    BuildContext, CountOutcome, DynUpstreamFactory, ListKind, ListQuery, PluginErrorKind, PluginId,
    ProbeQuery, ProtocolId, RawTable, RawValue, Request, UpstreamFactory, UpstreamProvider,
};
use hofund_plugin_sdk::json::{Value, json};

use super::{BedrockFactory, MODELS};

const RUNTIME: &str = "https://bedrock-runtime.us-east-1.amazonaws.com";
const CHAT: &str = "https://bedrock-runtime.us-east-1.amazonaws.com/openai/v1";
const MESSAGES: &str = "https://bedrock-runtime.us-east-1.amazonaws.com/anthropic";
const PRESETS: &str = include_str!("../presets.toml");

fn plugin() -> PluginId {
    PluginId::from_static("bedrock")
}

fn raw(pairs: &[(&str, &str)]) -> RawTable {
    let mut raw = RawTable::new();
    for (key, value) in pairs {
        raw.insert(*key, RawValue::String((*value).to_owned()));
    }
    raw
}

fn signed(base: &str) -> RawTable {
    raw(&[
        ("base_url", base),
        ("access_key_id", "AKIDEXAMPLE"),
        ("secret_access_key", "fake-secret"),
    ])
}

fn build(host: &TestHost, raw: &RawTable, protocol: ProtocolId) -> Arc<dyn UpstreamProvider> {
    DynUpstreamFactory::validate_and_build_with(
        &BedrockFactory,
        raw,
        host.context(),
        &BuildContext::new(protocol),
    )
    .expect("build")
}

fn build_error(raw: &RawTable, protocol: ProtocolId) -> String {
    let root = tempfile::tempdir().expect("scratch");
    let host = TestHost::open(plugin(), root.path());
    match DynUpstreamFactory::validate_and_build_with(
        &BedrockFactory,
        raw,
        host.context(),
        &BuildContext::new(protocol),
    ) {
        Ok(_) => panic!("must not build"),
        Err(error) => error.to_string(),
    }
}

fn with_host<T>(test: impl FnOnce(&TestHost) -> T) -> T {
    let root = tempfile::tempdir().expect("scratch");
    let host = TestHost::open(plugin(), root.path());
    test(&host)
}

fn user_request(model: &str) -> Request {
    let mut request: Request = hofund_plugin_sdk::json::from_value(json!({
        "model": model,
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
    }))
    .expect("request");
    request.stream = false;
    request
}

fn sent(body: &[u8]) -> Value {
    hofund_plugin_sdk::json::from_slice(body).expect("json body")
}

fn has(names: &[String], name: &str) -> bool {
    names.iter().any(|present| present == name)
}

const CONVERSE_REPLY: &str = r#"{"output":{"message":{"role":"assistant","content":[{"text":"ok"}]}},"stopReason":"end_turn","usage":{"inputTokens":1,"outputTokens":1,"totalTokens":2}}"#;
const CHAT_REPLY: &str = r#"{"id":"c1","object":"chat.completion","model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#;
const MESSAGES_REPLY: &str = r#"{"id":"m1","type":"message","role":"assistant","model":"m","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#;

#[test]
fn the_static_models_match_the_catalog_row() {
    let start = PRESETS.find("models = [").expect("models line");
    let line = PRESETS[start..].lines().next().expect("line");
    let listed: Vec<&str> = line
        .trim_start_matches("models = [")
        .trim_end_matches(']')
        .split(", ")
        .map(|id| id.trim_matches('"'))
        .collect();
    assert_eq!(listed, MODELS);
}

#[test]
fn the_catalog_lists_with_no_network_call() {
    with_host(|host| {
        let provider = build(host, &signed(RUNTIME), ProtocolId::BEDROCK_CONVERSE);
        let list = block_on(provider.list_models(&ListQuery::new())).expect("list");
        assert_eq!(list.kind(), ListKind::Catalog);
        let ids: Vec<&str> = list.models().iter().map(|model| model.id()).collect();
        assert_eq!(ids, MODELS);
        assert!(host.http().calls().is_empty());
    });
}

#[test]
fn validate_rejects_unknown_and_mistyped_options_without_their_values() {
    let mut unknown = signed(RUNTIME);
    unknown.insert("project", RawValue::String("SENTINEL-PROJECT".to_owned()));
    let error = UpstreamFactory::validate(&BedrockFactory, &unknown)
        .err()
        .expect("unknown key");
    assert_eq!(error.path(), "project");
    assert!(!error.to_string().contains("SENTINEL"), "{error}");

    let mut nested = RawTable::new();
    nested.insert("leak", RawValue::String("SENTINEL-NESTED".to_owned()));
    let mut mistyped = signed(RUNTIME);
    mistyped.insert("region", RawValue::Table(nested));
    let error = UpstreamFactory::validate(&BedrockFactory, &mistyped)
        .err()
        .expect("mistyped");
    assert_eq!(error.path(), "region");
    assert!(error.to_string().contains("must be a string"), "{error}");
    assert!(!error.to_string().contains("SENTINEL"), "{error}");

    let mut headers = signed(RUNTIME);
    headers.insert("headers", RawValue::String("x".to_owned()));
    let error = UpstreamFactory::validate(&BedrockFactory, &headers)
        .err()
        .expect("headers");
    assert_eq!(error.path(), "headers");

    let mut table = RawTable::new();
    table.insert("x-team", RawValue::Integer(1));
    let mut headers = signed(RUNTIME);
    headers.insert("headers", RawValue::Table(table));
    let error = UpstreamFactory::validate(&BedrockFactory, &headers)
        .err()
        .expect("header value");
    assert_eq!(error.path(), "headers.x-team");
}

#[test]
fn the_schema_lists_the_base_and_the_aws_options() {
    let schema = UpstreamFactory::config_schema(&BedrockFactory);
    let names = schema.field_names();
    for name in [
        "base_url",
        "region",
        "access_key_id",
        "secret_access_key",
        "session_token",
    ] {
        assert!(names.contains(&name), "{name}");
    }
    assert!(!names.contains(&"project"));
}

#[test]
fn an_empty_base_or_an_unserved_protocol_fails_build() {
    for protocol in [
        ProtocolId::OPENAI_CHAT,
        ProtocolId::OPENAI_RESPONSES,
        ProtocolId::ANTHROPIC_MESSAGES,
        ProtocolId::BEDROCK_CONVERSE,
    ] {
        let error = build_error(&signed(" / "), protocol);
        assert!(
            error.contains("provider base URL is empty"),
            "{protocol}: {error}"
        );
    }
    // The host refuses an undeclared protocol first; the plugin refuses too.
    with_host(|host| {
        let config = UpstreamFactory::validate(&BedrockFactory, &signed(RUNTIME)).expect("config");
        let error = UpstreamFactory::build_with(
            &BedrockFactory,
            &config,
            host.context(),
            &BuildContext::new(ProtocolId::GENERATIVE_LANGUAGE),
        )
        .err()
        .expect("unserved");
        assert!(
            error
                .to_string()
                .contains("bedrock does not serve protocol `generative-language`"),
            "{error}"
        );
    });
}

#[test]
fn the_default_build_is_chat() {
    with_host(|host| {
        let config = UpstreamFactory::validate(&BedrockFactory, &raw(&[("base_url", CHAT)]))
            .expect("config");
        let provider =
            UpstreamFactory::build(&BedrockFactory, &config, host.context()).expect("build");
        host.http().enqueue(CHAT_REPLY);
        block_on(provider.execute(
            user_request("openai.gpt-oss-20b-1:0"),
            ProtocolId::OPENAI_CHAT,
        ))
        .expect("chat");
        let calls = host.http().calls();
        assert_eq!(calls[0].uri(), format!("{CHAT}/chat/completions"));
    });
}

#[test]
fn converse_needs_both_keys_and_a_region() {
    let mut missing = signed(RUNTIME);
    missing.insert("access_key_id", RawValue::String("  ".to_owned()));
    let error = build_error(&missing, ProtocolId::BEDROCK_CONVERSE);
    assert!(error.contains("Converse access key id is empty"), "{error}");

    let mut missing = signed(RUNTIME);
    missing.insert("secret_access_key", RawValue::String(String::new()));
    let error = build_error(&missing, ProtocolId::BEDROCK_CONVERSE);
    assert!(
        error.contains("Converse secret access key is empty"),
        "{error}"
    );

    let mut token = signed(RUNTIME);
    token.insert("session_token", RawValue::String("a\nb".to_owned()));
    let error = build_error(&token, ProtocolId::BEDROCK_CONVERSE);
    assert!(
        error.contains("Converse session token is not valid header text"),
        "{error}"
    );

    let error = build_error(
        &signed("https://proxy.example.test"),
        ProtocolId::BEDROCK_CONVERSE,
    );
    assert!(error.contains("Converse needs a region"), "{error}");

    let mut region = signed("https://proxy.example.test");
    region.insert("region", RawValue::String("us east".to_owned()));
    let error = build_error(&region, ProtocolId::BEDROCK_CONVERSE);
    assert!(error.contains("is not a region id"), "{error}");

    // The API-key wires take no AWS keys, so empty ones are fine there.
    with_host(|host| {
        build(
            host,
            &raw(&[("base_url", CHAT)]),
            ProtocolId::OPENAI_RESPONSES,
        );
    });
}

#[test]
fn converse_signs_the_model_path_at_the_runtime_root() {
    with_host(|host| {
        let mut config = signed(CHAT);
        config.insert("session_token", RawValue::String(" SESSION ".to_owned()));
        let provider = build(host, &config, ProtocolId::BEDROCK_CONVERSE);
        host.http().enqueue(CONVERSE_REPLY);
        let completion = block_on(provider.execute(
            user_request("anthropic.claude-3-5-sonnet-20240620-v1:0"),
            ProtocolId::OPENAI_CHAT,
        ))
        .expect("converse");
        assert!(matches!(
            completion,
            hofund_plugin_api::Completion::Unary(_)
        ));
        let calls = host.http().calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].uri(),
            format!("{RUNTIME}/model/anthropic.claude-3-5-sonnet-20240620-v1%3A0/converse")
        );
        let names = calls[0].sensitive_headers();
        assert!(has(names, "authorization"), "{names:?}");
        assert!(has(names, "x-amz-security-token"), "{names:?}");
        let body = sent(calls[0].body());
        assert_eq!(body["messages"][0]["content"][0]["text"], "hi");
        assert!(body.get("model").is_none(), "{body}");
    });
}

#[test]
fn converse_without_a_session_token_sends_none() {
    with_host(|host| {
        let provider = build(host, &signed(MESSAGES), ProtocolId::BEDROCK_CONVERSE);
        host.http().enqueue(CONVERSE_REPLY);
        block_on(provider.execute(user_request("demo"), ProtocolId::OPENAI_CHAT))
            .expect("converse");
        let calls = host.http().calls();
        assert_eq!(calls[0].uri(), format!("{RUNTIME}/model/demo/converse"));
        assert!(!has(calls[0].sensitive_headers(), "x-amz-security-token"));
    });
}

#[test]
fn converse_refuses_a_stored_api_key() {
    with_host(|host| {
        host.secrets().insert(&plugin(), "api_key", "ABSK-SENTINEL");
        let provider = build(host, &signed(RUNTIME), ProtocolId::BEDROCK_CONVERSE);
        let error = block_on(provider.execute(user_request("demo"), ProtocolId::OPENAI_CHAT))
            .err()
            .expect("api key");
        assert!(error.to_string().contains("not an API key"), "{error}");
        assert!(!error.to_string().contains("SENTINEL"), "{error}");
        let error = block_on(
            provider.probe(&ProbeQuery::new(ProtocolId::BEDROCK_CONVERSE, "demo").expect("probe")),
        )
        .err()
        .expect("probe key");
        assert_eq!(error.kind(), PluginErrorKind::Config);
        assert!(error.to_string().contains("not an API key"), "{error}");
        assert!(host.http().calls().is_empty());
    });
}

#[test]
fn converse_probes_one_minimal_signed_call() {
    with_host(|host| {
        let provider = build(host, &signed(RUNTIME), ProtocolId::BEDROCK_CONVERSE);
        host.http().enqueue("{}");
        block_on(provider.probe(
            &ProbeQuery::new(ProtocolId::BEDROCK_CONVERSE, "anthropic.claude").expect("probe"),
        ))
        .expect("probe");
        let calls = host.http().calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].uri(),
            format!("{RUNTIME}/model/anthropic.claude/converse")
        );
        let body = sent(calls[0].body());
        assert_eq!(body["inferenceConfig"]["maxTokens"], 16);
        assert_eq!(body["messages"][0]["content"][0]["text"], "hi");
        assert!(has(calls[0].sensitive_headers(), "authorization"));
    });
}

#[test]
fn a_streamed_converse_probe_uses_the_stream_path() {
    with_host(|host| {
        let provider = build(host, &signed(RUNTIME), ProtocolId::BEDROCK_CONVERSE);
        host.http().enqueue("");
        let query = ProbeQuery::new(ProtocolId::BEDROCK_CONVERSE, "demo")
            .expect("probe")
            .with_extra_body(br#"{"stream":true}"#.to_vec().into());
        let _ = block_on(provider.probe(&query));
        let calls = host.http().calls();
        assert_eq!(
            calls[0].uri(),
            format!("{RUNTIME}/model/demo/converse-stream")
        );
    });
}

#[test]
fn chat_sends_the_key_as_a_bearer_token_with_max_completion_tokens() {
    with_host(|host| {
        host.secrets().insert(&plugin(), "api_key", "ABSK-key");
        let provider = build(
            host,
            &raw(&[("base_url", "  https://x.test/openai/v1/ ")]),
            ProtocolId::OPENAI_CHAT,
        );
        host.http().enqueue(CHAT_REPLY);
        let mut request = user_request("openai.gpt-oss-20b-1:0");
        request.params.max_tokens = Some(10);
        block_on(provider.execute(request, ProtocolId::OPENAI_CHAT)).expect("chat");
        host.http().enqueue("{}");
        block_on(provider.probe(
            &ProbeQuery::new(ProtocolId::OPENAI_CHAT, "openai.gpt-oss-20b-1:0").expect("probe"),
        ))
        .expect("probe");
        let calls = host.http().calls();
        assert_eq!(calls.len(), 2);
        for call in &calls {
            assert_eq!(call.uri(), "https://x.test/openai/v1/chat/completions");
            assert!(has(call.sensitive_headers(), "authorization"));
            assert!(!has(call.sensitive_headers(), "x-api-key"));
            let body = sent(call.body());
            assert!(body.get("max_completion_tokens").is_some(), "{body}");
            assert!(body.get("max_tokens").is_none(), "{body}");
        }
        assert_eq!(sent(calls[1].body())["max_completion_tokens"], 16);
    });
}

#[test]
fn responses_probe_posts_a_minimal_input() {
    with_host(|host| {
        let provider = build(
            host,
            &raw(&[("base_url", CHAT)]),
            ProtocolId::OPENAI_RESPONSES,
        );
        host.http().enqueue("{}");
        block_on(
            provider.probe(
                &ProbeQuery::new(ProtocolId::OPENAI_RESPONSES, "openai.gpt-oss-20b-1:0")
                    .expect("probe"),
            ),
        )
        .expect("probe");
        let calls = host.http().calls();
        assert_eq!(calls[0].uri(), format!("{CHAT}/responses"));
        let body = sent(calls[0].body());
        assert_eq!(body["max_output_tokens"], 16);
        assert_eq!(body["input"][0]["content"][0]["text"], "hi");
        // No key stored: no credential header at all.
        assert!(!has(calls[0].sensitive_headers(), "authorization"));
    });
}

#[test]
fn messages_send_the_key_alone_and_fit_the_body() {
    with_host(|host| {
        host.secrets().insert(&plugin(), "api_key", "ABSK-key");
        let provider = build(
            host,
            &raw(&[("base_url", MESSAGES)]),
            ProtocolId::ANTHROPIC_MESSAGES,
        );
        host.http().enqueue(MESSAGES_REPLY);
        let mut request = user_request("global.anthropic.claude-opus-4-7");
        request
            .extensions
            .insert("metadata", json!({"user_id": "u"}));
        request.extensions.insert(
            "anthropic_beta",
            json!(["advanced-tool-use-2025-11-20, redact-thinking-2026-02-12"]),
        );
        block_on(provider.execute(request, ProtocolId::ANTHROPIC_MESSAGES)).expect("messages");
        let calls = host.http().calls();
        assert_eq!(calls[0].uri(), format!("{MESSAGES}/v1/messages"));
        let names = calls[0].sensitive_headers();
        assert!(has(names, "x-api-key"), "{names:?}");
        assert!(!has(names, "authorization"), "{names:?}");
        let body = sent(calls[0].body());
        assert!(body.get("metadata").is_none(), "{body}");
        assert_eq!(
            body["anthropic_beta"],
            json!(["tool-search-tool-2025-10-19"])
        );
    });
}

#[test]
fn messages_probe_sends_the_version_and_the_key() {
    with_host(|host| {
        host.secrets().insert(&plugin(), "api_key", "ABSK-key");
        let provider = build(
            host,
            &raw(&[("base_url", MESSAGES)]),
            ProtocolId::ANTHROPIC_MESSAGES,
        );
        host.http().enqueue("{}");
        block_on(
            provider
                .probe(&ProbeQuery::new(ProtocolId::ANTHROPIC_MESSAGES, "claude").expect("probe")),
        )
        .expect("probe");
        let calls = host.http().calls();
        assert_eq!(calls[0].uri(), format!("{MESSAGES}/v1/messages"));
        assert_eq!(sent(calls[0].body())["max_tokens"], 16);
        assert!(has(calls[0].sensitive_headers(), "x-api-key"));
        assert!(!has(calls[0].sensitive_headers(), "authorization"));
    });
}

#[test]
fn a_configured_credential_header_replaces_the_key() {
    with_host(|host| {
        host.secrets().insert(&plugin(), "api_key", "ABSK-key");
        let mut config = raw(&[("base_url", MESSAGES)]);
        let mut headers = RawTable::new();
        headers.insert("X-Api-Key", RawValue::String("configured".to_owned()));
        headers.insert(
            "Anthropic-Version",
            RawValue::String("2099-01-01".to_owned()),
        );
        config.insert("headers", RawValue::Table(headers));
        let provider = build(host, &config, ProtocolId::ANTHROPIC_MESSAGES);
        host.http().enqueue("{}");
        block_on(
            provider
                .probe(&ProbeQuery::new(ProtocolId::ANTHROPIC_MESSAGES, "claude").expect("probe")),
        )
        .expect("probe");
        let names = host.http().calls()[0].sensitive_headers().to_vec();
        assert_eq!(
            names.iter().filter(|name| *name == "x-api-key").count(),
            1,
            "{names:?}"
        );
        assert!(has(&names, "anthropic-version"), "{names:?}");
    });
}

#[test]
fn a_probe_error_hides_the_key_and_header_values() {
    with_host(|host| {
        host.secrets()
            .insert(&plugin(), "api_key", "ABSK-SENTINEL-KEY");
        let mut config = raw(&[("base_url", CHAT)]);
        let mut headers = RawTable::new();
        headers.insert("x-team", RawValue::String("SENTINEL-HEADER".to_owned()));
        config.insert("headers", RawValue::Table(headers));
        let provider = build(host, &config, ProtocolId::OPENAI_CHAT);
        host.http().enqueue_response(
            401,
            &[],
            r#"{"error":{"message":"bad ABSK-SENTINEL-KEY SENTINEL-HEADER"}}"#,
        );
        let report = block_on(
            provider.probe(&ProbeQuery::new(ProtocolId::OPENAI_CHAT, "m").expect("probe")),
        );
        let text = match report {
            Ok(report) => format!("{report:?}"),
            Err(error) => error.to_string(),
        };
        assert!(!text.contains("SENTINEL"), "{text}");
        assert!(has(host.http().calls()[0].sensitive_headers(), "x-team"));
    });
}

#[test]
fn count_input_is_messages_only() {
    with_host(|host| {
        let chat = build(host, &raw(&[("base_url", CHAT)]), ProtocolId::OPENAI_CHAT);
        let outcome = block_on(chat.count_input(user_request("m"), ProtocolId::OPENAI_CHAT))
            .expect("chat count");
        assert!(matches!(outcome, CountOutcome::Unsupported));
        let converse = build(host, &signed(RUNTIME), ProtocolId::BEDROCK_CONVERSE);
        let outcome = block_on(converse.count_input(user_request("m"), ProtocolId::OPENAI_CHAT))
            .expect("converse count");
        assert!(matches!(outcome, CountOutcome::Unsupported));
        assert!(host.http().calls().is_empty());

        host.secrets().insert(&plugin(), "api_key", "ABSK-key");
        let messages = build(
            host,
            &raw(&[("base_url", MESSAGES)]),
            ProtocolId::ANTHROPIC_MESSAGES,
        );
        host.http().enqueue(r#"{"input_tokens":7}"#);
        let mut request = user_request("claude");
        request
            .extensions
            .insert("metadata", json!({"user_id": "u"}));
        let outcome = block_on(messages.count_input(request, ProtocolId::ANTHROPIC_MESSAGES))
            .expect("messages count");
        assert!(!matches!(outcome, CountOutcome::Unsupported));
        let calls = host.http().calls();
        assert_eq!(
            calls[0].uri(),
            format!("{MESSAGES}/v1/messages/count_tokens")
        );
        assert!(sent(calls[0].body()).get("metadata").is_none());
    });
}
