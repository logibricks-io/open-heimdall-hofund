//! Unit tests against recording host backends; no network.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use hofund_kit_subscription::{TestHost, block_on};
use hofund_plugin_api::{
    AccountId, AccountStatus, PluginErrorKind, PluginId, PluginKind, ProbeQuery, ProtocolId,
    RawTable, SecretString, SignInChallenge, SignInCtx, SignInProof, Subscription,
    SubscriptionFactory,
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
    host_at(dir, SystemTime::UNIX_EPOCH)
}

fn host_at(dir: &tempfile::TempDir, now: SystemTime) -> TestHost {
    TestHost::open_for_at(
        PluginKind::Subscription,
        PluginId::from_static("workbuddy"),
        dir.path(),
        now,
    )
}

fn after_expiry() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(10_000)
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
    seed_site(host, expires, Site::China)
}

fn seed_site(host: &TestHost, expires: i64, site: Site) -> AccountId {
    let credential = Credential {
        uid: format!("uid-{}", site.plugin_id()),
        access: SecretString::from(TOKEN.to_owned()),
        refresh: SecretString::from("wb-refresh-test".to_owned()),
        expires_at: expires,
        refresh_expires_at: 4_000_000_000_000,
        domain: "www.codebuddy.cn".to_owned(),
        token_type: "Bearer".to_owned(),
        label: "wb-user".to_owned(),
        site,
        desktop: false,
        api_key: false,
        enterprise_id: String::new(),
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
    let list = crate::models::parse_config(
        br#"{"models":[{"id":"deepseek-v3","name":"DeepSeek V3"}]}"#,
        Site::China,
    )
    .expect("parse");
    assert_eq!(list.models()[0].id(), "deepseek-v3");
}

#[test]
fn r55c_model_images_follow_site_and_edit_tags() {
    let body = br#"{"data":{"models":[
        {"id":"kimi-k3","supportsImages":true,"maxInputTokens":1000,"maxOutputTokens":100},
        {"id":"glm-5.3","supportsImages":true},
        {"id":"hunyuan-image-alpha","tags":["text-to-image"]},
        {"id":"hunyuan-image-alpha-edit","tags":["image-to-image"]}
    ]}}"#;
    let international = crate::models::parse_config(body, Site::International).expect("parse");
    let kimi = &international.models()[0];
    assert_eq!(kimi.image_input(), Some(true));
    assert_eq!(kimi.input_modalities(), ["text", "image"]);
    assert_eq!(kimi.context(), 1000);
    assert_eq!(kimi.max_output(), 100);
    assert_eq!(international.models()[1].image_input(), Some(false));
    let drawer = &international.models()[2];
    assert!(drawer.draws());
    assert_eq!(drawer.image_input(), Some(true));
    let china = crate::models::parse_config(body, Site::China).expect("parse");
    assert_eq!(china.models()[1].image_input(), Some(true));

    for site in [Site::China, Site::International] {
        let declared = crate::models::declared(site);
        for model in declared.models() {
            let expected = !(site == Site::International && model.id().starts_with("glm-"));
            assert_eq!(
                model.image_input(),
                Some(expected),
                "{}:{}",
                site.plugin_id(),
                model.id()
            );
        }
    }
    let declared = crate::models::declared(Site::International);
    let kimi = declared
        .models()
        .iter()
        .find(|model| model.id() == "kimi-k3")
        .expect("kimi");
    assert_eq!(kimi.input_modalities(), ["text", "image"]);
}

