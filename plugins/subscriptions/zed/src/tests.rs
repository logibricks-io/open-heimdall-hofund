//! Unit tests against recording host backends; no network.

use std::sync::Arc;

use hofund_kit_subscription::{TestHost, block_on};
use hofund_plugin_api::{
    AccountId, Completion, ContentPart, PluginErrorKind, PluginId, PluginKind, ProbeQuery,
    ProtocolId, RawTable, Request, SecretString, Subscription, SubscriptionFactory,
    UpstreamProvider,
};
use hofund_plugin_sdk::json::{self, json};
use hofund_plugin_sdk::oauth::{SecretObject, secret_text};

use crate::body;
use crate::creds::{self, Credential};
use crate::models;
use crate::{Endpoints, factory_with_endpoints};

use std::sync::atomic::{AtomicUsize, Ordering};

const CLOUD: &str = "https://zed.test";
const ACCESS: &str = "zed-access-test";
static SEED_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn endpoints() -> Endpoints {
    Endpoints {
        cloud: CLOUD.to_owned(),
    }
}

fn host(dir: &tempfile::TempDir) -> TestHost {
    TestHost::open_for(
        PluginKind::Subscription,
        PluginId::from_static("zed"),
        dir.path(),
    )
}

fn subscription(host: &TestHost) -> Arc<dyn Subscription> {
    let factory = factory_with_endpoints(endpoints());
    let cfg = factory.validate(&RawTable::new()).expect("config");
    factory.build(&cfg, host.context()).expect("build")
}

fn seed(host: &TestHost) -> AccountId {
    seed_named(host, "default")
}

fn seed_named(host: &TestHost, name: &str) -> AccountId {
    let n = SEED_COUNTER.fetch_add(1, Ordering::Relaxed);
    let user_id = format!("u-{name}-{n}");
    let credential = Credential {
        user_id,
        access: SecretString::from(format!("{ACCESS}-{n}")),
        system_id: "sys".to_owned(),
        org: "org1".to_owned(),
        login: "zed-user".to_owned(),
        name: String::new(),
        plan: String::new(),
        models_json: include_str!("fixtures/models.json").to_owned(),
    };
    let id = creds::account_id(&credential);
    block_on(async {
        let accounts = host.context().accounts();
        let account = accounts
            .account(id.clone(), "zed-user".to_owned())
            .expect("account");
        accounts
            .upsert(account, creds::secret_blob(&credential).to_secret())
            .await
            .expect("upsert");
    });
    id
}

fn seed_with_models_json(host: &TestHost, name: &str, models_json: &str) -> AccountId {
    let n = SEED_COUNTER.fetch_add(1, Ordering::Relaxed);
    let user_id = format!("u-{name}-{n}");
    let credential = Credential {
        user_id,
        access: SecretString::from(format!("{ACCESS}-{n}")),
        system_id: "sys".to_owned(),
        org: "org1".to_owned(),
        login: "zed-user".to_owned(),
        name: String::new(),
        plan: String::new(),
        models_json: models_json.to_owned(),
    };
    let id = creds::account_id(&credential);
    block_on(async {
        let accounts = host.context().accounts();
        let account = accounts
            .account(id.clone(), "zed-user".to_owned())
            .expect("account");
        accounts
            .upsert(account, creds::secret_blob(&credential).to_secret())
            .await
            .expect("upsert");
    });
    models::parse_stored_models(&creds::Stored {
        credential: credential.clone(),
    })
    .expect("parse models");
    id
}

fn enqueue_model_token(host: &TestHost) {
    host.http().enqueue_response(
        200,
        &[("content-type", "application/json")],
        br#"{"token":"model-token-test"}"#,
    );
}

fn enqueue_wrapped_completion(host: &TestHost, body: &[u8]) {
    host.http().enqueue_response(
        200,
        &[
            ("content-type", "application/json"),
            ("x-zed-server-supports-status-messages", "true"),
        ],
        body,
    );
}

fn upstream_for(host: &TestHost, id: &AccountId) -> Arc<dyn UpstreamProvider> {
    subscription(host)
        .upstream(id, host.context())
        .expect("upstream")
}

