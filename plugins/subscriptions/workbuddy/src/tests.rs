//! Unit tests against recording host backends; no network.

use std::sync::Arc;

use hofund_kit_subscription::{TestHost, block_on};
use hofund_plugin_api::{
    AccountId, AccountStatus, PluginId, PluginKind, ProbeQuery, ProtocolId, RawTable, SecretString,
    Subscription, SubscriptionFactory,
};
use hofund_plugin_sdk::json;
use hofund_plugin_sdk::oauth::{SecretObject, secret_text};

use crate::creds::{self, Credential};
use crate::discover::{self, REFERENCE_CN};
use crate::quota;
use crate::site::Site;
use crate::{Endpoints, contribute, factory_with_extension_data};

const CN: &str = "https://workbuddy-cn.test";
const TOKEN: &str = "wb-access-test";

/// Resolves agent home the way the daemon does when the override env var is unset.
fn daemon_agent_home(user_home: &std::path::Path) -> std::path::PathBuf {
    let decl = contribute()
        .subscriptions()
        .first()
        .expect("factory")
        .spec()
        .agent_sign_in()
        .expect("agent sign-in");
    user_home.join(decl.home_dir())
}

fn install_desktop_auth_fixture(data_root: &std::path::Path) {
    let auth_dir = data_root.join("Public").join("auth");
    std::fs::create_dir_all(&auth_dir).expect("dir");
    std::fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/fixtures/workbuddy-desktop.info"
        ),
        auth_dir.join("workbuddy-desktop.info"),
    )
    .expect("copy");
}

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

fn subscription(
    host: &TestHost,
    extension_data: Option<std::path::PathBuf>,
) -> Arc<dyn Subscription> {
    let factory = factory_with_extension_data(endpoints(), extension_data);
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
        auth_path: String::new(),
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

fn setup_desktop_auth(dir: &tempfile::TempDir) -> std::path::PathBuf {
    let data = dir.path().join("extension-data");
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
    data
}

#[test]
fn a2c_22_discover_import_desktop_auth() {
    let dir = tempfile::tempdir().expect("dir");
    let user_home = dir.path().join("user-home");
    std::fs::create_dir_all(&user_home).expect("dir");
    let agent_home = daemon_agent_home(&user_home);
    install_desktop_auth_fixture(&agent_home);
    let rows = discover::list(&agent_home).expect("list");
    assert_eq!(rows.len(), 1);
    let host = host(&dir);
    let id = block_on(discover::import(&agent_home, REFERENCE_CN, host.context())).expect("import");
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
fn a2c_38_discover_resolves_daemon_agent_home() {
    let dir = tempfile::tempdir().expect("dir");
    let user_home = dir.path().join("injected-home");
    std::fs::create_dir_all(&user_home).expect("dir");
    install_desktop_auth_fixture(&daemon_agent_home(&user_home));
    let rows = discover::list(&daemon_agent_home(&user_home)).expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        contribute().subscriptions()[0]
            .spec()
            .agent_sign_in()
            .expect("decl")
            .home_dir(),
        crate::auth_store::AGENT_HOME_DIR
    );
}

#[test]
fn a2c_22b_desktop_expired_reports_needs_sign_in() {
    let dir = tempfile::tempdir().expect("dir");
    let data = dir.path().join("extension-data");
    let auth_dir = data.join("Public").join("auth");
    std::fs::create_dir_all(&auth_dir).expect("dir");
    let mut info = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/fixtures/workbuddy-desktop.info"
    ))
    .expect("read");
    info = info.replace("4102444800000", "1");
    std::fs::write(auth_dir.join("workbuddy-desktop.info"), info).expect("write");
    let host = host(&dir);
    let id = block_on(discover::import(&data, REFERENCE_CN, host.context())).expect("import");
    let status =
        block_on(subscription(&host, None).account_status(&id, host.context())).expect("status");
    assert!(matches!(status, AccountStatus::NeedsSignIn { .. }));
}

#[test]
fn a2c_36_desktop_rereads_rotated_token_on_upstream() {
    let dir = tempfile::tempdir().expect("dir");
    let data = setup_desktop_auth(&dir);
    let host = host(&dir);
    let id = block_on(discover::import(&data, REFERENCE_CN, host.context())).expect("import");
    let auth_file = data.join("Public/auth/workbuddy-desktop.info");
    let mut info = std::fs::read_to_string(&auth_file).expect("read");
    info = info.replace("wb-access-fixture", "wb-access-rotated-on-disk");
    std::fs::write(&auth_file, info).expect("write");
    let stored = block_on(creds::Stored::load(host.context(), &id)).expect("load");
    assert_eq!(
        secret_text(&stored.credential.access),
        "wb-access-rotated-on-disk"
    );
    host.http().enqueue(
        br#"{"id":"chatcmpl-1","object":"chat.completion","choices":[{"message":{"role":"assistant","content":"ok"}}]}"#,
    );
    let upstream = subscription(&host, None)
        .upstream(&id, host.context())
        .expect("upstream");
    let query = ProbeQuery::new(ProtocolId::OPENAI_CHAT, "deepseek-v3").expect("query");
    block_on(upstream.probe(&query)).expect("probe");
    assert!(
        host.http().calls()[0]
            .sensitive_headers()
            .iter()
            .any(|name| name.eq_ignore_ascii_case("authorization"))
    );
}

#[test]
fn a2c_28_models_from_config_json() {
    let list =
        crate::models::parse_config(br#"{"models":[{"id":"deepseek-v3","name":"DeepSeek V3"}]}"#)
            .expect("parse");
    assert_eq!(list.models()[0].id(), "deepseek-v3");
}

#[test]
fn a2c_37_quota_without_credit_fields_is_empty() {
    let windows = quota::windows(br#"{"data":{"other":1}}"#).expect("q");
    assert!(windows.is_empty());
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
    block_on(subscription(&host, None).refresh(&id, host.context())).expect("refresh");
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
    let upstream = subscription(&host, None)
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
        auth_path: String::new(),
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
    let result =
        block_on(subscription(&host, None).daily_checkin(&id, host.context())).expect("run");
    assert_eq!(result.outcome(), hofund_plugin_api::CheckinOutcome::Claimed);
}

#[test]
fn a2c_22c_agent_home_dir_is_codebuddy_extension_data() {
    let expected = if cfg!(target_os = "macos") {
        "Library/Application Support/CodeBuddyExtension/Data"
    } else if cfg!(target_os = "windows") {
        "AppData/Local/CodeBuddyExtension/Data"
    } else {
        ".local/share/CodeBuddyExtension/Data"
    };
    assert_eq!(crate::auth_store::AGENT_HOME_DIR, expected);
}