#[test]
fn a2c_37_quota_without_credit_fields_is_empty() {
    let summary = quota::summary(br#"{"data":{"other":1}}"#, Site::China).expect("q");
    assert!(summary.windows.is_empty());
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
fn r55_refresh_accepts_enveloped_tokens_and_relative_expiry() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host, 1);
    host.http().enqueue(
        json::json!({
            "code": 0,
            "data": {
                "accessToken": "wb-access-enveloped",
                "refreshToken": "wb-refresh-enveloped",
                "expiresIn": 3600,
                "refreshExpiresIn": 7200
            }
        })
        .to_string()
        .into_bytes(),
    );
    block_on(subscription(&host, None).refresh(&id, host.context())).expect("refresh");
    let stored = block_on(creds::Stored::load(host.context(), &id)).expect("stored");
    assert_eq!(
        secret_text(&stored.credential.access),
        "wb-access-enveloped"
    );
    assert_eq!(
        secret_text(&stored.credential.refresh),
        "wb-refresh-enveloped"
    );
    assert_eq!(stored.credential.expires_at, 3_600_000);
    assert_eq!(stored.credential.refresh_expires_at, 7_200_000);
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
    assert_eq!(call.uri(), format!("{CN}/v2/chat/completions"));
    assert_eq!(call.header("content-encoding"), Some("gzip"));
    let body = hofund_plugin_sdk::inflate::gzip(call.body()).expect("gzip chat body");
    let body: json::Value = json::from_slice(&body).expect("chat JSON");
    assert_eq!(
        body.get("model").and_then(json::Value::as_str),
        Some("deepseek-v3")
    );
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
    let summary = quota::summary(
        br#"{"data":{"Packages":[{"CycleTotalCapacity":"100","CycleUsedCapacity":"10"}]}}"#,
        Site::China,
    )
    .expect("q");
    assert_eq!(summary.windows[0].name(), "Credits");
    assert_eq!(summary.windows[0].used(), 10);
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
        api_key: false,
        enterprise_id: String::new(),
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
fn a2c_39_checkin_refreshes_expired_token() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host_at(&dir, after_expiry());
    let id = seed(&host, 1);
    host.http().enqueue(
        json::json!({
            "accessToken": "wb-access-for-checkin",
            "refreshToken": "wb-refresh-for-checkin",
            "expiresAt": 4_000_000_000_000_i64
        })
        .to_string()
        .into_bytes(),
    );
    host.http().enqueue(
        br#"{"code":0,"msg":"ok","data":{"active":true,"today_checked_in":false,"streak_days":"1","today_credit":"0"}}"#,
    );
    host.http().enqueue(
        br#"{"code":0,"msg":"ok","data":{"credit":"5","streak_days":"2","message":"ok"}}"#,
    );
    let result =
        block_on(subscription(&host, None).daily_checkin(&id, host.context())).expect("run");
    assert_eq!(result.outcome(), hofund_plugin_api::CheckinOutcome::Claimed);
    let calls = host.http().calls();
    assert_eq!(calls.len(), 3);
    assert!(calls[0].uri().contains("/token/refresh"));
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

#[test]
fn r55_upstream_restriction_rests_account_with_reference_hint() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host, 4_000_000_000_000);
    host.http().enqueue_response(
        403,
        &[],
        br#"{"data":{"code":"11140"},"msg":"request illegal"}"#.to_vec(),
    );
    let upstream = subscription(&host, None)
        .upstream(&id, host.context())
        .expect("upstream");
    let query = ProbeQuery::new(ProtocolId::OPENAI_CHAT, "deepseek-v3").expect("query");
    let error = block_on(upstream.probe(&query)).expect_err("restricted");
    assert_eq!(error.status(), Some(403));
    assert_eq!(error.retry_after(), Some(Duration::from_secs(2 * 60 * 60)));
    assert!(error.message().ends_with(crate::errors::RESTRICTED_HINT));
}

#[test]
fn r55b_enterprise_profile_usage_uses_enterprise_meter() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host, 4_000_000_000_000);
    host.http().enqueue(
        br#"{"code":0,"data":{"Packages":[],"SubscriptionPackageCode":"","IsPaidUser":false}}"#,
    );
    host.http().enqueue(
        br#"{"code":0,"data":{"credit":196.13,"cycleStartTime":"2026-10-08 00:00:00","cycleResetTime":"2026-11-09 00:00:00","limitNum":2000}}"#,
    );
    let windows = block_on(subscription(&host, None).quota(&id, host.context())).expect("quota");
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0].name(), "Credits");
    assert_eq!(windows[0].used(), 10);
    assert_eq!(
        windows[0].resets_at(),
        hofund_plugin_sdk::time::parse_rfc3339("2026-11-08T16:00:00Z")
    );
    let calls = host.http().calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[0].uri(),
        format!("{CN}/billing/meter/get-user-resource-summary")
    );
    assert_eq!(
        calls[1].uri(),
        format!("{CN}/billing/meter/get-enterprise-user-usage")
    );
    for call in &calls {
        assert_eq!(call.body(), b"{}");
        assert_eq!(call.header("content-encoding"), None);
    }
}

#[test]
fn r55b_refused_enterprise_meter_leaves_summary_empty() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host, 4_000_000_000_000);
    host.http().enqueue(br#"{"code":0,"data":{"Packages":[]}}"#);
    host.http().enqueue_response(
        400,
        &[],
        br#"{"code":10001,"msg":"uid or enterpriseID is empty"}"#.to_vec(),
    );
    let windows = block_on(subscription(&host, None).quota(&id, host.context())).expect("quota");
    assert!(windows.is_empty());
    assert_eq!(host.http().calls().len(), 2);
}

