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
        json!({"email": "user@factory.test", "userId": "uid-1", "orgId": "org-1", "region": "eu"})
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
    let calls = host.http().calls();
    assert!(
        calls
            .iter()
            .any(|call| call.uri().contains("/api/cli/whoami"))
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
    assert!(
        host.http()
            .calls()
            .last()
            .is_some_and(|call| { call.uri().contains("workos.test/authenticate") })
    );
}

#[test]
fn a2b_14_model_catalog_lists_factory_models() {
    let list = models::catalog().expect("catalog");
    assert_eq!(list.models().len(), 39);
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
            "id": "chat-1",
            "object": "chat.completion",
            "choices": [{"message": {"role": "assistant", "content": "ok"}}]
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
        json!({"email": "api@factory.test", "userId": "u-9"})
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
            .any(|name| name.eq_ignore_ascii_case("x-api-provider")),
        "expected x-api-provider on the upstream HTTP request"
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
