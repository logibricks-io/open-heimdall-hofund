//! Deterministic tests with a recording HTTP backend.

use std::sync::Arc;

use hofund_kit_subscription::{TestHost, block_on};
use hofund_plugin_api::{
    AccountId, PluginErrorKind, PluginId, PluginKind, ProbeQuery, ProtocolId, RawTable,
    SecretString, SignInChallenge, SignInCtx, SignInProof, Subscription, SubscriptionFactory,
};
use hofund_plugin_sdk::json::json;
use hofund_plugin_sdk::oauth::SecretObject;
use hofund_plugin_sdk::oauth::secret_text;

use crate::models;
use crate::quota;
use crate::tokens::{self, AuthKind};
use crate::{Endpoints, factory_with_endpoints};

const SESSION: &str = "0123456789abcdef0123456789abcdef";
const API: &str = "https://factory.test";

fn endpoints() -> Endpoints {
    Endpoints {
        api: API.to_owned(),
        api_eu: "https://factory-eu.test".to_owned(),
        workos_device: "https://workos.test/authorize/device".to_owned(),
        workos_token: "https://workos.test/authenticate".to_owned(),
    }
}

fn plugin() -> PluginId {
    PluginId::from_static("factory")
}

fn host(dir: &tempfile::TempDir) -> TestHost {
    TestHost::open_for(PluginKind::Subscription, plugin(), dir.path())
}

fn subscription(host: &TestHost) -> Arc<dyn Subscription> {
    let factory = factory_with_endpoints(endpoints());
    let cfg = factory.validate(&RawTable::new()).expect("config");
    factory.build(&cfg, host.context()).expect("build")
}

fn ctx() -> SignInCtx {
    SignInCtx::new(plugin())
        .with_session(SESSION)
        .expect("session")
}

fn secret(text: &str) -> SecretString {
    SecretString::from(text.to_owned())
}

fn device_auth_body() -> Vec<u8> {
    json!({
        "device_code": "dev-secret-code",
        "user_code": "WDJB-MJHT",
        "verification_uri": "https://app.factory.ai/device",
        "verification_uri_complete": "https://app.factory.ai/device?code=WDJB-MJHT",
        "expires_in": 600,
        "interval": 1
    })
    .to_string()
    .into_bytes()
}

fn seed_oauth(host: &TestHost) -> AccountId {
    let id = AccountId::from_static("a-0000000000000001");
    let blob = SecretObject::new()
        .with_number("v", 1)
        .with_text("auth_type", AuthKind::Oauth.as_str())
        .with_secret("access_token", &secret("factory-access-1"))
        .with_secret("refresh_token", &secret("factory-refresh-1"))
        .with_number("expires_ms", 4_000_000_000_000)
        .with_text("email", "user@factory.test");
    block_on(async {
        let accounts = host.context().accounts();
        let account = accounts
            .account(id.clone(), "user@factory.test")
            .expect("account");
        accounts
            .upsert(account, blob.to_secret())
            .await
            .expect("upsert");
    });
    id
}

#[test]
fn a2b_12_device_sign_in_polls_workos_then_registers() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    host.http().enqueue(device_auth_body());
    let challenge = block_on(sub.start_sign_in(&ctx(), host.context())).expect("start");
    let SignInChallenge::DeviceCode { user_code, .. } = challenge else {
        panic!("device challenge");
    };
    assert_eq!(user_code, "WDJB-MJHT");

    host.http().enqueue(
        br#"{"access_token":"wo-access","refresh_token":"wo-refresh","token_type":"Bearer","expires_in":3600}"#
            .to_vec(),
    );
    host.http().enqueue(
        json!({"email": "user@factory.test", "userId": "uid-1", "orgId": "org-1", "region": "eu", "premBaseHostV2": "prem.factory.test/"})
            .to_string()
            .into_bytes(),
    );
    let id = block_on(sub.complete_sign_in(&ctx(), SignInProof::Await, host.context()))
        .expect("complete");
    let blob = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    let parsed = SecretObject::parse(&blob).expect("blob");
    assert_eq!(parsed.text("auth_type"), Some("oauth"));
    assert!(parsed.secret("refresh_token").is_some());
    assert_eq!(parsed.text("region"), Some("eu"));
    assert_eq!(parsed.text("active_organization_id"), Some("org-1"));
    assert_eq!(parsed.text("prem_base_host"), Some("prem.factory.test/"));
    let calls = host.http().calls();
    assert!(
        calls
            .iter()
            .any(|call| call.uri().contains("/api/cli/whoami"))
    );
    assert!(
        calls
            .iter()
            .take(2)
            .all(|call| { String::from_utf8_lossy(call.body()).contains(crate::WORKOS_CLIENT_ID) })
    );
}