#[test]
fn r55b_profile_usage_adds_listed_extra_and_plan_reset() {
    let dir = tempfile::tempdir().expect("dir");
    let now = hofund_plugin_sdk::time::parse_rfc3339("2026-10-26T06:00:00Z").expect("now");
    let host = host_at(&dir, now);
    let id = seed(&host, 4_000_000_000_000);
    host.http().enqueue(
        br#"{"code":0,"data":{"SubscriptionPackageCode":"","Packages":[{"PackageCode":"TCACA_code_008_cfWoLwvjU4","CycleTotalCapacity":"500","CycleUsedCapacity":"0"}]}}"#,
    );
    host.http()
        .enqueue(br#"{"code":0,"data":{"TotalCount":0,"Accounts":[]}}"#);
    host.http().enqueue(
        br#"{"code":0,"data":{"TotalCount":2,"Accounts":[
            {"PackageCode":"TCACA_code_007_nzdH5h4Nl0","CapacityType":1,"CycleCapacitySizePrecise":"300","CycleCapacityRemainPrecise":"280","DeductionEndTime":1793300000000,"CycleEndTime":"2026-10-30 01:34:42"},
            {"PackageCode":"TCACA_code_008_cfWoLwvjU4","CapacityType":4,"CycleCapacitySizePrecise":"500","CycleCapacityRemainPrecise":"500","DeductionEndTime":2050936768000,"CycleStartTime":"2026-10-01 00:00:00","CycleEndTime":"2026-10-31 23:59:59"}
        ]}}"#,
    );
    let windows = block_on(subscription(&host, None).quota(&id, host.context())).expect("quota");
    assert_eq!(windows.len(), 2);
    assert_eq!(windows[0].name(), "Credits");
    assert_eq!(windows[0].used(), 0);
    assert!(windows[0].blocking());
    assert_eq!(
        windows[0].resets_at(),
        hofund_plugin_sdk::time::parse_rfc3339("2026-10-31T15:59:59Z")
    );
    assert_eq!(windows[1].name(), "Extra credits");
    assert_eq!(windows[1].used(), 7);
    assert!(!windows[1].blocking());

    let calls = host.http().calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(
        calls[1].uri(),
        format!("{CN}/billing/meter/get-user-resource-paid-packages")
    );
    assert_eq!(
        calls[2].uri(),
        format!("{CN}/billing/meter/get-user-resource-free-packages")
    );
    let paid: json::Value = json::from_slice(calls[1].body()).expect("paid request");
    assert_eq!(
        paid.get("PageNumber").and_then(json::Value::as_u64),
        Some(1)
    );
    assert_eq!(
        paid.get("PageSize").and_then(json::Value::as_u64),
        Some(200)
    );
    assert_eq!(
        paid.get("NeedRenewInfo").and_then(json::Value::as_bool),
        Some(true)
    );
    assert!(
        paid.get("PackageCodes")
            .and_then(json::Value::as_array)
            .is_some_and(|codes| codes.iter().any(|code| code == "TCACA_code_002_AkiJS3ZHF5"))
    );
    let free: json::Value = json::from_slice(calls[2].body()).expect("free request");
    assert_eq!(
        free.get("SlicePeriodStartTime")
            .and_then(json::Value::as_str),
        Some("2026-10-26 00:00:00")
    );
    assert_eq!(
        free.get("SlicePeriodEndTime").and_then(json::Value::as_str),
        Some("2026-10-26 23:59:59")
    );
    for call in &calls {
        assert_eq!(call.header("content-encoding"), None);
    }
}

fn sign_in_ctx(site: &str) -> SignInCtx {
    SignInCtx::new(PluginId::from_static("workbuddy"))
        .with_session("0123456789abcdef0123456789abcdef")
        .expect("session")
        .with_option("site", site)
        .expect("site")
}

