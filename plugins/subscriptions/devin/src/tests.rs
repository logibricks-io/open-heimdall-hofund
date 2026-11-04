//! Unit tests against recording host backends; no network.

use std::sync::Arc;

use hofund_kit_subscription::{TestHost, block_on};
use hofund_plugin_api::{
    AccountId, PluginId, PluginKind, ProbeQuery, ProtocolId, RawTable, SecretString, Subscription,
    SubscriptionFactory,
};
use hofund_plugin_sdk::oauth::{SecretObject, secret_text};

use crate::creds::{self, Credential};
use crate::{DevinFactory, discover};

const MODELS_FIXTURE: &str = include_str!("fixtures/models.json");

fn host(dir: &tempfile::TempDir) -> TestHost {
    TestHost::open_for(
        PluginKind::Subscription,
        PluginId::from_static("devin"),
        dir.path(),
    )
}

fn subscription(host: &TestHost) -> Arc<dyn Subscription> {
    let cfg = DevinFactory.validate(&RawTable::new()).expect("config");
    DevinFactory.build(&cfg, host.context()).expect("build")
}

fn seed_account(host: &TestHost, key: &str, label: &str) -> AccountId {
    let credential = Credential {
        key: SecretString::from(key.to_owned()),
        api_server: "https://server.test".to_owned(),
        devin_api: "https://api.devin.test".to_owned(),
        label: label.to_owned(),
    };
    let id = creds::account_id(&credential);
    block_on(async {
        let accounts = host.context().accounts();
        let account = accounts
            .account(id.clone(), label.to_owned())
            .expect("account");
        accounts
            .upsert(account, creds::secret_blob(&credential).to_secret())
            .await
            .expect("upsert");
    });
    id
}

#[test]
fn a2b_3_discover_import_fixture_credentials() {
    let dir = tempfile::tempdir().expect("dir");
    let home = dir.path().join("agent").join("devin");
    std::fs::create_dir_all(&home).expect("dir");
    std::fs::write(
        home.join("credentials.toml"),
        "windsurf_api_key = \"devin-import-key-1\"\n",
    )
    .expect("write");
    let rows = discover::list(&home).expect("list");
    assert_eq!(rows.len(), 1);
    let host = host(&dir);
    let id =
        block_on(discover::import(&home, discover::REFERENCE, host.context())).expect("import");
    let secret = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    let blob = SecretObject::parse(&secret).expect("blob");
    assert_eq!(
        blob.secret("api_key").as_ref().map(secret_text),
        Some("devin-import-key-1")
    );
}

#[test]
fn a2b_4_refresh_is_no_op_for_api_key() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = seed_account(&host, "devin-static-key", "dev@example.test");
    block_on(sub.refresh(&id, host.context())).expect("refresh");
    let calls = host.http().calls();
    assert!(calls.is_empty(), "refresh must not call the network");
}

#[test]
fn a2b_5_model_list_parsing() {
    let models = crate::models::parse_models(MODELS_FIXTURE.as_bytes()).expect("parse");
    assert!(models.iter().any(|model| model.id() == "swe-2"));
}

#[test]
fn a2b_6_upstream_round_trip_with_recording_http() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    host.http()
        .enqueue(br#"{"choices":[{"message":{"role":"assistant","content":"ok"}}]}"#.to_vec());
    let id = seed_account(&host, "devin-upstream-key", "dev@example.test");
    let upstream = subscription(&host)
        .upstream(&id, host.context())
        .expect("upstream");
    let request = ProbeQuery::new(ProtocolId::OPENAI_CHAT, "swe-2")
        .expect("query")
        .minimal_request();
    block_on(upstream.execute(request, ProtocolId::OPENAI_CHAT)).expect("execute");
    let calls = host.http().calls();
    let call = calls.last().expect("call");
    assert!(call.uri().contains("api.devin.test/v1/chat/completions"));
    assert!(
        call.sensitive_headers()
            .contains(&"authorization".to_owned())
    );
}

#[test]
fn a2b_7_secrets_not_in_debug_or_errors() {
    let key = "devin-secret-key-xyzzy";
    let credential = Credential {
        key: SecretString::from(key.to_owned()),
        api_server: "https://server.test".to_owned(),
        devin_api: "https://api.devin.test".to_owned(),
        label: "dev@example.test".to_owned(),
    };
    let debug = format!("{credential:?}");
    assert!(!debug.contains(key));
    let error =
        creds::read_credentials(std::path::Path::new("/nonexistent/devin/credentials.toml"));
    let text = error.expect_err("missing").to_string();
    assert!(!text.contains(key));
}

#[test]
fn a2b_8_quota_is_empty() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = seed_account(&host, "devin-quota-key", "dev@example.test");
    let windows = block_on(sub.quota(&id, host.context())).expect("quota");
    assert!(windows.is_empty());
}

#[test]
fn a2b_9_list_models_requires_cli() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_account(&host, "devin-models-key", "dev@example.test");
    let sub = subscription(&host);
    let error = block_on(sub.list_models(&id, host.context())).expect_err("models");
    assert!(error.message().contains("devin models list"));
}

#[test]
fn a2b_28_reimport_replaces_stored_key() {
    let dir = tempfile::tempdir().expect("dir");
    let home = dir.path().join("agent").join("devin");
    std::fs::create_dir_all(&home).expect("dir");
    let path = home.join("credentials.toml");
    std::fs::write(&path, "windsurf_api_key = \"devin-key-v1\"\n").expect("write");
    let host = host(&dir);
    let id =
        block_on(discover::import(&home, discover::REFERENCE, host.context())).expect("import");
    std::fs::write(&path, "windsurf_api_key = \"devin-key-v2\"\n").expect("rewrite");
    block_on(discover::import(&home, discover::REFERENCE, host.context())).expect("reimport");
    let secret = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    assert_eq!(
        SecretObject::parse(&secret)
            .expect("blob")
            .secret("api_key")
            .as_ref()
            .map(secret_text),
        Some("devin-key-v2")
    );
}
