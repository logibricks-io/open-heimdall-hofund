//! Unit tests against recording host backends; no network.

use std::sync::Arc;

use hofund_kit_subscription::{TestHost, block_on};
use hofund_plugin_api::{
    AccountId, PluginId, PluginKind, ProbeQuery, ProtocolId, RawTable, SecretString, Subscription,
    SubscriptionFactory,
};
use hofund_plugin_sdk::oauth::{SecretObject, secret_text};

use crate::creds::{self, Credential};
use crate::discover::{self, REFERENCE};
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
    let credential = Credential {
        user_id: "u1".to_owned(),
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
fn a2c_14_discover_import_fixture_credentials() {
    let dir = tempfile::tempdir().expect("dir");
    let home = dir.path().join("zed-home");
    std::fs::create_dir_all(&home).expect("dir");
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/src/fixtures/credential.json"),
        home.join(creds::CREDENTIAL_FILE),
    )
    .expect("copy");
    let rows = discover::list(&home).expect("list");
    assert_eq!(rows.len(), 1);
    let host = host(&dir);
    let id = block_on(discover::import(&home, REFERENCE, host.context())).expect("import");
    let secret = block_on(host.context().accounts().secret(&id))
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
    let id = seed(&host);
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
fn a2c_17_upstream_round_trip() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host);
    host.http().enqueue(br#"{"token":"model-token-1"}"#);
    host.http()
        .enqueue(br#"{"id":"resp-1","object":"response","output":[]}"#);
    let upstream = subscription(&host)
        .upstream(&id, host.context())
        .expect("upstream");
    let query = ProbeQuery::new(ProtocolId::OPENAI_RESPONSES, "claude-sonnet-4").expect("query");
    block_on(upstream.probe(&query)).expect("probe");
    assert_eq!(host.http().calls().len(), 2);
    assert!(host.http().calls()[1].uri().contains("/completions"));
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
    let out = crate::body::transform(
        br#"{"input":[{"type":"message","role":"developer","content":"hi"}]}"#,
    );
    let text = std::str::from_utf8(&out).expect("utf8");
    assert!(text.contains("system"));
}
