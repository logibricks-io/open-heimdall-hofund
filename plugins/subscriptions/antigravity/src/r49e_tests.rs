//! Offline tests for the Antigravity hub version manifest refresh (R-49e).

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use hofund_kit_subscription::{TestHost, block_on};
use hofund_plugin_api::{
    AccountId, Completion, PluginId, PluginKind, ProtocolId, RawTable, RawValue, SecretString,
    Subscription, SubscriptionFactory,
};
use hofund_plugin_sdk::oauth::{SecretObject, account_id_from};

use crate::{Endpoints, client, factory_with_endpoints, version};

const TOKEN_URL: &str = "https://oauth.test/token";
const LOAD: &str = "https://code-assist-prod.test";
const DAILY: &str = "https://code-assist-daily.test";
const MANIFEST: &str = "https://antigravity-version.test/manifest/latest-arm64-mac.yml";
const TEST_CLIENT_ID: &str = "hofund-test-antigravity-client";
const TEST_CLIENT_SECRET: &str = "hofund-test-antigravity-secret";
const UNARY_REPLY: &str = r#"{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"hi"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":1,"totalTokenCount":4}}}"#;

fn endpoints() -> Endpoints {
    Endpoints {
        authorize: "https://accounts.test/o/oauth2/v2/auth".to_owned(),
        token: TOKEN_URL.to_owned(),
        userinfo: "https://oauth.test/userinfo".to_owned(),
        load_code_assist: LOAD.to_owned(),
        code_assist: DAILY.to_owned(),
        version_manifest: MANIFEST.to_owned(),
        onboard_poll: Duration::from_millis(1),
    }
}

fn host_at(dir: &tempfile::TempDir, now: SystemTime) -> TestHost {
    TestHost::open_for_at(
        PluginKind::Subscription,
        PluginId::from_static("antigravity"),
        dir.path(),
        now,
    )
}

fn subscription(host: &TestHost) -> Arc<dyn Subscription> {
    let factory = factory_with_endpoints(endpoints());
    let mut raw = RawTable::new();
    raw.insert(client::ID_KEY, RawValue::String(TEST_CLIENT_ID.to_owned()));
    raw.insert(
        client::SECRET_KEY,
        RawValue::String(TEST_CLIENT_SECRET.to_owned()),
    );
    let config = factory.validate(&raw).expect("config");
    factory.build(&config, host.context()).expect("build")
}