#[test]
fn r55_browser_sign_in_works_for_both_sites() {
    for (site, base, uid, label, enterprise) in [
        ("workbuddy", CN, "browser-cn", "Browser User", ""),
        (
            "workbuddy-ai",
            "https://workbuddy-ai.test",
            "browser-ai",
            "Browser User · Team One",
            "enterprise-1",
        ),
    ] {
        let dir = tempfile::tempdir().expect("dir");
        let host = host(&dir);
        let sub = subscription(&host, None);
        host.http().enqueue(
            json::json!({
                "code": 0,
                "data": {"state": "browser-state", "authUrl": "https://login.test/authorize"}
            })
            .to_string()
            .into_bytes(),
        );
        let challenge =
            block_on(sub.start_sign_in(&sign_in_ctx(site), host.context())).expect("browser start");
        let SignInChallenge::Instructions { text } = challenge else {
            panic!("browser instructions");
        };
        assert!(text.contains("https://login.test/authorize?"));
        assert!(text.contains("version=2.0.0"));
        assert!(text.contains("loginSessionId="));
        host.http().enqueue(
            json::json!({
                "code": 0,
                "data": {
                    "accessToken": "browser-access-fixture",
                    "refreshToken": "browser-refresh-fixture",
                    "expiresIn": 3600
                }
            })
            .to_string()
            .into_bytes(),
        );
        host.http().enqueue(
            json::json!({
                "code": 0,
                "data": {
                    "uid": uid,
                    "nickname": "Browser User",
                    "type": if enterprise.is_empty() { "personal" } else { "enterprise" },
                    "enterpriseId": enterprise,
                    "enterpriseName": if enterprise.is_empty() { "" } else { "Team One" }
                }
            })
            .to_string()
            .into_bytes(),
        );
        let id =
            block_on(sub.complete_sign_in(&sign_in_ctx(site), SignInProof::Await, host.context()))
                .expect("browser complete");
        let stored = block_on(creds::Stored::load(host.context(), &id)).expect("stored");
        assert_eq!(stored.credential.site.plugin_id(), site);
        assert_eq!(stored.credential.uid, uid);
        assert_eq!(stored.credential.label, label);
        assert_eq!(stored.credential.enterprise_id, enterprise);
        assert!(!stored.credential.desktop);
        assert!(!stored.credential.api_key);
        let calls = host.http().calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(
            calls[0].uri(),
            format!("{base}/v2/plugin/auth/state?platform={site}")
        );
        assert_eq!(calls[0].header("x-no-authorization"), Some("true"));
        assert_eq!(calls[0].header("x-no-user-id"), Some("true"));
        assert_eq!(calls[0].header("x-no-enterprise-id"), Some("true"));
        assert_eq!(calls[0].header("x-no-department-info"), Some("true"));
        assert_eq!(
            calls[1].uri(),
            format!("{base}/v2/plugin/auth/token?state=browser-state")
        );
        assert_eq!(
            calls[2].uri(),
            format!("{base}/v2/plugin/login/account?state=browser-state")
        );
        assert!(
            calls[2]
                .sensitive_headers()
                .iter()
                .any(|name| name == "authorization")
        );
        assert_eq!(calls[2].header("x-no-user-id"), Some("true"));
        assert_eq!(calls[2].header("x-no-enterprise-id"), Some("true"));
    }
}

#[test]
fn r55_api_key_sign_in_works_for_both_sites_without_exposing_key() {
    let key = "ck_test0000000000abcdefgh";
    for site in ["workbuddy", "workbuddy-ai"] {
        let dir = tempfile::tempdir().expect("dir");
        let host = host(&dir);
        let sub = subscription(&host, None);
        let id = block_on(sub.complete_sign_in(
            &sign_in_ctx(site),
            SignInProof::Token(SecretString::from(key.to_owned())),
            host.context(),
        ))
        .expect("key import");
        assert!(host.http().calls().is_empty());
        assert!(!format!("{id:?}").contains(key));
        let stored = block_on(creds::Stored::load(host.context(), &id)).expect("stored");
        assert_eq!(stored.credential.uid, "anonymous_abcdefgh");
        assert_eq!(stored.credential.site.plugin_id(), site);
        assert!(stored.credential.api_key);
        assert!(stored.credential.enterprise_id.is_empty());
        assert!(stored.credential.domain.is_empty());
        assert_eq!(
            secret_text(&stored.credential.access),
            "ck_test0000000000abcdefgh"
        );
        assert_eq!(
            block_on(sub.account_status(&id, host.context())).expect("status"),
            AccountStatus::Ready
        );
        assert!(
            block_on(sub.quota(&id, host.context()))
                .expect("key quota")
                .is_empty()
        );
        assert!(host.http().calls().is_empty());
        host.http().enqueue(
            br#"{"id":"chatcmpl-1","object":"chat.completion","choices":[{"message":{"role":"assistant","content":"ok"}}]}"#,
        );
        let upstream = sub.upstream(&id, host.context()).expect("upstream");
        let query = ProbeQuery::new(ProtocolId::OPENAI_CHAT, "deepseek-v3").expect("query");
        block_on(upstream.probe(&query)).expect("key chat");
        let calls = host.http().calls();
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0]
                .sensitive_headers()
                .iter()
                .any(|name| name == "x-api-key")
        );
        assert!(
            calls[0]
                .sensitive_headers()
                .iter()
                .any(|name| name == "authorization")
        );
        assert!(
            !calls[0]
                .sensitive_headers()
                .iter()
                .any(|name| name == "x-domain")
        );
    }
}

