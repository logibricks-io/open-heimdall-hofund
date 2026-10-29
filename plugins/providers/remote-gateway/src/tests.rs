use std::sync::Arc;

use hofund_kit_upstream::{TestHost, block_on};
use hofund_plugin_api::{
    BuildContext, CountOutcome, DynUpstreamFactory, ListKind, ListQuery, PluginId, ProbeQuery,
    ProtocolId, RawTable, RawValue, Request, UpstreamFactory, UpstreamProvider,
};
use hofund_plugin_sdk::json::{Value, json};

use super::{RemoteGatewayFactory, root};

const ROOT: &str = "http://192.168.1.20:3425";

const CHAT_REPLY: &str = r#"{"id":"c1","object":"chat.completion","model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#;
const RESPONSES_REPLY: &str = r#"{"id":"r1","object":"response","status":"completed","model":"m","output":[{"type":"message","id":"o1","role":"assistant","content":[{"type":"output_text","text":"ok"}]}]}"#;
const MESSAGES_REPLY: &str = r#"{"id":"m1","type":"message","role":"assistant","model":"m","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#;
const BILLING: &str = "x-anthropic-billing-header: cc_version=2.1.197; cc_entrypoint=cli; cch=abc;";

fn plugin() -> PluginId {
    PluginId::from_static("remote-gateway")
}

fn raw(base: &str) -> RawTable {
    let mut raw = RawTable::new();
    raw.insert("base_url", RawValue::String(base.to_owned()));
    raw
}

fn build(host: &TestHost, raw: &RawTable, protocol: ProtocolId) -> Arc<dyn UpstreamProvider> {
    DynUpstreamFactory::validate_and_build_with(
        &RemoteGatewayFactory,
        raw,
        host.context(),
        &BuildContext::new(protocol),
    )
    .expect("build")
}

fn with_host<T>(test: impl FnOnce(&TestHost) -> T) -> T {
    let root = tempfile::tempdir().expect("scratch");
    let host = TestHost::open(plugin(), root.path());
    host.secrets().insert(&plugin(), "api_key", "gw-test-key");
    test(&host)
}