#[test]
fn a2c_14_import_fixture_credentials() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/fixtures/credential.json"
    ))
    .expect("read");
    let ids = block_on(subscription(&host).import_accounts(&text, host.context())).expect("import");
    assert_eq!(ids.len(), 1);
    let secret = block_on(host.context().accounts().secret(&ids[0]))
        .expect("read")
        .expect("stored");
    assert_eq!(
        SecretObject::parse(&secret)
            .expect("blob")
            .secret("access_token")
            .as_ref()
            .map(secret_text),
        Some("zed-access-fixture")
    );
}

#[test]
fn a2c_15_refresh_mints_llm_token() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_named(&host, "refresh");
    host.http().enqueue(br#"{"token":"model-token-1"}"#);
    block_on(subscription(&host).refresh(&id, host.context())).expect("refresh");
    assert!(host.http().calls()[0].uri().contains("/client/llm_tokens"));
}

#[test]
fn a2c_16_model_list_parsing() {
    let list = models::parse_list(include_bytes!("fixtures/models.json")).expect("parse");
    assert!(list.models().iter().any(|m| m.id() == "claude-sonnet-4"));
}

#[test]
fn a2c_16b_model_list_display_name() {
    let body = br#"{"models":[{"id":"m1","display_name":"Pretty","provider":"anthropic"}]}"#;
    let list = models::parse_list(body).expect("parse");
    assert_eq!(list.models()[0].display_name(), Some("Pretty"));
}

#[test]
fn a2c_17_models_mint_401_is_auth() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_named(&host, "models-401");
    host.http().enqueue_response(
        401,
        &[("content-type", "application/json")],
        br#"{"message":"unauthorized"}"#,
    );
    let err = block_on(subscription(&host).list_models(&id, host.context())).expect_err("auth");
    assert_eq!(err.kind(), PluginErrorKind::Auth);
}