#[test]
fn a2b_13_refresh_rotates_tokens_via_workos() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_oauth(&host);
    host.http().enqueue(
        br#"{"access_token":"factory-access-2","refresh_token":"factory-refresh-2","expires_in":3600,"token_type":"Bearer"}"#
            .to_vec(),
    );
    host.http().enqueue(
        json!({"orgId": "org-next", "region": "eu", "premBaseHostV2": "prem-next.factory.test"})
            .to_string()
            .into_bytes(),
    );
    block_on(tokens::refresh_for_test(
        host.context().clone(),
        endpoints(),
        id.clone(),
        secret("factory-access-1"),
    ))
    .expect("refresh");
    let blob = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    let parsed = SecretObject::parse(&blob).expect("blob");
    assert_eq!(
        parsed.secret("access_token").as_ref().map(secret_text),
        Some("factory-access-2")
    );
    assert_eq!(
        parsed.secret("refresh_token").as_ref().map(secret_text),
        Some("factory-refresh-2")
    );
    assert_eq!(parsed.text("active_organization_id"), Some("org-next"));
    assert_eq!(parsed.text("region"), Some("eu"));
    assert_eq!(
        parsed.text("prem_base_host"),
        Some("prem-next.factory.test")
    );
    let calls = host.http().calls();
    assert!(calls.iter().any(|call| {
        call.uri().contains("workos.test/authenticate")
            && String::from_utf8_lossy(call.body()).contains(crate::WORKOS_CLIENT_ID)
    }));
    assert!(
        calls
            .last()
            .is_some_and(|call| call.uri().contains("factory.test/api/cli/whoami"))
    );
}

fn check_transient_refresh(status: Option<u16>) {
    let dir = tempfile::tempdir().expect("dir");
    let host = TestHost::open_for_at(
        PluginKind::Subscription,
        plugin(),
        dir.path(),
        std::time::UNIX_EPOCH + std::time::Duration::from_millis(4_000_000_000_000 - 60_000),
    );
    let id = seed_oauth(&host);
    match status {
        Some(status) => {
            host.http()
                .enqueue_response(status, &[], b"temporarily unavailable".to_vec())
        }
        None => host.http().enqueue_connect("offline"),
    }
    let bearer = block_on(tokens::bearer(host.context(), &endpoints(), &id))
        .expect("request-time refresh keeps an unexpired bearer");
    assert_eq!(secret_text(&bearer.token), "factory-access-1");

    match status {
        Some(status) => {
            host.http()
                .enqueue_response(status, &[], b"temporarily unavailable".to_vec())
        }
        None => host.http().enqueue_connect("offline"),
    }
    let error = block_on(tokens::refresh_account(host.context(), &endpoints(), &id))
        .expect_err("explicit refresh reports transient failure");
    if let Some(status) = status {
        assert_eq!(error.status(), Some(status));
    } else {
        assert_eq!(error.kind(), PluginErrorKind::Connect);
    }
}

#[test]
fn r52_transient_refresh_grace_is_request_time_only() {
    for status in [Some(429), Some(503), None] {
        check_transient_refresh(status);
    }
}

#[test]
fn a2b_14_model_catalog_lists_factory_models() {
    let list = models::catalog_at(std::time::SystemTime::now()).expect("catalog");
    assert_eq!(list.models().len(), 47);
    assert!(
        list.models()
            .iter()
            .any(|model| model.id() == "claude-sonnet-5-5")
    );
    assert!(list.models().iter().any(|model| model.id() == "gpt-5.5"));
    let (wire, _) = models::wire_target("minimax-m2.7");
    assert_eq!(wire, ProtocolId::ANTHROPIC_MESSAGES);
    assert_eq!(models::x_api_provider("gpt-5.5"), Some("openai"));
}

#[test]
fn a2b_15_usage_parse_limits() {
    let limits = json!({
        "limits": {
            "standard": {
                "fiveHour": {"usedPercent": 25.0, "windowEnd": "2030-01-01T00:00:00Z"},
                "monthly": {"usedPercent": 10.0, "windowEnd": "2030-02-01T00:00:00Z"},
            }
        }
    });
    let windows = quota::windows_from_limits(&limits).expect("windows");
    assert_eq!(windows.len(), 2);
    assert_eq!(windows[0].name(), "5 hours");
    assert_eq!(windows[0].used(), 25);
}