fn user_request(model: &str) -> Request {
    let mut request: Request = hofund_plugin_sdk::json::from_value(json!({
        "model": model,
        "messages": [
            {"role": "system", "content": [{"type": "text", "text": BILLING}, {"type": "text", "text": "be brief"}]},
            {"role": "user", "content": [{"type": "text", "text": "hi"}]},
        ],
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

#[test]
fn the_address_is_put_at_the_gateway_root_however_it_was_typed() {
    for (typed, expected) in [
        ("192.168.1.20:3425", ROOT),
        ("  http://192.168.1.20:3425/  ", ROOT),
        ("http://192.168.1.20:3425/v1", ROOT),
        ("http://192.168.1.20:3425/v1/", ROOT),
        ("http://192.168.1.20:3425/v1/messages", ROOT),
        ("http://192.168.1.20:3425/v1/messages/count_tokens", ROOT),
        ("http://192.168.1.20:3425/v1/chat/completions", ROOT),
        ("http://192.168.1.20:3425/v1/responses", ROOT),
        ("http://192.168.1.20:3425/v1/systemone", ROOT),
        ("http://192.168.1.20:3425/v1/embeddings", ROOT),
        ("http://192.168.1.20:3425/v1/rerank", ROOT),
        ("http://192.168.1.20:3425/v1/audio/transcriptions", ROOT),
        ("http://192.168.1.20:3425/v1/audio/translations", ROOT),
        ("http://192.168.1.20:3425/v1/images/generations", ROOT),
        ("http://192.168.1.20:3425/v1/images/edits", ROOT),
        ("http://192.168.1.20:3425/v1/videos", ROOT),
        ("https://gw.example/team", "https://gw.example/team"),
        ("https://gw.example/team/v1", "https://gw.example/team"),
        // One suffix only, as the reference strips.
        ("https://gw.example/v1/v1", "https://gw.example/v1"),
        ("", ""),
        ("  / ", ""),
    ] {
        assert_eq!(root(typed), expected, "{typed:?}");
    }
}

#[test]
fn each_request_goes_on_in_the_protocol_the_client_spoke() {
    with_host(|host| {
        let provider = build(host, &raw("192.168.1.20:3425/v1"), ProtocolId::OPENAI_CHAT);
        host.http().enqueue(CHAT_REPLY);
        host.http().enqueue(RESPONSES_REPLY);
        host.http().enqueue(MESSAGES_REPLY);
        for inbound in [
            ProtocolId::OPENAI_CHAT,
            ProtocolId::OPENAI_RESPONSES,
            ProtocolId::ANTHROPIC_MESSAGES,
        ] {
            block_on(provider.execute(user_request("m"), inbound)).expect("execute");
        }
        let calls = host.http().calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].uri(), format!("{ROOT}/v1/chat/completions"));
        assert_eq!(calls[1].uri(), format!("{ROOT}/v1/responses"));
        assert_eq!(calls[2].uri(), format!("{ROOT}/v1/messages"));
        for call in &calls[..2] {
            assert!(has(call.sensitive_headers(), "authorization"));
            assert!(!has(call.sensitive_headers(), "x-api-key"));
        }
        let names = calls[2].sensitive_headers();
        assert!(
            has(names, "authorization") && has(names, "x-api-key"),
            "{names:?}"
        );
    });
}

#[test]
fn a_same_protocol_body_keeps_the_billing_line() {
    with_host(|host| {
        let provider = build(host, &raw(ROOT), ProtocolId::OPENAI_CHAT);
        host.http().enqueue(MESSAGES_REPLY);
        block_on(provider.execute(user_request("m"), ProtocolId::ANTHROPIC_MESSAGES))
            .expect("execute");
        let calls = host.http().calls();
        let body = String::from_utf8_lossy(calls[0].body()).into_owned();
        assert!(body.contains("x-anthropic-billing-header"), "{body}");
    });
}

#[test]
fn another_inbound_protocol_goes_on_the_built_protocol_once() {
    with_host(|host| {
        let chat = build(host, &raw(ROOT), ProtocolId::OPENAI_CHAT);
        let messages = build(host, &raw(ROOT), ProtocolId::ANTHROPIC_MESSAGES);
        let responses = build(host, &raw(ROOT), ProtocolId::OPENAI_RESPONSES);
        host.http().enqueue(CHAT_REPLY);
        host.http().enqueue(MESSAGES_REPLY);
        host.http().enqueue(RESPONSES_REPLY);
        for provider in [&chat, &messages, &responses] {
            block_on(provider.execute(user_request("m"), ProtocolId::GENERATIVE_LANGUAGE))
                .expect("execute");
        }
        let calls = host.http().calls();
        assert_eq!(calls[0].uri(), format!("{ROOT}/v1/chat/completions"));
        assert_eq!(calls[1].uri(), format!("{ROOT}/v1/messages"));
        assert_eq!(calls[2].uri(), format!("{ROOT}/v1/responses"));
        // Crossing protocols drops the billing line, as every upstream does.
        let body = String::from_utf8_lossy(calls[0].body()).into_owned();
        assert!(!body.contains("x-anthropic-billing-header"), "{body}");
        assert!(body.contains("be brief"), "{body}");
    });
}

#[test]
fn configured_headers_ride_on_every_request() {
    with_host(|host| {
        let mut config = raw(ROOT);
        let mut headers = RawTable::new();
        headers.insert("x-team", RawValue::String("blue".to_owned()));
        config.insert("headers", RawValue::Table(headers));
        let provider = build(host, &config, ProtocolId::OPENAI_CHAT);
        host.http().enqueue(CHAT_REPLY);
        block_on(provider.execute(user_request("m"), ProtocolId::OPENAI_CHAT)).expect("execute");
        host.http().enqueue(r#"{"data":[{"id":"gpt-5.5"}]}"#);
        block_on(provider.list_models(&ListQuery::new())).expect("list");
        host.http().enqueue("{}");
        block_on(provider.probe(&ProbeQuery::new(ProtocolId::OPENAI_CHAT, "m").expect("probe")))
            .expect("probe");
        for call in host.http().calls() {
            assert!(has(call.sensitive_headers(), "x-team"), "{}", call.uri());
        }
    });
}

#[test]
fn only_a_messages_request_counts_input() {
    with_host(|host| {
        let provider = build(host, &raw(ROOT), ProtocolId::OPENAI_CHAT);
        for inbound in [
            ProtocolId::OPENAI_CHAT,
            ProtocolId::OPENAI_RESPONSES,
            ProtocolId::GENERATIVE_LANGUAGE,
        ] {
            let outcome =
                block_on(provider.count_input(user_request("m"), inbound)).expect("count");
            assert!(matches!(outcome, CountOutcome::Unsupported));
        }
        assert!(host.http().calls().is_empty());
        host.http().enqueue(r#"{"input_tokens":7}"#);
        let outcome =
            block_on(provider.count_input(user_request("m"), ProtocolId::ANTHROPIC_MESSAGES))
                .expect("count");
        assert!(!matches!(outcome, CountOutcome::Unsupported));
        let messages = build(host, &raw(ROOT), ProtocolId::ANTHROPIC_MESSAGES);
        host.http().enqueue(r#"{"input_tokens":7}"#);
        let outcome =
            block_on(messages.count_input(user_request("m"), ProtocolId::GENERATIVE_LANGUAGE))
                .expect("count");
        assert!(!matches!(outcome, CountOutcome::Unsupported));
        let calls = host.http().calls();
        assert_eq!(calls.len(), 2);
        for call in &calls {
            assert_eq!(call.uri(), format!("{ROOT}/v1/messages/count_tokens"));
        }
    });
}

#[test]
fn the_list_is_the_gateway_v1_list_with_its_endpoints() {
    with_host(|host| {
        let provider = build(
            host,
            &raw("192.168.1.20:3425/v1/messages"),
            ProtocolId::ANTHROPIC_MESSAGES,
        );
        host.http().enqueue(
            r#"{"data":[{"id":"claude-sonnet-5","display_name":"Claude Sonnet 5","native_endpoints":["anthropic"],"context_length":200000},{"id":"text-embedding-3"}]}"#,
        );
        let list = block_on(provider.list_models(&ListQuery::new())).expect("list");
        assert_eq!(list.kind(), ListKind::Live);
        assert_eq!(
            list.source_url(),
            Some(format!("{ROOT}/v1/models").as_str())
        );
        let models = list.models();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id(), "claude-sonnet-5");
        assert_eq!(models[0].endpoints(), ["anthropic"]);
        assert_eq!(list.dropped(), ["text-embedding-3"]);
        let calls = host.http().calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].method(), "GET");
        assert_eq!(calls[0].uri(), format!("{ROOT}/v1/models"));
        assert_eq!(calls[0].sensitive_headers(), ["authorization"]);
    });
}

#[test]
fn a_configured_authorization_replaces_the_key_on_the_list_and_probe() {
    with_host(|host| {
        let mut config = raw(ROOT);
        let mut headers = RawTable::new();
        headers.insert("Authorization", RawValue::String("Bearer other".to_owned()));
        config.insert("headers", RawValue::Table(headers));
        let provider = build(host, &config, ProtocolId::OPENAI_CHAT);
        host.http().enqueue(r#"{"data":[{"id":"gpt-5.5"}]}"#);
        block_on(provider.list_models(&ListQuery::new())).expect("list");
        host.http().enqueue("{}");
        block_on(provider.probe(&ProbeQuery::new(ProtocolId::OPENAI_CHAT, "m").expect("probe")))
            .expect("probe");
        for call in host.http().calls() {
            assert_eq!(
                call.sensitive_headers(),
                ["authorization"],
                "{}",
                call.uri()
            );
        }
    });
}

#[test]
fn an_empty_key_sends_no_credential() {
    with_host(|host| {
        host.secrets().insert(&plugin(), "api_key", "");
        let provider = build(host, &raw(ROOT), ProtocolId::OPENAI_CHAT);
        host.http().enqueue(r#"{"data":[{"id":"gpt-5.5"}]}"#);
        block_on(provider.list_models(&ListQuery::new())).expect("list");
        host.http().enqueue("{}");
        block_on(provider.probe(&ProbeQuery::new(ProtocolId::OPENAI_CHAT, "m").expect("probe")))
            .expect("probe");
        for call in host.http().calls() {
            assert!(call.sensitive_headers().is_empty(), "{}", call.uri());
        }
    });
}

#[test]
fn a_list_error_hides_the_key_and_names_the_status() {
    with_host(|host| {
        host.secrets()
            .insert(&plugin(), "api_key", "gw-SENTINEL-KEY");
        let provider = build(host, &raw(ROOT), ProtocolId::OPENAI_CHAT);
        host.http().enqueue_response(
            401,
            &[],
            r#"{"error":{"message":"bad key gw-SENTINEL-KEY"}}"#,
        );
        let error = block_on(provider.list_models(&ListQuery::new())).expect_err("refused");
        assert_eq!(error.status(), Some(401));
        assert!(!error.to_string().contains("SENTINEL"), "{error}");
        assert!(error.to_string().contains("HTTP 401"), "{error}");
        host.http()
            .enqueue(r#"{"data":[{"id":"text-embedding-3"}]}"#);
        let error = block_on(provider.list_models(&ListQuery::new())).expect_err("empty");
        assert!(error.to_string().contains("no models listed"), "{error}");
    });
}

#[test]
fn a_configured_header_value_is_hidden_in_list_and_probe_errors() {
    with_host(|host| {
        let mut config = raw(ROOT);
        let mut headers = RawTable::new();
        headers.insert("x-team-token", RawValue::String("HDR-SENTINEL".to_owned()));
        config.insert("headers", RawValue::Table(headers));
        let provider = build(host, &config, ProtocolId::OPENAI_CHAT);
        host.http()
            .enqueue_response(401, &[], r#"{"error":{"message":"bad team HDR-SENTINEL"}}"#);
        let error = block_on(provider.list_models(&ListQuery::new())).expect_err("list refused");
        assert!(error.to_string().contains("HTTP 401"), "{error}");
        assert!(!error.to_string().contains("HDR-SENTINEL"), "{error}");
        host.http()
            .enqueue_response(403, &[], r#"{"error":{"message":"bad team HDR-SENTINEL"}}"#);
        let error = block_on(
            provider.probe(&ProbeQuery::new(ProtocolId::OPENAI_CHAT, "m").expect("probe")),
        )
        .map(drop)
        .expect_err("probe refused");
        assert_eq!(error.status(), Some(403));
        assert!(!error.to_string().contains("HDR-SENTINEL"), "{error}");
    });
}

#[test]
fn the_probe_asks_chat_at_the_gateway_v1() {
    with_host(|host| {
        let provider = build(host, &raw(ROOT), ProtocolId::ANTHROPIC_MESSAGES);
        host.http().enqueue("{}");
        block_on(provider.probe(&ProbeQuery::new(ProtocolId::OPENAI_CHAT, "m").expect("probe")))
            .expect("probe");
        host.http()
            .enqueue_response(403, &[], r#"{"error":{"message":"denied gw-test-key"}}"#);
        let error = block_on(
            provider.probe(&ProbeQuery::new(ProtocolId::OPENAI_CHAT, "m").expect("probe")),
        )
        .map(drop)
        .expect_err("refused");
        assert_eq!(error.status(), Some(403));
        assert!(!error.to_string().contains("gw-test-key"), "{error}");
        let calls = host.http().calls();
        assert_eq!(calls[0].uri(), format!("{ROOT}/v1/chat/completions"));
        assert_eq!(calls[0].sensitive_headers(), ["authorization"]);
        let body = sent(calls[0].body());
        assert_eq!(body["model"], "m");
        assert_eq!(body["max_completion_tokens"], 16);
    });
}

#[test]
fn config_and_build_refuse_what_they_cannot_use() {
    with_host(|host| {
        let factory = RemoteGatewayFactory;
        let mut extra = raw(ROOT);
        extra.insert("region", RawValue::String("x".to_owned()));
        assert!(UpstreamFactory::validate(&factory, &extra).is_err());
        let mut number = RawTable::new();
        number.insert("base_url", RawValue::Integer(1));
        assert!(UpstreamFactory::validate(&factory, &number).is_err());
        let mut headers = raw(ROOT);
        headers.insert("headers", RawValue::String("x".to_owned()));
        assert!(UpstreamFactory::validate(&factory, &headers).is_err());
        let mut values = raw(ROOT);
        let mut table = RawTable::new();
        table.insert("x-team", RawValue::Integer(1));
        values.insert("headers", RawValue::Table(table));
        assert!(UpstreamFactory::validate(&factory, &values).is_err());
        assert!(UpstreamFactory::validate(&factory, &RawTable::new()).is_ok());
        for (config, protocol) in [
            (raw(ROOT), ProtocolId::GENERATIVE_LANGUAGE),
            (raw("  "), ProtocolId::OPENAI_CHAT),
            (RawTable::new(), ProtocolId::OPENAI_CHAT),
        ] {
            assert!(
                DynUpstreamFactory::validate_and_build_with(
                    &factory,
                    &config,
                    host.context(),
                    &BuildContext::new(protocol),
                )
                .is_err()
            );
        }
        // The typed entry refuses an unserved protocol itself, not only
        // behind the host's checked entry.
        let config = UpstreamFactory::validate(&factory, &raw(ROOT)).expect("valid");
        assert!(
            UpstreamFactory::build_with(
                &factory,
                &config,
                host.context(),
                &BuildContext::new(ProtocolId::GENERATIVE_LANGUAGE),
            )
            .is_err()
        );
        // `build` is Chat.
        let provider = DynUpstreamFactory::validate_and_build(&factory, &raw(ROOT), host.context())
            .expect("build");
        host.http().enqueue(CHAT_REPLY);
        block_on(provider.execute(user_request("m"), ProtocolId::GENERATIVE_LANGUAGE))
            .expect("execute");
        assert_eq!(
            host.http().calls()[0].uri(),
            format!("{ROOT}/v1/chat/completions")
        );
    });
}

#[test]
fn the_spec_serves_three_protocols_with_a_bearer_key_and_a_live_list() {
    let spec = UpstreamFactory::spec(&RemoteGatewayFactory);
    assert_eq!(spec.plugin().as_str(), "remote-gateway");
    assert_eq!(
        spec.credential(),
        hofund_plugin_api::CredentialStyle::Bearer
    );
    assert_eq!(
        spec.protocols(),
        [
            ProtocolId::OPENAI_CHAT,
            ProtocolId::OPENAI_RESPONSES,
            ProtocolId::ANTHROPIC_MESSAGES,
        ]
    );
    assert_eq!(spec.list_kind(), ListKind::Live);
}