#[test]
fn a2c_18_upstream_posts_completions() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host);
    host.http().enqueue(br#"{"token":"model-token-1"}"#);
    block_on(subscription(&host).refresh(&id, host.context())).expect("refresh");
    host.http().enqueue(br#"{"token":"model-token-2"}"#);
    let (headers, body) = completion_ok_anthropic();
    host.http().enqueue_response(200, headers, body);
    let upstream = subscription(&host)
        .upstream(&id, host.context())
        .expect("upstream");
    let query = ProbeQuery::new(ProtocolId::OPENAI_RESPONSES, "claude-sonnet-4").expect("query");
    block_on(upstream.probe(&query)).expect("probe");
    assert!(
        host.http()
            .calls()
            .iter()
            .any(|call| call.uri().contains("/completions"))
    );
}

#[test]
fn a2c_18_secrets_not_in_debug() {
    let key = "zed-secret-access-token";
    let credential = Credential {
        user_id: "u".to_owned(),
        access: SecretString::from(key.to_owned()),
        system_id: String::new(),
        org: String::new(),
        login: String::new(),
        name: String::new(),
        plan: String::new(),
        models_json: "[]".to_owned(),
    };
    assert!(!format!("{credential:?}").contains(key));
}

#[test]
fn a2c_19_body_rewrites_developer_role() {
    let out =
        body::transform(br#"{"input":[{"type":"message","role":"developer","content":"hi"}]}"#);
    let text = std::str::from_utf8(&out).expect("utf8");
    assert!(text.contains("system"));
}

#[test]
fn a2c_19b_body_keeps_string_input() {
    let body = br#"{"input":"plain prompt","model":"m"}"#;
    let out = body::transform(body);
    assert_eq!(out, body);
}

#[test]
fn a2c_19c_body_flattens_namespace_tools() {
    let body = br#"{"input":[],"tools":[{"type":"namespace","name":"nico","tools":[{"type":"function","name":"ping","parameters":{"type":"object"}}]}]}"#;
    let out = body::transform(body);
    let text = std::str::from_utf8(&out).expect("utf8");
    assert!(text.contains("nico__ping"));
}

fn completion_ok_anthropic() -> (&'static [(&'static str, &'static str)], &'static [u8]) {
    (
        &[
            ("content-type", "application/json"),
            ("x-zed-server-supports-status-messages", "true"),
        ],
        br#"{"event":{"type":"message_start","message":{"id":"m1","type":"message","role":"assistant","content":[]}}}
{"event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}}
{"event":{"type":"message_stop"}}
{"status":"stream_ended"}
"#,
    )
}

fn last_completion_body(host: &TestHost) -> hofund_plugin_sdk::json::Value {
    let calls = host.http().calls();
    let call = calls
        .iter()
        .rev()
        .find(|call| call.uri().contains("/completions"))
        .expect("completions");
    hofund_plugin_sdk::json::from_slice(call.body()).expect("json")
}

#[test]
fn a2c_20_envelope_fields_and_anthropic_provider_request() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host);
    host.http().enqueue(br#"{"token":"model-token-1"}"#);
    let (headers, body) = completion_ok_anthropic();
    host.http().enqueue_response(200, headers, body);
    let upstream = subscription(&host)
        .upstream(&id, host.context())
        .expect("upstream");
    let query = ProbeQuery::new(ProtocolId::OPENAI_RESPONSES, "claude-sonnet-4").expect("query");
    block_on(upstream.probe(&query)).expect("probe");
    let envelope = last_completion_body(&host);
    assert_eq!(
        envelope.get("provider").and_then(|v| v.as_str()),
        Some("anthropic")
    );
    assert_eq!(
        envelope.get("model").and_then(|v| v.as_str()),
        Some("claude-sonnet-4")
    );
    let pr = envelope
        .get("provider_request")
        .and_then(|v| v.as_object())
        .expect("provider_request");
    assert!(pr.contains_key("messages"));
    assert!(!pr.contains_key("input"));
}

#[test]
fn a2c_21_bare_ndjson_completions_stream() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host);
    host.http().enqueue(br#"{"token":"model-token-1"}"#);
    host.http().enqueue_response(
        200,
        &[("content-type", "application/json")],
        br#"{"type":"message_start","message":{"id":"m1","type":"message","role":"assistant","content":[]}}
{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}
{"type":"message_stop"}
"#,
    );
    let upstream = subscription(&host)
        .upstream(&id, host.context())
        .expect("upstream");
    let query = ProbeQuery::new(ProtocolId::ANTHROPIC_MESSAGES, "claude-sonnet-4").expect("query");
    block_on(upstream.probe(&query)).expect("bare ndjson");
}

#[test]
fn a2c_22_models_expired_header_re_mints() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_named(&host, "expired-hdr");
    host.http().enqueue(br#"{"token":"tok-a"}"#);
    host.http().enqueue_response(
        200,
        &[("x-zed-expired-token", "true")],
        br#"{"models":[{"id":"m1","provider":"anthropic"}]}"#,
    );
    host.http().enqueue(br#"{"token":"tok-b"}"#);
    host.http().enqueue_response(
        200,
        &[],
        br#"{"models":[{"id":"m1","provider":"anthropic","display_name":"One"}]}"#,
    );
    let list = block_on(subscription(&host).list_models(&id, host.context())).expect("list");
    assert_eq!(list.models()[0].id(), "m1");
    let calls = host.http().calls();
    assert_eq!(
        calls
            .iter()
            .filter(|c| c.uri().contains("llm_tokens"))
            .count(),
        2
    );
}

#[test]
fn a2c_23_models_list_401_after_re_mint_is_status_not_auth() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_named(&host, "models-twice-401");
    host.http().enqueue(br#"{"token":"tok-a"}"#);
    host.http()
        .enqueue_response(401, &[], br#"{"message":"nope"}"#);
    host.http().enqueue(br#"{"token":"tok-b"}"#);
    host.http()
        .enqueue_response(401, &[], br#"{"message":"nope"}"#);
    let err = block_on(subscription(&host).list_models(&id, host.context())).expect_err("401");
    assert_ne!(err.kind(), PluginErrorKind::Auth);
}

#[test]
fn a2c_24_models_402_repick_persists_org() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_named(&host, "repick-402");
    host.http().enqueue(br#"{"token":"tok"}"#);
    host.http()
        .enqueue_response(402, &[], br#"{"message":"pay"}"#);
    let me = br#"{"organizations":[{"id":"org1","is_personal":true},{"id":"org2","is_personal":false}],"default_organization_id":"org1","plans_by_organization":{"org1":"zed_free","org2":"zed_pro"}}"#;
    host.http().enqueue_response(200, &[], me);
    host.http().enqueue(br#"{"token":"tok2"}"#);
    host.http().enqueue_response(
        200,
        &[],
        br#"{"models":[{"id":"m1","provider":"anthropic"}]}"#,
    );
    block_on(subscription(&host).list_models(&id, host.context())).expect("list");
    let secret = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    let blob = SecretObject::parse(&secret).expect("blob");
    let org = blob.text("org").expect("org");
    assert_eq!(org, "org2");
}

#[test]
fn a2c_25_live_list_failure_uses_snapshot() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_named(&host, "snapshot");
    host.http().enqueue(br#"{"token":"tok"}"#);
    host.http().enqueue_response(502, &[], b"down");
    let list = block_on(subscription(&host).list_models(&id, host.context())).expect("snapshot");
    assert!(list.models().iter().any(|m| m.id() == "claude-sonnet-4"));
}

#[test]
fn a2c_26_completion_headers_user_agent_and_version() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host);
    host.http().enqueue(br#"{"token":"model-token-1"}"#);
    let (headers, body) = completion_ok_anthropic();
    host.http().enqueue_response(200, headers, body);
    let upstream = subscription(&host)
        .upstream(&id, host.context())
        .expect("upstream");
    let query = ProbeQuery::new(ProtocolId::ANTHROPIC_MESSAGES, "claude-sonnet-4").expect("query");
    block_on(upstream.probe(&query)).expect("probe");
    let calls = host.http().calls();
    let call = calls
        .iter()
        .find(|call| call.uri().contains("/completions"))
        .expect("completions");
    let headers = call.headers();
    assert!(
        headers
            .iter()
            .any(|(n, v)| n.eq_ignore_ascii_case("user-agent") && v.starts_with("Zed/"))
    );
    assert!(
        headers
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case("x-zed-version"))
    );
    assert!(!headers.iter().any(|(n, _)| n == "Accept"));
}

#[test]
fn a2c_27_quota_users_me_plan_window() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_named(&host, "quota-me");
    let me = br#"{"organizations":[{"id":"org1"}],"plans_by_organization":{"org1":"zed_pro"},"plan":{"subscription_period":{"ended_at":"2030-01-01T00:00:00Z"}}}"#;
    host.http().enqueue_response(200, &[], me);
    let windows = block_on(subscription(&host).quota(&id, host.context())).expect("quota");
    assert!(!windows.is_empty());
}

#[test]
fn a2c_28_quota_403_is_not_sign_in_expired() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_named(&host, "quota-403");
    host.http()
        .enqueue_response(403, &[], br#"{"message":"forbidden"}"#);
    let err = block_on(subscription(&host).quota(&id, host.context())).expect_err("403");
    assert_ne!(err.kind(), PluginErrorKind::Auth);
}

#[test]
fn a2c_29_supports_images_gate_strips_input_image() {
    use hofund_plugin_sdk::json::{Map, Value, json};
    let mut root: Map<String, Value> = hofund_plugin_sdk::json::from_value(json!({
        "input":[{"type":"message","role":"user","content":[
            {"type":"input_text","text":"hi"},
            {"type":"input_image","image_url":"x"}
        ]}]
    }))
    .expect("map");
    body::prepare_provider(
        body::Wire::Responses,
        &mut root,
        "gpt-x",
        models::Capabilities {
            supports_images: false,
            supports_thinking: false,
        },
    );
    let text = hofund_plugin_sdk::json::to_string(&Value::Object(root)).expect("json");
    assert!(!text.contains("input_image"));
}

#[test]
fn a2c_30_gemini_unknown_tool_mode_lowercased() {
    use hofund_plugin_sdk::json::{Map, Value, json};
    let mut root: Map<String, Value> = hofund_plugin_sdk::json::from_value(json!({
        "toolConfig":{"functionCallingConfig":{"mode":"CUSTOM_MODE"}}
    }))
    .expect("map");
    body::prepare_provider(
        body::Wire::Gemini,
        &mut root,
        "gemini-3",
        models::Capabilities::default(),
    );
    assert_eq!(
        root.get("toolConfig")
            .and_then(|tc| tc.get("functionCallingConfig"))
            .and_then(|fc| fc.get("mode"))
            .and_then(|m| m.as_str()),
        Some("custom_mode")
    );
}

#[test]
fn a2c_31_wrapped_failure_before_content_is_status_error() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host);
    host.http().enqueue_response(
        200,
        &[("content-type", "application/json")],
        br#"{"token":"tok"}"#,
    );
    host.http().enqueue_response(
        200,
        &[("x-zed-server-supports-status-messages", "true")],
        br#"{"status":{"failed":{"code":"upstream_http_429","message":"slow down"}}}
"#,
    );
    let upstream = subscription(&host)
        .upstream(&id, host.context())
        .expect("upstream");
    let query = ProbeQuery::new(ProtocolId::ANTHROPIC_MESSAGES, "claude-sonnet-4").expect("query");
    let err = block_on(upstream.probe(&query)).expect_err("429");
    assert_eq!(err.kind(), PluginErrorKind::Status);
}

#[test]
fn a2c_32_responses_stream_flag_on_provider_request() {
    use hofund_plugin_api::Request;
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let credential = Credential {
        user_id: "u-stream".to_owned(),
        access: SecretString::from(format!("{ACCESS}-stream")),
        system_id: String::new(),
        org: "org1".to_owned(),
        login: String::new(),
        name: String::new(),
        plan: String::new(),
        models_json: r#"{"models":[{"id":"gpt-5","provider":"open_ai"}]}"#.to_owned(),
    };
    let id = creds::account_id(&credential);
    block_on(async {
        host.context()
            .accounts()
            .upsert(
                host.context()
                    .accounts()
                    .account(id.clone(), "u".to_owned())
                    .expect("account"),
                creds::secret_blob(&credential).to_secret(),
            )
            .await
            .expect("upsert");
    });
    models::parse_stored_models(&creds::Stored { credential }).expect("parse");
    host.http().enqueue(br#"{"token":"tok"}"#);
    host.http().enqueue_response(
        200,
        &[("x-zed-server-supports-status-messages", "true")],
        br#"{"event":{"type":"response.created","response":{"id":"r1","object":"response","status":"in_progress"}}}
{"event":{"type":"response.output_text.delta","delta":"x","output_index":0,"content_index":0}}
{"event":{"type":"response.completed","response":{"id":"r1","object":"response","status":"completed","output":[]}}}
{"status":"stream_ended"}
"#,
    );
    let upstream = subscription(&host)
        .upstream(&id, host.context())
        .expect("upstream");
    let mut request = Request::new("gpt-5");
    request.stream = true;
    block_on(upstream.execute(request, ProtocolId::OPENAI_RESPONSES)).expect("execute");
    let stream = last_completion_body(&host)
        .get("provider_request")
        .and_then(|pr| pr.get("stream"))
        .and_then(|v| v.as_bool());
    assert_eq!(stream, Some(true));
}

#[test]
fn a2c_33_capabilities_default_false_without_flags() {
    let stored = creds::Stored {
        credential: Credential {
            user_id: "u".to_owned(),
            access: SecretString::from("a".to_owned()),
            system_id: String::new(),
            org: String::new(),
            login: String::new(),
            name: String::new(),
            plan: String::new(),
            models_json: r#"{"models":[{"id":"m","provider":"anthropic"}]}"#.to_owned(),
        },
    };
    let caps = models::capabilities(&stored, "m");
    assert!(!caps.supports_images);
    assert!(!caps.supports_thinking);
}

#[test]
fn a2c_34_tools_execute_round_trip_ids_name_and_arguments() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let models_json = r#"{"models":[{"id":"gpt-5","provider":"open_ai","supports_images":false}]}"#;
    let id = seed_with_models_json(&host, "tools-e2e", models_json);
    enqueue_model_token(&host);
    enqueue_wrapped_completion(
        &host,
        br#"{"event":{"type":"response.created","response":{"id":"resp_tools","object":"response","status":"in_progress","output":[]}}}
{"event":{"type":"response.completed","response":{"id":"resp_tools","object":"response","status":"completed","output":[{"type":"function_call","call_id":"call_weather_1","name":"get_weather","arguments":"{\"city\":\"Paris\"}"}],"usage":{"input_tokens":4,"output_tokens":6,"total_tokens":10}}}}
{"status":"stream_ended"}
"#,
    );
    let request: Request = json::from_value(json!({
        "model": "gpt-5",
        "stream": false,
        "messages": [{"role": "user", "content": [{"type": "text", "text": "Weather in Paris?"}]}],
        "tools": [{
            "name": "get_weather",
            "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}}
        }]
    }))
    .expect("request");
    let completion =
        block_on(upstream_for(&host, &id).execute(request, ProtocolId::OPENAI_RESPONSES))
            .expect("execute");
    let envelope = last_completion_body(&host);
    let tool_decl = envelope
        .get("provider_request")
        .and_then(|pr| pr.get("tools"))
        .and_then(|t| t.as_array())
        .and_then(|items| items.first())
        .expect("tool declaration");
    assert_eq!(
        tool_decl.get("type").and_then(|v| v.as_str()),
        Some("function")
    );
    assert_eq!(
        tool_decl.get("name").and_then(|v| v.as_str()),
        Some("get_weather")
    );
    let Completion::Unary(response) = completion else {
        panic!("expected unary completion");
    };
    let tool = response
        .message
        .content
        .iter()
        .find_map(|part| match part {
            ContentPart::ToolCall(call) => Some(call),
            _ => None,
        })
        .expect("tool call part");
    assert_eq!(tool.id, "call_weather_1");
    assert_eq!(tool.name, "get_weather");
    let args = tool.arguments.as_object().expect("json arguments");
    assert_eq!(args.get("city").and_then(|v| v.as_str()), Some("Paris"));
}

#[test]
fn a2c_35_thinking_enabled_when_catalog_allows() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let models_json =
        r#"{"models":[{"id":"claude-sonnet-4","provider":"anthropic","supports_thinking":true}]}"#;
    let id = seed_with_models_json(&host, "think-on", models_json);
    enqueue_model_token(&host);
    let (headers, body) = completion_ok_anthropic();
    host.http().enqueue_response(200, headers, body);
    let mut request: Request = json::from_value(json!({
        "model": "claude-sonnet-4",
        "stream": true,
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
    }))
    .expect("request");
    request
        .extensions
        .insert("thinking", json!({"type":"enabled","budget_tokens":1024}));
    block_on(upstream_for(&host, &id).execute(request, ProtocolId::ANTHROPIC_MESSAGES))
        .expect("execute");
    let envelope = last_completion_body(&host);
    assert!(
        envelope
            .get("provider_request")
            .and_then(|pr| pr.get("thinking"))
            .is_some(),
        "thinking block should remain when supported"
    );
}

#[test]
fn a2c_35b_thinking_stripped_when_catalog_disallows() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let models_json =
        r#"{"models":[{"id":"claude-sonnet-4","provider":"anthropic","supports_thinking":false}]}"#;
    let id = seed_with_models_json(&host, "think-off", models_json);
    enqueue_model_token(&host);
    let (headers, body) = completion_ok_anthropic();
    host.http().enqueue_response(200, headers, body);
    let mut request: Request = json::from_value(json!({
        "model": "claude-sonnet-4",
        "stream": true,
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
    }))
    .expect("request");
    request
        .extensions
        .insert("thinking", json!({"type":"enabled","budget_tokens":1024}));
    block_on(upstream_for(&host, &id).execute(request, ProtocolId::ANTHROPIC_MESSAGES))
        .expect("execute");
    let envelope = last_completion_body(&host);
    let pr = envelope.get("provider_request").expect("provider_request");
    assert!(
        pr.get("thinking").is_none(),
        "thinking must be stripped when supports_thinking is false"
    );
}