#[test]
fn a2b_16_chat_upstream_round_trip() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_oauth(&host);
    host.http().enqueue(
        json!({
            "content": [{"type": "text", "text": "ok"}],
            "id": "msg-1",
            "model": "claude-sonnet-5-5",
            "role": "assistant",
            "stop_reason": "end_turn",
            "type": "message",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })
        .to_string()
        .into_bytes(),
    );
    let sub = subscription(&host);
    let upstream = sub.upstream(&id, host.context()).expect("upstream");
    let query = ProbeQuery::new(ProtocolId::OPENAI_CHAT, "claude-sonnet-5-5").expect("query");
    block_on(upstream.probe(&query)).expect("probe");
    let calls = host.http().calls();
    assert!(
        calls
            .iter()
            .any(|call| call.uri().contains("/api/llm/a/v1") && call.uri().contains("messages"))
    );
    assert!(calls.iter().any(|call| {
        call.sensitive_headers()
            .contains(&"authorization".to_owned())
    }));
}

#[test]
fn a2b_17_no_secrets_in_errors() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_oauth(&host);
    host.http()
        .enqueue_response(401, &[], br#"{"message":"invalid_grant"}"#.to_vec());
    let error = block_on(tokens::refresh_for_test(
        host.context().clone(),
        endpoints(),
        id.clone(),
        secret("factory-access-1"),
    ))
    .expect_err("refresh");
    assert!(!error.message().contains("factory-refresh-1"));
    assert_eq!(error.kind(), PluginErrorKind::Auth);
    let blob = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    assert!(
        SecretObject::parse(&blob)
            .expect("blob")
            .flag(tokens::LAPSED)
    );
}

#[test]
fn a2b_18_api_key_sign_in_stores_api_auth() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    host.http().enqueue(
        json!({"email": "api@factory.test", "userId": "u-9", "orgId": "org-not-sent", "premBaseHostV2": "prem-key.factory.test"})
            .to_string()
            .into_bytes(),
    );
    let id = block_on(sub.complete_sign_in(
        &ctx(),
        SignInProof::Token(secret("fk-test-key-12345678")),
        host.context(),
    ))
    .expect("api sign-in");
    let blob = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    let parsed = SecretObject::parse(&blob).expect("blob");
    assert_eq!(parsed.text("auth_type"), Some("api"));
    assert_eq!(parsed.text("prem_base_host"), Some("prem-key.factory.test"));
    assert_eq!(parsed.text("active_organization_id"), None);
    assert_eq!(
        parsed.secret("api_key").as_ref().map(secret_text),
        Some("fk-test-key-12345678")
    );
}

fn seed_oauth_eu(host: &TestHost) -> AccountId {
    let id = AccountId::from_static("a-0000000000000002");
    let blob = SecretObject::new()
        .with_number("v", 1)
        .with_text("auth_type", AuthKind::Oauth.as_str())
        .with_secret("access_token", &secret("factory-access-eu"))
        .with_secret("refresh_token", &secret("factory-refresh-eu"))
        .with_number("expires_ms", 4_000_000_000_000)
        .with_text("email", "eu@factory.test")
        .with_text("region", "eu")
        .with_text("active_organization_id", "org-eu");
    block_on(async {
        let accounts = host.context().accounts();
        let account = accounts
            .account(id.clone(), "eu@factory.test")
            .expect("account");
        accounts
            .upsert(account, blob.to_secret())
            .await
            .expect("upsert");
    });
    id
}

#[test]
fn a2b_39_glm_uses_open_wire_and_x_api_provider() {
    let (wire, path) = models::wire_target("glm-5.3");
    assert_eq!(wire, ProtocolId::OPENAI_CHAT);
    assert_eq!(path, "/api/llm/o/v1");
    assert_eq!(models::x_api_provider("glm-5.3"), Some("fireworks"));
    assert_eq!(
        models::outbound_llm_headers("glm-5.3"),
        vec![("x-api-provider".to_owned(), "fireworks".to_owned())]
    );

    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_oauth(&host);
    host.http().enqueue(
        json!({
            "id": "chat-glm",
            "object": "chat.completion",
            "choices": [{"message": {"role": "assistant", "content": "ok"}}]
        })
        .to_string()
        .into_bytes(),
    );
    let sub = subscription(&host);
    let upstream = sub.upstream(&id, host.context()).expect("upstream");
    let query = ProbeQuery::new(ProtocolId::OPENAI_CHAT, "glm-5.3").expect("query");
    block_on(upstream.probe(&query)).expect("probe");
    let calls = host.http().calls();
    let call = calls.last().expect("upstream call");
    assert!(call.uri().contains("/api/llm/o/v1"));
    assert!(
        call.sensitive_headers()
            .iter()
            .any(|name| name.eq_ignore_ascii_case("x-api-provider"))
    );
}

