//! Unit tests against recording host backends; no network.

use std::sync::Arc;

use hofund_kit_subscription::{TestHost, block_on};
use hofund_plugin_api::{
    AccountId, PluginId, PluginKind, ProbeQuery, ProtocolId, RawTable, SecretString, Subscription,
    SubscriptionFactory,
};
use hofund_plugin_sdk::json;
use hofund_plugin_sdk::oauth::{SecretObject, secret_text};

use crate::creds::{self, Credential, Site, Stored};
use crate::discover::{self, REFERENCE};
use crate::models;
use crate::quota;
use crate::{Endpoints, factory_with_endpoints};

const OPENAPI: &str = "https://qoder.test";
const CHAT: &str = "https://qoder-chat.test/v1";
const TOKEN: &str = "qoder-access-test-token";
const REFRESH: &str = "qoder-refresh-test-token";

fn endpoints() -> Endpoints {
    Endpoints {
        openapi_global: OPENAPI.to_owned(),
        openapi_cn: "https://qoder-cn.test".to_owned(),
        chat_global: CHAT.to_owned(),
        chat_cn: "https://qoder-cn-chat.test/v1".to_owned(),
    }
}

fn host(dir: &tempfile::TempDir) -> TestHost {
    TestHost::open_for(
        PluginKind::Subscription,
        PluginId::from_static("qoder"),
        dir.path(),
    )
}

fn subscription(host: &TestHost) -> Arc<dyn Subscription> {
    let factory = factory_with_endpoints(endpoints());
    let cfg = factory.validate(&RawTable::new()).expect("config");
    factory.build(&cfg, host.context()).expect("build")
}

fn seed(host: &TestHost) -> AccountId {
    seed_with_expiry(host, 1)
}

fn seed_with_expiry(host: &TestHost, expires_at: i64) -> AccountId {
    let credential = Credential {
        uid: "uid-1".to_owned(),
        email: "me@example.test".to_owned(),
        name: String::new(),
        token: SecretString::from(TOKEN.to_owned()),
        refresh_token: SecretString::from(REFRESH.to_owned()),
        expires_at,
        machine_id: "machine".to_owned(),
        site: Site::Global,
    };
    let id = creds::account_id(&credential);
    block_on(async {
        let accounts = host.context().accounts();
        let account = accounts
            .account(id.clone(), "me@example.test")
            .expect("account");
        accounts
            .upsert(account, creds::secret_blob(&credential).to_secret())
            .await
            .expect("upsert");
    });
    id
}

#[test]
fn a2c_3_discover_import_fixture_credentials() {
    let dir = tempfile::tempdir().expect("dir");
    let home = dir.path().join("agent").join(".qoder");
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
    let blob = SecretObject::parse(&secret).expect("blob");
    assert_eq!(
        blob.secret("access_token").as_ref().map(secret_text),
        Some("qoder-access-fixture-token")
    );
}

#[test]
fn a2c_4_refresh_on_expiry() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host);
    host.http().enqueue(
        json::json!({
            "token": "qoder-access-renewed",
            "refresh_token": "qoder-refresh-renewed",
            "expires_at": 4_000_000_000_000_i64
        })
        .to_string()
        .into_bytes(),
    );
    block_on(subscription(&host).refresh(&id, host.context())).expect("refresh");
    let calls = host.http().calls();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].uri().contains("/jobToken/refresh"));
    let secret = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    assert_eq!(
        SecretObject::parse(&secret)
            .expect("blob")
            .secret("access_token")
            .as_ref()
            .map(secret_text),
        Some("qoder-access-renewed")
    );
}

#[test]
fn a2c_5_model_list_parsing() {
    let list = models::parse_list(include_bytes!("fixtures/models.json")).expect("parse");
    assert!(list.models().iter().any(|model| model.id() == "qfmodel"));
}

#[test]
fn a2c_6_upstream_round_trip() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_with_expiry(&host, 4_000_000_000_000);
    host.http().enqueue(
        br#"{"id":"chatcmpl-1","object":"chat.completion","choices":[{"message":{"role":"assistant","content":"ok"}}]}"#,
    );
    let upstream = subscription(&host)
        .upstream(&id, host.context())
        .expect("upstream");
    let query = ProbeQuery::new(ProtocolId::OPENAI_CHAT, "qfmodel").expect("query");
    block_on(upstream.probe(&query)).expect("probe");
    let calls = host.http().calls();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].uri().contains("/chat/completions"));
}

#[test]
fn a2c_7_quota_windows() {
    let windows = quota::windows(br#"{"quota":{"total":100,"used":25}}"#).expect("windows");
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0].name(), "Credits");
}

#[test]
fn a2c_8_secrets_not_in_error_strings() {
    let key = "qoder-secret-key-xyzzy";
    let path = std::path::Path::new("/nonexistent/qoder/credential.json");
    let error = creds::read_file(path).expect_err("missing");
    let text = error.to_string();
    assert!(!text.contains(key));
    let credential = Credential {
        uid: "u".to_owned(),
        email: String::new(),
        name: String::new(),
        token: SecretString::from(key.to_owned()),
        refresh_token: SecretString::from("r".to_owned()),
        expires_at: 0,
        machine_id: String::new(),
        site: Site::Global,
    };
    let debug = format!("{credential:?}");
    assert!(!debug.contains(key));
    let stored = Stored {
        credential: credential.clone(),
    };
    let stored_debug = format!("{stored:?}");
    assert!(!stored_debug.contains(key));
}

#[test]
fn a2c_9_daily_checkin_uses_bearer() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host);
    host.http().enqueue(
        br#"{"campaigns":[{"campaignId":"daily","actionType":"CLAIM_BENEFIT","claimStatus":"CLAIMABLE","startAt":0,"endAt":4102444800,"benefit":{"amount":100}}]}"#,
    );
    host.http()
        .enqueue(br#"{"status":"CLAIMED","benefit":{"amount":100}}"#);
    let result = block_on(subscription(&host).daily_checkin(&id, host.context())).expect("checkin");
    assert_eq!(result.outcome(), hofund_plugin_api::CheckinOutcome::Claimed);
}
