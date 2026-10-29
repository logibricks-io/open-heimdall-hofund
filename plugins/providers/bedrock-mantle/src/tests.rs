use std::sync::Arc;

use hofund_kit_upstream::{TestHost, block_on};
use hofund_plugin_api::{
    AuthChoice, BuildContext, CountOutcome, DynUpstreamFactory, ListKind, ListQuery,
    PluginErrorKind, PluginId, ProbeQuery, ProtocolId, RawTable, RawValue, Request,
    UpstreamFactory, UpstreamProvider,
};
use hofund_plugin_sdk::json::{Value, json};

use super::MantleFactory;

const ROOT: &str = "https://bedrock-mantle.us-east-1.api.aws";
const V1: &str = "https://bedrock-mantle.us-east-1.api.aws/v1";
const OPENAI: &str = "https://bedrock-mantle.us-east-1.api.aws/openai/v1";
const MESSAGES: &str = "https://bedrock-mantle.us-east-1.api.aws/anthropic";

const CHAT_REPLY: &str = r#"{"id":"c1","object":"chat.completion","model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#;
const RESPONSES_REPLY: &str = r#"{"id":"r1","object":"response","status":"completed","model":"m","output":[{"type":"message","id":"o1","role":"assistant","content":[{"type":"output_text","text":"ok"}]}]}"#;
const MESSAGES_REPLY: &str = r#"{"id":"m1","type":"message","role":"assistant","model":"m","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#;
const REFUSED: &str = r#"{"error":{"message":"The model `m` isn't supported on this route"}}"#;

fn plugin() -> PluginId {
    PluginId::from_static("bedrock-mantle")
}

fn raw(base: &str) -> RawTable {
    let mut raw = RawTable::new();
    raw.insert("base_url", RawValue::String(base.to_owned()));
    raw
}

fn build(host: &TestHost, raw: &RawTable, protocol: ProtocolId) -> Arc<dyn UpstreamProvider> {
    DynUpstreamFactory::validate_and_build_with(
        &MantleFactory,
        raw,
        host.context(),
        &BuildContext::new(protocol),
    )
    .expect("build")
}