#[test]
fn a2b_40_eu_account_quota_uses_eu_api_host() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_oauth_eu(&host);
    host.http().enqueue(
        json!({
            "limits": {
                "standard": {
                    "fiveHour": {"usedPercent": 0.0, "windowEnd": "2030-01-01T00:00:00Z"},
                }
            }
        })
        .to_string()
        .into_bytes(),
    );
    let sub = subscription(&host);
    block_on(sub.quota(&id, host.context())).expect("quota");
    assert!(
        host.http()
            .calls()
            .last()
            .is_some_and(|call| call.uri().contains("factory-eu.test"))
    );
}

#[test]
fn a2b_42_eu_account_upstream_uses_eu_api_host() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_oauth_eu(&host);
    host.http().enqueue(
        json!({
            "id": "chat-eu",
            "object": "chat.completion",
            "choices": [{"message": {"role": "assistant", "content": "ok"}}]
        })
        .to_string()
        .into_bytes(),
    );
    let sub = subscription(&host);
    let upstream = sub.upstream(&id, host.context()).expect("upstream");
    let query = ProbeQuery::new(ProtocolId::OPENAI_CHAT, "glm-5.3").expect("query");
    block_on(upstream.probe(&query)).expect("probe");
    assert!(host.http().calls().last().is_some_and(|call| {
        call.uri().contains("factory-eu.test") && call.uri().contains("/api/llm/o/v1")
    }));
}

fn set_prem(host: &TestHost, id: &AccountId, value: &str) {
    block_on(async {
        let accounts = host.context().accounts();
        let stored = accounts.secret(id).await.expect("read").expect("secret");
        let blob = SecretObject::parse(&stored)
            .expect("blob")
            .with_text("prem_base_host", value);
        let account = accounts
            .account(id.clone(), "user@factory.test")
            .expect("account");
        accounts
            .upsert(account, blob.to_secret())
            .await
            .expect("update");
    });
}

#[test]
fn r52_prem_host_routes_models_and_invalid_host_sends_nothing() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_oauth(&host);
    set_prem(&host, &id, "prem.factory.test///");
    host.http().enqueue(br#"{"id":"chat-prem"}"#.to_vec());
    let sub = subscription(&host);
    let upstream = sub.upstream(&id, host.context()).expect("upstream");
    let query = ProbeQuery::new(ProtocolId::OPENAI_CHAT, "glm-5.3").expect("query");
    block_on(upstream.probe(&query)).expect("probe");
    let calls = host.http().calls();
    let call = calls.last().expect("model call");
    assert_eq!(
        call.uri(),
        "https://prem.factory.test/api/llm/o/v1/chat/completions"
    );
    for name in [
        "x-client-version",
        "x-factory-client",
        "x-provider-routing-source",
    ] {
        assert!(call.sensitive_headers().iter().any(|header| header == name));
    }

    set_prem(&host, &id, "http://prem.factory.test");
    let error = block_on(upstream.probe(&query)).expect_err("invalid host");
    assert_eq!(error.kind(), PluginErrorKind::Config);
    assert_eq!(host.http().calls().len(), 1);
}

#[test]
fn r52_fast_anthropic_base_model_uses_fast_id_speed_and_beta() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_oauth(&host);
    host.http().enqueue(br#"{"id":"message-fast"}"#.to_vec());
    let sub = subscription(&host);
    let upstream = sub.upstream(&id, host.context()).expect("upstream");
    let query = ProbeQuery::new(ProtocolId::ANTHROPIC_MESSAGES, "claude-opus-5-5").expect("query");
    let mut request = query.minimal_request();
    request.extensions.insert("speed", json!("fast"));
    block_on(upstream.execute(request, ProtocolId::ANTHROPIC_MESSAGES)).expect("fast");
    let calls = host.http().calls();
    let call = calls.last().expect("model call");
    let body: hofund_plugin_sdk::json::Value =
        hofund_plugin_sdk::json::from_slice(call.body()).expect("body");
    assert_eq!(body["model"], "claude-opus-5-5-fast");
    assert_eq!(body["speed"], "fast");
    assert!(
        call.sensitive_headers()
            .iter()
            .any(|name| name == "anthropic-beta")
    );
    assert!(call.uri().ends_with("/api/llm/a/v1/messages"));
}