#[test]
fn r55_empty_key_and_insecure_browser_page_fail_closed() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host, None);
    let error = block_on(sub.complete_sign_in(
        &sign_in_ctx("workbuddy"),
        SignInProof::Token(SecretString::from("  ".to_owned())),
        host.context(),
    ))
    .expect_err("empty key");
    assert_eq!(error.kind(), PluginErrorKind::Auth);
    assert!(host.http().calls().is_empty());

    host.http()
        .enqueue(br#"{"code":0,"data":{"state":"state","authUrl":"http://login.test/authorize"}}"#);
    let error = block_on(sub.start_sign_in(&sign_in_ctx("workbuddy"), host.context()))
        .expect_err("insecure page");
    assert!(error.message().contains("isn't https"));
    let error = block_on(sub.complete_sign_in(
        &sign_in_ctx("workbuddy"),
        SignInProof::Await,
        host.context(),
    ))
    .expect_err("no pending");
    assert_eq!(error.kind(), PluginErrorKind::Config);
    assert_eq!(host.http().calls().len(), 1);
}

#[test]
fn r55_unknown_sign_in_site_is_rejected_without_network() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host, None);
    let error = block_on(sub.start_sign_in(&sign_in_ctx("unknown"), host.context()))
        .expect_err("unknown site");
    assert_eq!(error.kind(), PluginErrorKind::Config);
    assert!(host.http().calls().is_empty());
}

fn chat_body(bytes: &[u8]) -> json::Value {
    let body = hofund_plugin_sdk::inflate::gzip(bytes).expect("gzip chat body");
    json::from_slice(&body).expect("chat JSON")
}

fn after_tools_request(model: &str, prefill: bool) -> hofund_plugin_api::Request {
    use hofund_plugin_api::{ContentPart, Extensions, Message, Request, Role, ToolResultPart};

    let result = Message {
        role: Role::Tool,
        content: vec![ContentPart::ToolResult(ToolResultPart {
            id: "call_1".to_owned(),
            is_error: false,
            content: Vec::new(),
            extensions: Extensions::new(),
        })],
        extensions: Extensions::new(),
    };
    Request::new(model).with_messages(vec![
        Message::new(Role::User),
        result,
        Message::new(if prefill { Role::Assistant } else { Role::User }),
    ])
}

#[test]
fn r55c_after_tools_refusal_retries_once_and_learns_per_model() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host, 4_000_000_000_000);
    host.http().enqueue_response(
        400,
        &[],
        br#"{"code":11153,"msg":"the conversation must end with a user message"}"#.to_vec(),
    );
    for _ in 0..3 {
        host.http().enqueue(
            br#"{"id":"chatcmpl-1","object":"chat.completion","choices":[{"message":{"role":"assistant","content":"ok"}}]}"#,
        );
    }
    let upstream = subscription(&host, None)
        .upstream(&id, host.context())
        .expect("upstream");
    for model in [
        "gemini-after-tools",
        "gemini-after-tools",
        "deepseek-after-tools",
    ] {
        block_on(upstream.execute(after_tools_request(model, false), ProtocolId::OPENAI_CHAT))
            .expect("served");
    }
    let calls = host.http().calls();
    assert_eq!(calls.len(), 4);
    let roles = |at: usize| {
        chat_body(calls[at].body())["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .map(|message| message["role"].as_str().expect("role").to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(roles(0), ["user", "tool", "user"]);
    assert_eq!(roles(1), ["user", "tool", "assistant", "user"]);
    assert_eq!(chat_body(calls[1].body())["messages"][2]["content"], "");
    assert_eq!(roles(2), ["user", "tool", "assistant", "user"]);
    assert_eq!(roles(3), ["user", "tool", "user"]);
}

#[test]
fn r55c_after_tools_prefill_is_not_retried() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host, 4_000_000_000_000);
    host.http().enqueue_response(
        400,
        &[],
        br#"{"code":11153,"msg":"the conversation must end with a user message"}"#.to_vec(),
    );
    let upstream = subscription(&host, None)
        .upstream(&id, host.context())
        .expect("upstream");
    let error = block_on(upstream.execute(
        after_tools_request("gemini-after-tools", true),
        ProtocolId::OPENAI_CHAT,
    ))
    .err()
    .expect("prefill refused");
    assert_eq!(error.status(), Some(400));
    assert_eq!(host.http().calls().len(), 1);
}

