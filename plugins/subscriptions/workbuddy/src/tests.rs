//! Unit tests against recording host backends; no network.

use std::sync::Arc;

use hofund_kit_subscription::{TestHost, block_on};
use hofund_plugin_api::{
    AccountId, PluginId, PluginKind, ProbeQuery, ProtocolId, RawTable, SecretString, Subscription,
    SubscriptionFactory,
};
use hofund_plugin_sdk::json;
use hofund_plugin_sdk::oauth::{SecretObject, secret_text};

use crate::creds::{self, Credential};
use crate::discover::{self, REFERENCE_CN};
use crate::quota;
use crate::site::Site;
use crate::{Endpoints, factory_with_endpoints};

const CN: &str = "https://workbuddy-cn.test";
const TOKEN: &str = "wb-access-test";

fn endpoints() -> Endpoints {
    Endpoints {
        cn: CN.to_owned(),
        ai: "https://workbuddy-ai.test".to_owned(),
    }
}

fn host(dir: &tempfile::TempDir) -> TestHost {
    TestHost::open_for(
        PluginKind::Subscription,
        PluginId::from_static("workbuddy"),
        dir.path(),
    )
}

fn subscription(host: &TestHost) -> Arc<dyn Subscription> {
    let factory = factory_with_endpoints(endpoints());
    let cfg = factory.validate(&RawTable::new()).expect("config");
    factory.build(&cfg, host.context()).expect("build")
}

fn seed(host: &TestHost, expires: i64) -> AccountId {
    let credential = Credential {
        uid: "uid-cn".to_owned(),
        access: SecretString::from(TOKEN.to_owned()),
        refresh: SecretString::from("wb-refresh-test".to_owned()),
        expires_at: expires,
        refresh_expires_at: 4_000_000_000_000,
        domain: "www.codebuddy.cn".to_owned(),
        token_type: "Bearer".to_owned(),
        label: "wb-user".to_owned(),
        site: Site::China,
        desktop: false,
    };
    let id = creds::account_id(&credential);
    block_on(async {
        let accounts = host.context().accounts();
        let account = accounts
            .account(id.clone(), "wb-user".to_owned())
            .expect("account");
        accounts
            .upsert(account, creds::secret_blob(&credential).to_secret())
            .await
            .expect("upsert");
    });
    id
}

#[test]
fn a2c_22_discover_import_desktop_auth() {
    let dir = tempfile::tempdir().expect("dir");
    let data = dir.path().join("CodeBuddyExtension").join("Data");
    let auth_dir = data.join("Public").join("auth");
    std::fs::create_dir_all(&auth_dir).expect("dir");
    std::fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/fixtures/workbuddy-desktop.info"
        ),
        auth_dir.join("workbuddy-desktop.info"),
    )
    .expect("copy");
    crate::auth_store::set_test_extension_data(Some(data.clone()));
    let rows = discover::list(dir.path()).expect("list");
    assert_eq!(rows.len(), 1);
    let host = host(&dir);
    let id = block_on(discover::import(dir.path(), REFERENCE_CN, host.context())).expect("import");
    crate::auth_store::set_test_extension_data(None);
    let secret = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    assert_eq!(
        SecretObject::parse(&secret)
            .expect("blob")
            .secret("access_token")
            .as_ref()
            .map(secret_text),
        Some("wb-access-fixture")
    );
}

#[test]
fn a2c_22b_desktop_expired_reports_needs_sign_in() {
    let dir = tempfile::tempdir().expect("dir");
    let data = dir.path().join("data");
    let auth_dir = data.join("Public").join("auth");
    std::fs::create_dir_all(&auth_dir).expect("dir");
    let mut info = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/fixtures/workbuddy-desktop.info"
    ))
    .expect("read");
    info = info.replace("4102444800000", "1");
    std::fs::write(auth_dir.join("workbuddy-desktop.info"), info).expect("write");
    crate::auth_store::set_test_extension_data(Some(data.clone()));
    let host = host(&dir);
    let id = block_on(discover::import(dir.path(), REFERENCE_CN, host.context())).expect("import");
    crate::auth_store::set_test_extension_data(None);
    let status = block_on(subscription(&host).account_status(&id, host.context())).expect("status");
    assert!(matches!(
        status,
        hofund_plugin_api::AccountStatus::NeedsSignIn { .. }
    ));
}

#[test]
fn a2c_28_models_from_config_json() {
    let list =
        crate::models::parse_config(br#"{"models":[{"id":"deepseek-v3","name":"DeepSeek V3"}]}"#)
            .expect("parse");
    assert_eq!(list.models()[0].id(), "deepseek-v3");
}

#[test]
fn a2c_23_refresh_on_expiry() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host, 1);
    host.http().enqueue(
        json::json!({
            "accessToken": "wb-access-renewed",
            "refreshToken": "wb-refresh-renewed",
            "expiresAt": 4_000_000_000_000_i64
        })
        .to_string()
        .into_bytes(),
    );
    block_on(subscription(&host).refresh(&id, host.context())).expect("refresh");
    assert!(host.http().calls()[0].uri().contains("/token/refresh"));
}

#[test]
fn a2c_24_upstream_sends_workbuddy_user_agent() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host, 4_000_000_000_000);
    host.http().enqueue(
        br#"{"id":"chatcmpl-1","object":"chat.completion","choices":[{"message":{"role":"assistant","content":"ok"}}]}"#,
    );
    let upstream = subscription(&host)
        .upstream(&id, host.context())
        .expect("upstream");
    let query = ProbeQuery::new(ProtocolId::OPENAI_CHAT, "deepseek-v3").expect("query");
    block_on(upstream.probe(&query)).expect("probe");
    let call = &host.http().calls()[0];
    assert!(
        call.sensitive_headers()
            .iter()
            .any(|name| name.eq_ignore_ascii_case("user-agent"))
    );
    assert!(
        call.sensitive_headers()
            .iter()
            .any(|name| name.eq_ignore_ascii_case("x-user-id"))
    );
}

#[test]
fn a2c_25_quota_from_billing_mock() {
    let windows = quota::windows(br#"{"data":{"usedCredits":10,"totalCredits":100}}"#).expect("q");
    assert_eq!(windows[0].name(), "Credits");
}

#[test]
fn a2c_26_secrets_not_in_debug() {
    let key = "wb-secret-token";
    let credential = Credential {
        uid: "u".to_owned(),
        access: SecretString::from(key.to_owned()),
        refresh: SecretString::from("r".to_owned()),
        expires_at: 0,
        refresh_expires_at: 0,
        domain: String::new(),
        token_type: String::new(),
        label: String::new(),
        site: Site::China,
        desktop: false,
    };
    assert!(!format!("{credential:?}").contains(key));
}

#[test]
fn a2c_27_daily_checkin_cn_only() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host, 4_000_000_000_000);
    host.http().enqueue(
        br#"{"code":0,"msg":"ok","data":{"active":true,"today_checked_in":false,"streak_days":"1","today_credit":"0"}}"#,
    );
    host.http().enqueue(
        br#"{"code":0,"msg":"ok","data":{"credit":"5","streak_days":"2","message":"ok"}}"#,
    );
    let result = block_on(subscription(&host).daily_checkin(&id, host.context())).expect("run");
    assert_eq!(result.outcome(), hofund_plugin_api::CheckinOutcome::Claimed);
}