#[test]
fn r52_fast_responses_base_model_uses_fast_id_and_priority() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_oauth(&host);
    host.http().enqueue(br#"{"id":"response-fast"}"#.to_vec());
    let sub = subscription(&host);
    let upstream = sub.upstream(&id, host.context()).expect("upstream");
    let query = ProbeQuery::new(ProtocolId::OPENAI_RESPONSES, "gpt-5.5").expect("query");
    let mut request = query.minimal_request();
    request.extensions.insert("service_tier", json!("priority"));
    block_on(upstream.execute(request, ProtocolId::OPENAI_RESPONSES)).expect("fast");
    let calls = host.http().calls();
    let call = calls.last().expect("model call");
    let body: hofund_plugin_sdk::json::Value =
        hofund_plugin_sdk::json::from_slice(call.body()).expect("body");
    assert_eq!(body["model"], "gpt-5.5-fast");
    assert_eq!(body["service_tier"], "priority");
    assert_eq!(call.uri(), "https://factory.test/api/llm/o/v1/responses");
}

#[test]
fn r52_gemini_stream_uses_generate_with_model_and_thinking_config() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_oauth(&host);
    host.http().enqueue_response(
        200,
        &[("content-type", "text/event-stream")],
        b"data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"ok\"}]}}]}\n\n".to_vec(),
    );
    let sub = subscription(&host);
    let upstream = sub.upstream(&id, host.context()).expect("upstream");
    let query = ProbeQuery::new(ProtocolId::GENERATIVE_LANGUAGE, "gemini-4-argon").expect("query");
    let mut request = query.minimal_request();
    request.stream = true;
    request.extensions.insert(
        "generationConfig",
        json!({"thinkingConfig": {"includeThoughts": true, "thinkingLevel": "high"}}),
    );
    block_on(upstream.execute(request, ProtocolId::GENERATIVE_LANGUAGE)).expect("stream");
    let calls = host.http().calls();
    let call = calls.last().expect("model call");
    assert_eq!(call.uri(), "https://factory.test/api/llm/g/v1/generate");
    let body: hofund_plugin_sdk::json::Value =
        hofund_plugin_sdk::json::from_slice(call.body()).expect("body");
    assert_eq!(body["model"], "gemini-4-argon");
    assert_eq!(
        body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "high"
    );
    assert!(
        body["systemInstruction"]["parts"][0]["text"]
            .as_str()
            .is_some_and(|text| text.starts_with("You are Droid,"))
    );
    assert!(body.get("stream").is_none());
    assert!(
        call.sensitive_headers()
            .iter()
            .any(|name| name == "x-api-provider")
    );
}

#[test]
fn r52_content_policy_is_a_non_cooling_request_refusal_without_a_retry() {
    for (body, upstream_error) in [
        (
            r#"{"code":"content_policy_rejected","detail":"blocked"}"#,
            false,
        ),
        (
            r#"{"error":{"code":"content_policy_rejected","message":"blocked"}}"#,
            false,
        ),
        (
            r#"{"code":"content_policy_rejected","detail":"blocked"}"#,
            true,
        ),
    ] {
        let dir = tempfile::tempdir().expect("dir");
        let host = host(&dir);
        let id = seed_oauth(&host);
        let headers: &[(&str, &str)] = if upstream_error {
            &[("x-factory-upstream-error", "1")]
        } else {
            &[]
        };
        host.http()
            .enqueue_response(403, headers, body.as_bytes().to_vec());
        let sub = subscription(&host);
        let upstream = sub.upstream(&id, host.context()).expect("upstream");
        let query = ProbeQuery::new(ProtocolId::OPENAI_CHAT, "glm-5.3").expect("query");
        let error = block_on(upstream.execute(query.minimal_request(), ProtocolId::OPENAI_CHAT))
            .err()
            .expect("refused");
        assert_eq!(host.http().calls().len(), 1);
        if upstream_error {
            assert_eq!(error.status(), Some(403));
            assert!(error.cools());
        } else {
            assert_eq!(error.status(), Some(400));
            assert!(!error.cools());
            assert!(error.skips_failover_same_plugin());
            assert!(error.message().contains("reads the whole conversation"));
            assert!(error.message().contains("content_policy_rejected"));
        }
    }
}

#[test]
fn r52_rate_limit_keeps_retry_after_and_cools() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_oauth(&host);
    host.http()
        .enqueue_response(429, &[("retry-after", "17")], b"slow down".to_vec());
    let sub = subscription(&host);
    let upstream = sub.upstream(&id, host.context()).expect("upstream");
    let query = ProbeQuery::new(ProtocolId::OPENAI_CHAT, "glm-5.3").expect("query");
    let error = block_on(upstream.execute(query.minimal_request(), ProtocolId::OPENAI_CHAT))
        .err()
        .expect("rate limit");
    assert_eq!(error.status(), Some(429));
    assert_eq!(
        error.retry_after(),
        Some(std::time::Duration::from_secs(17))
    );
    assert!(error.cools());
}