fn token_reply(access: &str) -> Vec<u8> {
    format!(r#"{{"access_token":"{access}","token_type":"Bearer","expires_in":3600}}"#).into_bytes()
}

fn manifest_body(version: &str) -> Vec<u8> {
    format!("version: {version}\npath: bundle.zip\n").into_bytes()
}

fn seed(host: &TestHost) -> AccountId {
    let id = account_id_from(&["antigravity", "u@example.test"]);
    let blob = SecretObject::new()
        .with_number("v", 1)
        .with_secret("refresh_token", &SecretString::from("refresh-1".to_owned()))
        .with_text("email", "u@example.test")
        .with_text(crate::project::PROJECT, "proj-1");
    let account = host
        .context()
        .accounts()
        .account(id.clone(), "u@example.test")
        .expect("account");
    block_on(host.context().accounts().upsert(account, blob.to_secret())).expect("seed");
    id
}

fn unary_request() -> hofund_plugin_api::Request {
    hofund_plugin_sdk::json::from_value(hofund_plugin_sdk::json::json!({
        "model": "gemini-2.5-pro",
        "messages": [{"role":"user","content":[{"type":"text","text":"hi"}]}],
    }))
    .expect("request")
}

fn assert_hub_version(host: &TestHost, expected: &str) {
    let calls = host.http().calls();
    let code_call = calls
        .iter()
        .find(|call| call.uri().contains("generateContent"))
        .expect("code-assist call");
    assert!(
        code_call
            .sensitive_headers()
            .iter()
            .any(|name| name.eq_ignore_ascii_case("user-agent")),
        "user-agent must be on the upstream call"
    );
    assert_eq!(
        block_on(version::resolve(host.context(), MANIFEST)),
        expected
    );
}

/// One test keeps the shared version cache from racing other lib tests.
#[test]
fn r49e_version_manifest_refresh_is_cached_and_used_on_upstream() {
    version::use_global_cache_for_test();
    let dir = tempfile::tempdir().expect("dir");
    version::reset_cache_for_test();
    let host = host_at(&dir, SystemTime::UNIX_EPOCH);
    host.http().enqueue_response(503, &[], Vec::<u8>::new());
    let resolved = block_on(version::resolve(host.context(), MANIFEST));
    assert_eq!(resolved, version::FALLBACK_VERSION);

    version::reset_cache_for_test();
    let host = host_at(&dir, SystemTime::UNIX_EPOCH);
    let id = seed(&host);
    let sub = subscription(&host);
    host.http().enqueue(token_reply("access-1"));
    host.http().enqueue(manifest_body("3.1.4"));
    host.http().enqueue(UNARY_REPLY.as_bytes().to_vec());
    block_on(
        sub.upstream(&id, host.context())
            .expect("upstream")
            .execute(unary_request(), ProtocolId::GENERATIVE_LANGUAGE),
    )
    .expect("execute");
    let calls = host.http().calls();
    let manifest_call = calls
        .iter()
        .find(|call| call.uri() == MANIFEST)
        .expect("manifest");
    assert_eq!(
        manifest_call.header("user-agent"),
        Some(version::MANIFEST_USER_AGENT)
    );
    assert_hub_version(&host, "3.1.4");

    version::reset_cache_for_test();
    let host = host_at(&dir, SystemTime::UNIX_EPOCH);
    let id = seed(&host);
    let sub = subscription(&host);
    host.http().enqueue(token_reply("access-1"));
    host.http().enqueue(manifest_body("3.2.0"));
    host.http().enqueue(UNARY_REPLY.as_bytes().to_vec());
    host.http().enqueue(token_reply("access-2"));
    host.http().enqueue(UNARY_REPLY.as_bytes().to_vec());
    let upstream = sub.upstream(&id, host.context()).expect("upstream");
    block_on(upstream.execute(unary_request(), ProtocolId::GENERATIVE_LANGUAGE)).expect("first");
    block_on(upstream.execute(unary_request(), ProtocolId::GENERATIVE_LANGUAGE)).expect("second");
    let manifest_calls = host
        .http()
        .calls()
        .iter()
        .filter(|call| call.uri() == MANIFEST)
        .count();
    assert_eq!(manifest_calls, 1);

    version::reset_cache_for_test();
    version::warm_cache_for_test(
        "3.0.0",
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000),
    );
    let host = host_at(
        &dir,
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 + 6 * 3600 + 1),
    );
    let id = seed(&host);
    let sub = subscription(&host);
    host.http().enqueue(token_reply("access-1"));
    host.http().enqueue(manifest_body("3.0.1"));
    host.http().enqueue(UNARY_REPLY.as_bytes().to_vec());
    block_on(
        sub.upstream(&id, host.context())
            .expect("upstream")
            .execute(unary_request(), ProtocolId::GENERATIVE_LANGUAGE),
    )
    .expect("execute");
    assert_hub_version(&host, "3.0.1");

    version::reset_cache_for_test();
    version::warm_cache_for_test("3.5.0", SystemTime::UNIX_EPOCH);
    let host = host_at(&dir, SystemTime::UNIX_EPOCH + Duration::from_secs(7 * 3600));
    let id = seed(&host);
    let sub = subscription(&host);
    host.http().enqueue(token_reply("access-1"));
    host.http().enqueue_connect("manifest offline");
    host.http().enqueue(UNARY_REPLY.as_bytes().to_vec());
    let Completion::Raw(_) = block_on(
        sub.upstream(&id, host.context())
            .expect("upstream")
            .execute(unary_request(), ProtocolId::GENERATIVE_LANGUAGE),
    )
    .expect("execute") else {
        panic!("expected raw completion");
    };
    assert_hub_version(&host, "3.5.0");
}