fn with_host<T>(test: impl FnOnce(&TestHost) -> T) -> T {
    let root = tempfile::tempdir().expect("scratch");
    let host = TestHost::open(plugin(), root.path());
    host.secrets().insert(&plugin(), "api_key", "ABSK-test");
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

fn uris(host: &TestHost) -> Vec<String> {
    host.http()
        .calls()
        .iter()
        .map(|call| call.uri().to_owned())
        .collect()
}

fn chat(provider: &Arc<dyn UpstreamProvider>, model: &str) -> Result<(), PluginErrorKind> {
    block_on(provider.execute(user_request(model), ProtocolId::OPENAI_CHAT))
        .map(|_| ())
        .map_err(|error| error.kind())
}

#[test]
fn chat_and_responses_pick_the_prefix_by_model_id() {
    with_host(|host| {
        let provider = build(host, &raw(V1), ProtocolId::OPENAI_CHAT);
        for _ in 0..3 {
            host.http().enqueue(CHAT_REPLY);
        }
        for model in ["openai.gpt-5.5", "openai.gpt-oss-120b", "xai.grok-4.3"] {
            chat(&provider, model).expect("chat");
        }
        assert_eq!(
            uris(host),
            [
                format!("{OPENAI}/chat/completions"),
                format!("{V1}/chat/completions"),
                format!("{OPENAI}/chat/completions"),
            ]
        );
        for call in host.http().calls() {
            assert!(has(call.sensitive_headers(), "authorization"));
            assert!(!has(call.sensitive_headers(), "x-api-key"));
        }
    });
    with_host(|host| {
        let provider = build(
            host,
            &raw("https://bedrock-mantle.eu-west-1.api.aws/openai/v1/"),
            ProtocolId::OPENAI_RESPONSES,
        );
        host.http().enqueue(RESPONSES_REPLY);
        block_on(provider.execute(user_request("openai.gpt-5.5"), ProtocolId::OPENAI_RESPONSES))
            .expect("responses");
        assert_eq!(
            uris(host),
            ["https://bedrock-mantle.eu-west-1.api.aws/openai/v1/responses"]
        );
    });
}

#[test]
fn a_mantle_root_is_normalized_per_wire() {
    with_host(|host| {
        let provider = build(host, &raw(ROOT), ProtocolId::OPENAI_CHAT);
        host.http().enqueue(CHAT_REPLY);
        chat(&provider, "google.gemma-3-27b-it").expect("chat");
        let provider = build(host, &raw(V1), ProtocolId::ANTHROPIC_MESSAGES);
        host.http().enqueue(MESSAGES_REPLY);
        block_on(provider.execute(
            user_request("anthropic.claude"),
            ProtocolId::ANTHROPIC_MESSAGES,
        ))
        .expect("messages");
        assert_eq!(
            uris(host),
            [
                format!("{V1}/chat/completions"),
                format!("{MESSAGES}/v1/messages"),
            ]
        );
    });
}

#[test]
fn a_preset_base_on_another_host_keeps_its_path_and_picks_a_prefix() {
    with_host(|host| {
        let provider = build(
            host,
            &raw("https://relay.example/v1"),
            ProtocolId::OPENAI_CHAT,
        );
        host.http().enqueue(CHAT_REPLY);
        chat(&provider, "openai.gpt-5.5").expect("chat");
        let direct = build(
            host,
            &raw("https://relay.example/mantle"),
            ProtocolId::OPENAI_CHAT,
        );
        host.http().enqueue(CHAT_REPLY);
        chat(&direct, "openai.gpt-5.5").expect("chat");
        assert_eq!(
            uris(host),
            [
                "https://relay.example/openai/v1/chat/completions",
                "https://relay.example/mantle/chat/completions",
            ]
        );
    });
}

#[test]
fn a_refused_prefix_is_tried_once_at_the_other_and_remembered() {
    with_host(|host| {
        let provider = build(host, &raw(V1), ProtocolId::OPENAI_CHAT);
        host.http().enqueue_response(400, &[], REFUSED);
        host.http().enqueue(CHAT_REPLY);
        host.http().enqueue(CHAT_REPLY);
        chat(&provider, "newmaker.model-1").expect("first");
        chat(&provider, "newmaker.model-1").expect("learned");
        assert_eq!(
            uris(host),
            [
                format!("{V1}/chat/completions"),
                format!("{OPENAI}/chat/completions"),
                format!("{OPENAI}/chat/completions"),
            ]
        );
        for call in host.http().calls() {
            assert!(has(call.sensitive_headers(), "authorization"));
        }
        // Another instance learns on its own.
        let fresh = build(host, &raw(V1), ProtocolId::OPENAI_CHAT);
        host.http().enqueue(CHAT_REPLY);
        chat(&fresh, "newmaker.model-1").expect("fresh");
        assert_eq!(uris(host)[3], format!("{V1}/chat/completions"));
    });
}

#[test]
fn a_refusal_at_both_prefixes_returns_the_first_error_and_forgets() {
    with_host(|host| {
        let provider = build(host, &raw(V1), ProtocolId::OPENAI_CHAT);
        host.http().enqueue_response(400, &[], REFUSED);
        host.http().enqueue_response(
            404,
            &[],
            r#"{"error":{"message":"model is not supported on this route either"}}"#,
        );
        let error =
            block_on(provider.execute(user_request("newmaker.model-1"), ProtocolId::OPENAI_CHAT))
                .map(drop)
                .expect_err("refused");
        assert_eq!(error.status(), Some(400));
        host.http().enqueue(CHAT_REPLY);
        chat(&provider, "newmaker.model-1").expect("guess again");
        assert_eq!(uris(host)[2], format!("{V1}/chat/completions"));
    });
}

#[test]
fn a_refusal_on_the_learned_prefix_is_not_retried_or_forgotten() {
    with_host(|host| {
        let provider = build(host, &raw(V1), ProtocolId::OPENAI_CHAT);
        host.http().enqueue_response(400, &[], REFUSED);
        host.http().enqueue(CHAT_REPLY);
        chat(&provider, "newmaker.model-1").expect("learn");
        host.http().enqueue_response(400, &[], REFUSED);
        assert_eq!(
            chat(&provider, "newmaker.model-1"),
            Err(PluginErrorKind::Status)
        );
        host.http().enqueue(CHAT_REPLY);
        chat(&provider, "newmaker.model-1").expect("still learned");
        let uris = uris(host);
        assert_eq!(uris.len(), 4);
        assert_eq!(uris[2], format!("{OPENAI}/chat/completions"));
        assert_eq!(uris[3], format!("{OPENAI}/chat/completions"));
    });
}

#[test]
fn a_second_transport_failure_forgets_the_route() {
    for fail in [0, 1] {
        with_host(|host| {
            let provider = build(host, &raw(V1), ProtocolId::OPENAI_CHAT);
            host.http().enqueue_response(400, &[], REFUSED);
            // The SDK tries a connect failure twice and a timed-out POST once.
            if fail == 0 {
                host.http().enqueue_connect("down");
                host.http().enqueue_connect("down");
            } else {
                host.http().enqueue_timeout("slow");
            }
            let error = block_on(
                provider.execute(user_request("newmaker.model-1"), ProtocolId::OPENAI_CHAT),
            )
            .map(drop)
            .expect_err("first error");
            assert_eq!(error.status(), Some(400));
            let before = host.http().calls().len();
            host.http().enqueue(CHAT_REPLY);
            chat(&provider, "newmaker.model-1").expect("guess again");
            assert_eq!(
                host.http().calls()[before].uri(),
                format!("{V1}/chat/completions")
            );
        });
    }
}

#[test]
fn a_broken_body_or_an_unrelated_second_error_keeps_the_route() {
    with_host(|host| {
        let provider = build(host, &raw(V1), ProtocolId::OPENAI_CHAT);
        host.http().enqueue_response(400, &[], REFUSED);
        host.http().enqueue("{not json");
        // A cross-protocol call decodes the accepted body, so reading it fails.
        let read = block_on(provider.execute(
            user_request("newmaker.model-1"),
            ProtocolId::ANTHROPIC_MESSAGES,
        ));
        assert!(read.is_err());
        host.http().enqueue(CHAT_REPLY);
        chat(&provider, "newmaker.model-1").expect("learned");
        assert_eq!(uris(host)[2], format!("{OPENAI}/chat/completions"));
    });
    with_host(|host| {
        let provider = build(host, &raw(V1), ProtocolId::OPENAI_CHAT);
        host.http().enqueue_response(400, &[], REFUSED);
        host.http()
            .enqueue_response(400, &[], r#"{"error":{"message":"invalid model id"}}"#);
        let error =
            block_on(provider.execute(user_request("newmaker.model-1"), ProtocolId::OPENAI_CHAT))
                .map(drop)
                .expect_err("second");
        assert!(error.message().contains("invalid model id"), "{error}");
        host.http().enqueue(CHAT_REPLY);
        chat(&provider, "newmaker.model-1").expect("learned");
        assert_eq!(uris(host)[2], format!("{OPENAI}/chat/completions"));
    });
}

#[test]
fn an_empty_model_or_another_status_is_never_rerouted() {
    with_host(|host| {
        let provider = build(host, &raw(V1), ProtocolId::OPENAI_CHAT);
        host.http().enqueue_response(400, &[], REFUSED);
        assert!(chat(&provider, "").is_err());
        host.http().enqueue_response(500, &[], REFUSED);
        assert!(chat(&provider, "newmaker.model-1").is_err());
        assert_eq!(uris(host).len(), 2);
    });
}

#[test]
fn messages_send_the_key_alone_and_fit_the_body() {
    with_host(|host| {
        let provider = build(host, &raw(MESSAGES), ProtocolId::ANTHROPIC_MESSAGES);
        host.http().enqueue(MESSAGES_REPLY);
        let mut request = user_request("anthropic.claude-opus-4-7");
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
fn only_messages_count_input() {
    with_host(|host| {
        let chat = build(host, &raw(V1), ProtocolId::OPENAI_CHAT);
        let outcome =
            block_on(chat.count_input(user_request("m"), ProtocolId::OPENAI_CHAT)).expect("count");
        assert!(matches!(outcome, CountOutcome::Unsupported));
        assert!(host.http().calls().is_empty());
        let messages = build(host, &raw(MESSAGES), ProtocolId::ANTHROPIC_MESSAGES);
        host.http().enqueue(r#"{"input_tokens":7}"#);
        let mut request = user_request("anthropic.claude");
        request
            .extensions
            .insert("metadata", json!({"user_id": "u"}));
        block_on(messages.count_input(request, ProtocolId::ANTHROPIC_MESSAGES)).expect("count");
        let calls = host.http().calls();
        assert_eq!(
            calls[0].uri(),
            format!("{MESSAGES}/v1/messages/count_tokens")
        );
        assert!(sent(calls[0].body()).get("metadata").is_none());
    });
}

#[test]
fn the_chat_probe_caps_completion_tokens_and_retries_the_other_prefix() {
    with_host(|host| {
        let provider = build(host, &raw(V1), ProtocolId::OPENAI_CHAT);
        host.http().enqueue_response(400, &[], REFUSED);
        host.http().enqueue("{}");
        block_on(
            provider.probe(
                &ProbeQuery::new(ProtocolId::OPENAI_CHAT, "newmaker.model-1").expect("probe"),
            ),
        )
        .expect("probe");
        let calls = host.http().calls();
        assert_eq!(calls[0].uri(), format!("{V1}/chat/completions"));
        assert_eq!(calls[1].uri(), format!("{OPENAI}/chat/completions"));
        let body = sent(calls[0].body());
        assert_eq!(body["max_completion_tokens"], 16);
        assert!(body.get("max_tokens").is_none());
        assert!(has(calls[0].sensitive_headers(), "authorization"));
    });
}

#[test]
fn the_responses_and_messages_probes_post_minimal_bodies() {
    with_host(|host| {
        let provider = build(host, &raw(V1), ProtocolId::OPENAI_RESPONSES);
        host.http().enqueue("{}");
        block_on(provider.probe(
            &ProbeQuery::new(ProtocolId::OPENAI_RESPONSES, "openai.gpt-5.5").expect("probe"),
        ))
        .expect("probe");
        let provider = build(host, &raw(MESSAGES), ProtocolId::ANTHROPIC_MESSAGES);
        host.http().enqueue_response(400, &[], REFUSED);
        let error = block_on(
            provider
                .probe(&ProbeQuery::new(ProtocolId::ANTHROPIC_MESSAGES, "claude").expect("probe")),
        )
        .expect_err("messages are not rerouted");
        assert_eq!(error.status(), Some(400));
        let calls = host.http().calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].uri(), format!("{OPENAI}/responses"));
        assert_eq!(sent(calls[0].body())["max_output_tokens"], 16);
        assert_eq!(calls[1].uri(), format!("{MESSAGES}/v1/messages"));
        assert_eq!(sent(calls[1].body())["max_tokens"], 16);
        assert!(has(calls[1].sensitive_headers(), "x-api-key"));
        assert!(!has(calls[1].sensitive_headers(), "authorization"));
    });
}

#[test]
fn a_probe_error_hides_the_key_and_header_values() {
    with_host(|host| {
        host.secrets()
            .insert(&plugin(), "api_key", "ABSK-SENTINEL-KEY");
        let mut config = raw(V1);
        let mut headers = RawTable::new();
        headers.insert("x-team", RawValue::String("SENTINEL-HEADER".to_owned()));
        config.insert("headers", RawValue::Table(headers));
        let provider = build(host, &config, ProtocolId::OPENAI_CHAT);
        host.http().enqueue_response(
            401,
            &[],
            r#"{"error":{"message":"bad ABSK-SENTINEL-KEY SENTINEL-HEADER"}}"#,
        );
        let error = block_on(
            provider.probe(&ProbeQuery::new(ProtocolId::OPENAI_CHAT, "m").expect("probe")),
        )
        .map(drop)
        .expect_err("refused");
        assert!(!error.to_string().contains("SENTINEL"), "{error}");
        assert!(has(host.http().calls()[0].sensitive_headers(), "x-team"));
    });
}

#[test]
fn the_list_is_live_and_a_v1_base_lists_only_its_own_models() {
    with_host(|host| {
        let provider = build(host, &raw(OPENAI), ProtocolId::OPENAI_CHAT);
        host.http().enqueue(
            r#"{"data":[{"id":"openai.gpt-oss-120b","display_name":"GPT OSS"},{"id":"text-embedding-3"}]}"#,
        );
        let list = block_on(provider.list_models(&ListQuery::new())).expect("list");
        assert_eq!(list.kind(), ListKind::Live);
        let ids: Vec<&str> = list.models().iter().map(|model| model.id()).collect();
        assert_eq!(ids, ["openai.gpt-oss-120b"]);
        assert_eq!(list.dropped(), ["text-embedding-3"]);
        assert_eq!(list.source_url(), Some(format!("{V1}/models").as_str()));
        let calls = host.http().calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].method(), "GET");
        let names = calls[0].sensitive_headers();
        assert!(
            has(names, "authorization") && has(names, "x-api-key"),
            "{names:?}"
        );
    });
}

#[test]
fn the_list_sends_the_learned_choice_and_messages_send_the_key_alone() {
    with_host(|host| {
        let provider = build(host, &raw(V1), ProtocolId::OPENAI_CHAT);
        host.http()
            .enqueue(r#"{"data":[{"id":"openai.gpt-oss-120b"}]}"#);
        block_on(provider.list_models(&ListQuery::new().with_auth_choice(AuthChoice::Bearer)))
            .expect("list");
        let provider = build(host, &raw(MESSAGES), ProtocolId::ANTHROPIC_MESSAGES);
        host.http()
            .enqueue(r#"{"data":[{"id":"anthropic.claude-opus-4-7"}]}"#);
        let list = block_on(provider.list_models(&ListQuery::new())).expect("list");
        assert!(!list.v1_fallback());
        let calls = host.http().calls();
        assert!(!has(calls[0].sensitive_headers(), "x-api-key"));
        assert!(has(calls[0].sensitive_headers(), "authorization"));
        assert_eq!(calls[1].uri(), format!("{MESSAGES}/models"));
        assert!(has(calls[1].sensitive_headers(), "x-api-key"));
        assert!(!has(calls[1].sensitive_headers(), "authorization"));
    });
}

#[test]
fn a_list_with_no_chat_model_moves_to_the_next_candidate() {
    with_host(|host| {
        let provider = build(host, &raw(MESSAGES), ProtocolId::ANTHROPIC_MESSAGES);
        host.http()
            .enqueue(r#"{"data":[{"id":"text-embedding-3"}]}"#);
        host.http()
            .enqueue(r#"{"data":[{"id":"anthropic.claude-opus-4-7"}]}"#);
        let list = block_on(provider.list_models(&ListQuery::new())).expect("list");
        let ids: Vec<&str> = list.models().iter().map(|model| model.id()).collect();
        assert_eq!(ids, ["anthropic.claude-opus-4-7"]);
        assert!(list.v1_fallback());
        assert_eq!(
            list.source_url(),
            Some(format!("{MESSAGES}/v1/models").as_str())
        );
    });
}

#[test]
fn a_messages_list_on_a_versioned_base_also_asks_the_v1_candidates() {
    with_host(|host| {
        let provider = build(
            host,
            &raw("https://relay.example/v1"),
            ProtocolId::ANTHROPIC_MESSAGES,
        );
        host.http()
            .enqueue_response(404, &[], r#"{"error":{"message":"no"}}"#);
        host.http()
            .enqueue(r#"{"data":[{"id":"anthropic.claude-opus-4-7"}]}"#);
        block_on(provider.list_models(&ListQuery::new())).expect("list");
        assert_eq!(
            uris(host),
            [
                "https://relay.example/v1/models",
                "https://relay.example/v1/v1/models",
            ]
        );
    });
}

#[test]
fn list_failures_are_joined_redacted_and_keep_the_wait() {
    with_host(|host| {
        host.secrets()
            .insert(&plugin(), "api_key", "ABSK-SENTINEL-KEY");
        let provider = build(
            host,
            &raw("https://relay.example/mantle/api"),
            ProtocolId::OPENAI_CHAT,
        );
        host.http().enqueue(r#"{"data":[]}"#);
        host.http().enqueue_connect("down");
        host.http().enqueue_response(
            429,
            &[("retry-after", "7")],
            r#"{"error":{"message":"slow down ABSK-SENTINEL-KEY"}}"#,
        );
        host.http().enqueue_response(
            429,
            &[("retry-after", "9")],
            r#"{"error":{"message":"again"}}"#,
        );
        let error = block_on(provider.list_models(&ListQuery::new())).expect_err("no list");
        let text = error.to_string();
        assert!(!text.contains("SENTINEL"), "{text}");
        assert!(text.contains("no models listed"), "{text}");
        assert!(text.contains("HTTP 429"), "{text}");
        assert_eq!(error.status(), Some(429));
        assert_eq!(error.retry_after(), Some(std::time::Duration::from_secs(9)));
        assert_eq!(
            uris(host),
            [
                "https://relay.example/mantle/api/models",
                "https://relay.example/mantle/api/v1/models",
                "https://relay.example/mantle/v1/models",
                "https://relay.example/mantle/models",
            ]
        );
    });
}

#[test]
fn validate_rejects_unknown_keys_and_build_refuses_an_empty_base_or_converse() {
    let mut unknown = raw(V1);
    unknown.insert("region", RawValue::String("SENTINEL".to_owned()));
    let error = UpstreamFactory::validate(&MantleFactory, &unknown)
        .map(drop)
        .expect_err("unknown");
    assert_eq!(error.path(), "region");
    let mut headers = raw(V1);
    headers.insert("headers", RawValue::String("x".to_owned()));
    assert_eq!(
        UpstreamFactory::validate(&MantleFactory, &headers)
            .map(drop)
            .expect_err("headers")
            .path(),
        "headers"
    );
    let mut table = RawTable::new();
    table.insert("x-team", RawValue::Integer(1));
    let mut headers = raw(V1);
    headers.insert("headers", RawValue::Table(table));
    assert_eq!(
        UpstreamFactory::validate(&MantleFactory, &headers)
            .map(drop)
            .expect_err("header value")
            .path(),
        "headers.x-team"
    );
    let mut base = RawTable::new();
    base.insert("base_url", RawValue::Integer(1));
    assert_eq!(
        UpstreamFactory::validate(&MantleFactory, &base)
            .map(drop)
            .expect_err("base")
            .path(),
        "base_url"
    );
    assert!(
        UpstreamFactory::config_schema(&MantleFactory)
            .field_names()
            .contains(&"base_url")
    );
    with_host(|host| {
        let config = UpstreamFactory::validate(&MantleFactory, &raw(" / ")).expect("config");
        let error = UpstreamFactory::build_with(
            &MantleFactory,
            &config,
            host.context(),
            &BuildContext::new(ProtocolId::OPENAI_CHAT),
        )
        .map(drop)
        .expect_err("empty");
        assert!(
            error.to_string().contains("provider base URL is empty"),
            "{error}"
        );
        let config = UpstreamFactory::validate(&MantleFactory, &raw(V1)).expect("config");
        let error = UpstreamFactory::build_with(
            &MantleFactory,
            &config,
            host.context(),
            &BuildContext::new(ProtocolId::BEDROCK_CONVERSE),
        )
        .map(drop)
        .expect_err("converse");
        assert!(
            error
                .to_string()
                .contains("bedrock-mantle does not serve protocol `bedrock-converse`"),
            "{error}"
        );
        let provider =
            UpstreamFactory::build(&MantleFactory, &config, host.context()).expect("default build");
        host.http().enqueue(CHAT_REPLY);
        chat(&provider, "openai.gpt-5.5").expect("chat");
        assert_eq!(uris(host), [format!("{OPENAI}/chat/completions")]);
    });
}
