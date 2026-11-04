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

fn register_body() -> Vec<u8> {
    json!({
        "accessToken": "factory-access-1",
        "refreshToken": "factory-refresh-1",
        "expiresAt": "2030-01-01T00:00:00Z",
        "email": "user@factory.test",
        "userId": "uid-1"
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
        br#"{"access_token":"wo-access","refresh_token":"wo-refresh","token_type":"Bearer"}"#
            .to_vec(),
    );
    host.http().enqueue(register_body());
    let id = block_on(sub.complete_sign_in(&ctx(), SignInProof::Await, host.context()))
        .expect("complete");
    let blob = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    let parsed = SecretObject::parse(&blob).expect("blob");
    assert_eq!(parsed.text("auth_type"), Some("oauth"));
    assert!(parsed.secret("refresh_token").is_some());
}

#[test]
fn a2b_13_refresh_rotates_tokens() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_oauth(&host);
    host.http().enqueue(
        json!({
            "accessToken": "factory-access-2",
            "refreshToken": "factory-refresh-2",
            "expiresAt": "2031-01-01T00:00:00Z"
        })
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
        parsed
            .secret("access_token")
            .as_ref()
            .map(secret_text),
        Some("factory-access-2")
    );
    assert_eq!(
        parsed
            .secret("refresh_token")
            .as_ref()
            .map(secret_text),
        Some("factory-refresh-2")
    );
}

#[test]
fn a2b_14_model_catalog_lists_factory_models() {
    let list = models::catalog().expect("catalog");
    assert!(
        list.models()
            .iter()
            .any(|model| model.id() == "claude-sonnet-5-5")
    );
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
    assert!(calls.iter().any(
        |call| call.uri().contains("/api/llm/c/v1") && call.uri().contains("chat/completions")
    ));
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
        parsed
            .secret("api_key")
            .as_ref()
            .map(secret_text),
        Some("fk-test-key-12345678")
    );
}