#[test]
fn r55c_image_generation_and_edit_are_plain_json_for_both_sites() {
    use hofund_plugin_api::RawBody;

    for site in [Site::China, Site::International] {
        let dir = tempfile::tempdir().expect("dir");
        let host = host(&dir);
        let id = seed_site(&host, 4_000_000_000_000, site);
        let upstream = subscription(&host, None)
            .upstream(&id, host.context())
            .expect("upstream");
        host.http()
            .enqueue(br#"{"code":0,"msg":"ok","data":{"created":1,"data":[{"b64_json":"aGk="}]}}"#);
        let answer = block_on(upstream.forward(
            "/images/generations",
            RawBody::new(
                "application/json",
                br#"{"model":"gpt-image-2.5-sunburst","prompt":"a bird","n":1,"size":"1024x1024","response_format":"url","unknown":true}"#.to_vec(),
            ),
        ))
        .expect("generation");
        let generated: json::Value = json::from_slice(answer.body()).expect("answer");
        assert_eq!(generated["data"][0]["b64_json"], "aGk=");
        let calls = host.http().calls();
        assert_eq!(calls.len(), 1);
        let base = endpoints();
        let base = base.base_for(site);
        assert_eq!(calls[0].uri(), format!("{base}/v2/images/generations"));
        assert_eq!(calls[0].header("content-encoding"), None);
        assert_eq!(calls[0].body()[0], b'{');
        let sent: json::Value = json::from_slice(calls[0].body()).expect("request");
        assert_eq!(sent["response_format"], "b64_json");
        assert!(sent.get("unknown").is_none());

        host.http().enqueue(
            br#"{"code":0,"data":{"models":[{"id":"hunyuan-image-alpha","tags":["text-to-image"]},{"id":"hunyuan-image-alpha-edit","tags":["image-to-image"]}]}}"#,
        );
        host.http().enqueue(
            br#"{"code":0,"msg":"ok","data":{"created":1,"data":[{"url":"https://image.test/a.png"}]}}"#,
        );
        block_on(upstream.forward(
            "/images/edits",
            RawBody::new(
                "application/json",
                br#"{"model":"hunyuan-image-alpha","prompt":"make it red","n":1,"size":"auto","response_format":"b64_json","image":["data:image/png;base64,aGk="]}"#.to_vec(),
            ),
        ))
        .expect("edit");
        let calls = host.http().calls();
        assert_eq!(calls.len(), 3);
        assert!(calls[1].uri().ends_with("/v3/config"));
        assert_eq!(calls[2].uri(), format!("{base}/v2/images/edits"));
        assert_eq!(calls[2].header("content-encoding"), None);
        let sent: json::Value = json::from_slice(calls[2].body()).expect("request");
        assert_eq!(sent["model"], "hunyuan-image-alpha-edit");
        assert_eq!(sent["image"][0], "data:image/png;base64,aGk=");
        assert!(sent.get("size").is_none());
        assert!(sent.get("response_format").is_none());
    }
}

#[test]
fn r55c_image_envelope_refusal_and_unsupported_path_are_safe() {
    use hofund_plugin_api::RawBody;

    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host, 4_000_000_000_000);
    let upstream = subscription(&host, None)
        .upstream(&id, host.context())
        .expect("upstream");
    host.http()
        .enqueue(br#"{"code":11101,"msg":"invalid image"}"#);
    let error = block_on(upstream.forward(
        "/images/generations",
        RawBody::new(
            "application/json",
            br#"{"model":"gpt-image-2.5-sunburst","prompt":"a bird","n":1}"#.to_vec(),
        ),
    ))
    .expect_err("refused");
    assert_eq!(error.status(), Some(502));
    assert!(error.message().contains("invalid image (11101)"));
    assert!(!error.message().contains(TOKEN));

    let error = block_on(upstream.forward(
        "/embeddings",
        RawBody::new("application/json", b"{}".to_vec()),
    ))
    .expect_err("unsupported");
    assert_eq!(error.kind(), PluginErrorKind::Unsupported);
    assert_eq!(host.http().calls().len(), 1);
}