#[test]
fn a2c_36_unary_fold_preserves_text_and_usage() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let models_json = r#"{"models":[{"id":"gpt-5","provider":"open_ai"}]}"#;
    let id = seed_with_models_json(&host, "unary-fold", models_json);
    enqueue_model_token(&host);
    enqueue_wrapped_completion(
        &host,
        br#"{"event":{"type":"response.created","response":{"id":"resp_fold","object":"response","status":"in_progress","output":[]}}}
{"event":{"type":"response.output_item.added","output_index":0,"item":{"type":"message","role":"assistant","content":[]}}}
{"event":{"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"Hel"}}
{"event":{"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"lo"}}
{"event":{"type":"response.completed","response":{"id":"resp_fold","object":"response","status":"completed","output":[],"usage":{"input_tokens":11,"output_tokens":7,"total_tokens":18}}}}
{"status":"stream_ended"}
"#,
    );
    let request: Request = json::from_value(json!({
        "model": "gpt-5",
        "stream": false,
        "messages": [{"role": "user", "content": [{"type": "text", "text": "Say hi"}]}],
    }))
    .expect("request");
    let completion =
        block_on(upstream_for(&host, &id).execute(request, ProtocolId::OPENAI_RESPONSES))
            .expect("execute");
    let Completion::Unary(response) = completion else {
        panic!("expected unary folded completion");
    };
    let text: String = response
        .message
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Hello");
    assert_eq!(response.id.as_deref(), Some("resp_fold"));
    let usage = response.usage.expect("usage");
    assert_eq!(usage.input_tokens, Some(11));
    assert_eq!(usage.output_tokens, Some(7));
}
