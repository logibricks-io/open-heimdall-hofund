//! Unit tests against recording host backends; no network.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use hofund_kit_subscription::{TestHost, block_on};
use hofund_plugin_api::{
    AccountId, AccountStatus, PluginErrorKind, PluginId, PluginKind, ProbeQuery, ProtocolId,
    RawTable, SecretString, Subscription, SubscriptionFactory,
};
use hofund_plugin_sdk::json;
use hofund_plugin_sdk::oauth::{SecretObject, secret_text};

use crate::creds::{self, Credential, Site, Stored};
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
    host_at(dir, SystemTime::UNIX_EPOCH)
}

fn host_at(dir: &tempfile::TempDir, now: SystemTime) -> TestHost {
    TestHost::open_for_at(
        PluginKind::Subscription,
        PluginId::from_static("qoder"),
        dir.path(),
        now,
    )
}

/// A wall clock far enough past a 1 ms expiry that `ready_at` is false.
fn after_expiry() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(10_000)
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
fn a2c_3_import_fixture_credentials() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/fixtures/credential.json"
    ))
    .expect("read");
    let sub = subscription(&host);
    let ids = block_on(sub.import_accounts(&text, host.context())).expect("import");
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
fn a2c_4b_upstream_401_forces_refresh_on_fresh_token() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_with_expiry(&host, 4_000_000_000_000);
    host.http().enqueue_response(401, &[], Vec::new());
    host.http().enqueue(
        json::json!({
            "token": "qoder-access-after-401",
            "refresh_token": "qoder-refresh-after-401",
            "expires_at": 4_000_000_000_001_i64
        })
        .to_string()
        .into_bytes(),
    );
    host.http().enqueue(
        br#"{"id":"chatcmpl-1","object":"chat.completion","choices":[{"message":{"role":"assistant","content":"ok"}}]}"#,
    );
    let upstream = subscription(&host)
        .upstream(&id, host.context())
        .expect("upstream");
    let query = ProbeQuery::new(ProtocolId::OPENAI_CHAT, "qfmodel").expect("query");
    block_on(upstream.probe(&query)).expect("probe after refresh");
    let calls = host.http().calls();
    assert_eq!(calls.len(), 3);
    assert!(calls[0].uri().contains("/chat/completions"));
    assert!(calls[1].uri().contains("/jobToken/refresh"));
    assert!(calls[2].uri().contains("/chat/completions"));
}

#[test]
fn a2c_5_model_list_parsing() {
    let list = models::parse_list(include_bytes!("fixtures/models.json")).expect("parse");
    assert!(list.models().iter().any(|model| model.id() == "qfmodel"));
}

#[test]
fn a2c_5b_model_list_401_is_auth() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_with_expiry(&host, 4_000_000_000_000);
    host.http().enqueue_response(401, &[], Vec::new());
    let err = block_on(subscription(&host).list_models(&id, host.context())).expect_err("401");
    assert_eq!(err.kind(), PluginErrorKind::Auth);
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
    let bad = creds::parse_json("not json").expect_err("bad");
    let text = bad.to_string();
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
    let id = seed_with_expiry(&host, 4_000_000_000_000);
    host.http().enqueue(
        br#"{"campaigns":[{"campaignId":"daily","actionType":"CLAIM_BENEFIT","claimStatus":"CLAIMABLE","startAt":0,"endAt":4102444800,"benefit":{"amount":100}}]}"#,
    );
    host.http()
        .enqueue(br#"{"status":"CLAIMED","benefit":{"amount":100}}"#);
    let result = block_on(subscription(&host).daily_checkin(&id, host.context())).expect("checkin");
    assert_eq!(result.outcome(), hofund_plugin_api::CheckinOutcome::Claimed);
}

#[test]
fn a2c_10_account_status_ready_with_refresh_when_access_expired() {
    let dir = tempfile::tempdir().expect("dir");
    // The default TestHost clock is the Unix epoch, so expires_at=1 is still
    // in the future and this assertion would not cover the refresh-token arm.
    let host = host_at(&dir, after_expiry());
    let id = seed_with_expiry(&host, 1);
    let status = block_on(subscription(&host).account_status(&id, host.context())).expect("status");
    assert!(matches!(status, AccountStatus::Ready));
}

#[test]
fn a2c_12_refresh_refused_reports_needs_sign_in() {
    for status in [401_u16, 403] {
        let dir = tempfile::tempdir().expect("dir");
        let host = host_at(&dir, after_expiry());
        let id = seed_with_expiry(&host, 1);
        host.http().enqueue_response(status, &[], Vec::new());
        let err = block_on(subscription(&host).refresh(&id, host.context())).expect_err("refused");
        assert_eq!(err.kind(), PluginErrorKind::Auth);
        assert_eq!(err.message(), creds::LAPSED_REASON);
        assert!(!err.to_string().contains(REFRESH));
        assert!(!err.to_string().contains(TOKEN));
        let secret = block_on(host.context().accounts().secret(&id))
            .expect("read")
            .expect("stored");
        assert!(
            SecretObject::parse(&secret)
                .expect("blob")
                .flag(creds::LAPSED)
        );
        let reported =
            block_on(subscription(&host).account_status(&id, host.context())).expect("status");
        assert_eq!(
            reported,
            AccountStatus::NeedsSignIn {
                reason: creds::LAPSED_REASON.to_owned()
            }
        );
        assert_eq!(host.http().calls().len(), 1);
    }
}

#[test]
fn a2c_13_checkin_refreshes_expired_token() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host_at(&dir, after_expiry());
    let id = seed_with_expiry(&host, 1);
    host.http().enqueue(
        json::json!({
            "token": "qoder-access-for-checkin",
            "refresh_token": "qoder-refresh-for-checkin",
            "expires_at": 4_000_000_000_000_i64
        })
        .to_string()
        .into_bytes(),
    );
    host.http().enqueue(
        br#"{"campaigns":[{"campaignId":"daily","actionType":"CLAIM_BENEFIT","claimStatus":"CLAIMABLE","startAt":0,"endAt":4102444800,"benefit":{"amount":100}}]}"#,
    );
    host.http()
        .enqueue(br#"{"status":"CLAIMED","benefit":{"amount":100}}"#);
    let result = block_on(subscription(&host).daily_checkin(&id, host.context())).expect("checkin");
    assert_eq!(result.outcome(), hofund_plugin_api::CheckinOutcome::Claimed);
    let calls = host.http().calls();
    assert_eq!(calls.len(), 3);
    assert!(calls[0].uri().contains("/jobToken/refresh"));
}

#[test]
fn a2c_11_quota_without_usage_fields_is_empty() {
    let windows = quota::windows(br#"{"quota":{"name":"plan"}}"#).expect("windows");
    assert!(windows.is_empty());
}

#[test]
fn a2c_account_id_uses_uid() {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/fixtures/credential.json"
    ))
    .expect("read");
    let mut credential = creds::parse_json(&text).expect("parse");
    let id = creds::account_id(&credential);
    credential.email = "someone-else@example.test".to_owned();
    assert_eq!(id, creds::account_id(&credential));
}

#[test]
fn a2c_11b_quota_without_total_is_empty() {
    let windows = quota::windows(br#"{"quota":{"used":25}}"#).expect("windows");
    assert!(windows.is_empty());
}
