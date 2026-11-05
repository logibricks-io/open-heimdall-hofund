//! Unit tests against recording host backends; no network.

use std::sync::Arc;

use hofund_kit_subscription::{TestHost, block_on};
use hofund_plugin_api::{
    AccountId, PluginErrorKind, PluginId, PluginKind, ProbeQuery, ProtocolId, RawTable,
    SecretString, Subscription, SubscriptionFactory,
};
use hofund_plugin_sdk::oauth::{SecretObject, secret_text};

use crate::body;
use crate::creds::{self, Credential};
use crate::models;
use crate::{Endpoints, factory_with_endpoints};

const CLOUD: &str = "https://zed.test";
const ACCESS: &str = "zed-access-test";

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
    crate::tokens::clear_cache_for_tests();
    crate::models::clear_vendor_cache_for_tests();
    let credential = Credential {
        user_id: format!("u-{name}"),
        access: SecretString::from(ACCESS.to_owned()),
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
fn a2c_17_models_list_401_is_auth() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_named(&host, "models-401");
    host.http().enqueue(br#"{"token":"model-token-1"}"#);
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
    host.http().enqueue_response(
        200,
        &[
            ("content-type", "application/json"),
            ("x-zed-server-supports-status-messages", "true"),
        ],
        br#"{"event":{"type":"response.created","response":{"id":"r1","object":"response","status":"completed"}}}
{"event":{"type":"response.completed","response":{"id":"r1","object":"response","status":"completed","output":[]}}}
{"status":"stream_ended"}
"#,
    );
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
