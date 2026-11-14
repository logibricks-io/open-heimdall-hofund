//! Unit tests against recording host backends; no network.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use hofund_kit_subscription::{PipedScript, TestHost, block_on, poll_once};
use hofund_plugin_api::{
    AccountId, AccountStatus, PluginErrorKind, PluginId, PluginKind, ProbeQuery, ProtocolId,
    RawTable, SecretString, SignInFlow, Subscription, SubscriptionFactory,
};
use hofund_plugin_sdk::json;
use hofund_plugin_sdk::oauth::{SecretObject, secret_text};

use crate::creds::{self, Credential, Site, Stored};
use crate::identity::{Devices, TestEnv};
use crate::models;
use crate::quota;
use crate::{Endpoints, Qoder, factory_with_endpoints};

const OPENAPI: &str = "https://qoder.test";
const CHAT: &str = "https://qoder-chat.test";
const TOKEN: &str = "qoder-access-test-token";
const REFRESH: &str = "qoder-refresh-test-token";

fn endpoints() -> Endpoints {
    Endpoints {
        web_global: "https://qoder.test".to_owned(),
        web_cn: "https://qoder-cn.test".to_owned(),
        openapi_global: OPENAPI.to_owned(),
        openapi_cn: "https://qoder-cn.test".to_owned(),
        chat_global: CHAT.to_owned(),
        chat_cn: "https://qoder-cn-chat.test".to_owned(),
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

fn subscription_with_devices(_host: &TestHost, devices: Arc<Devices>) -> Arc<dyn Subscription> {
    Arc::new(Qoder {
        endpoints: Arc::new(endpoints()),
        devices,
        catalog: Arc::new(models::Catalog::default()),
        renewals: Arc::new(Mutex::new(HashMap::new())),
        flow: SignInFlow::Device(
            hofund_plugin_api::DeviceFlow::new("qoder.test")
                .expect("device host")
                .with_path("/device/selectAccounts")
                .expect("device path"),
        ),
        pending: Mutex::new(HashMap::new()),
    })
}

fn test_devices(dir: &tempfile::TempDir) -> Arc<Devices> {
    let runtime = dir.path().join("runtime-info-fixture");
    std::fs::write(&runtime, "fixture").expect("runtime fixture");
    let env = TestEnv::new(dir.path())
        .var("QODER_RUNTIME_INFO", runtime.as_os_str())
        .var("QODERCN_RUNTIME_INFO", runtime.as_os_str());
    Arc::new(Devices::for_test(env, "fixture-host"))
}

fn native_script(environment: &'static str) -> PipedScript {
    PipedScript::new()
        .line(r#"{"machineToken":"fixture-native-token","machineType":"7"}"#)
        .exits(move |spawn| {
            assert_eq!(spawn.arg_strings(), [environment]);
            Vec::new()
        })
}

fn native_models() -> &'static [u8] {
    include_bytes!("fixtures/models.json")
}

fn native_chat(text: &str) -> Vec<u8> {
    let chunk = json::json!({"choices": [{"index": 0, "delta": {"content": text}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 3, "completion_tokens": 1}});
    let envelope = json::json!({"statusCodeValue": 200, "body": chunk.to_string()});
    let done = json::json!({"statusCodeValue": 200, "body": "[DONE]"});
    format!("data: {envelope}\n\ndata: {done}\n\n").into_bytes()
}

macro_rules! header {
    ($call:expr, $name:expr) => {
        ($call).header($name)
    };
}

macro_rules! sensitive {
    ($call:expr, $name:expr) => {
        ($call)
            .sensitive_headers()
            .iter()
            .any(|header| header.eq_ignore_ascii_case($name))
    };
}

fn paired<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

fn seed(host: &TestHost) -> AccountId {
    seed_with_expiry(host, 1)
}

fn seed_with_expiry(host: &TestHost, expires_at: i64) -> AccountId {
    seed_site_with_expiry(host, expires_at, Site::Global)
}

fn seed_site_with_expiry(host: &TestHost, expires_at: i64, site: Site) -> AccountId {
    let credential = Credential {
        uid: "uid-1".to_owned(),
        email: "me@example.test".to_owned(),
        name: String::new(),
        token: SecretString::from(TOKEN.to_owned()),
        refresh_token: SecretString::from(REFRESH.to_owned()),
        expires_at,
        machine_id: "machine".to_owned(),
        site,
        vpc: String::new(),
        device_chat: false,
        device_token: SecretString::from(String::new()),
        device_refresh_token: SecretString::from(String::new()),
        pat_auth: false,
        pat: SecretString::from(String::new()),
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
fn a2c_61_upstream_401_forces_refresh_on_fresh_token() {
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
    host.http().enqueue(native_models());
    host.http().enqueue(native_chat("ok"));
    let upstream = subscription(&host)
        .upstream(&id, host.context())
        .expect("upstream");
    let query = ProbeQuery::new(ProtocolId::OPENAI_CHAT, "qfmodel").expect("query");
    block_on(upstream.probe(&query)).expect("probe after refresh");
    let calls = host.http().calls();
    assert_eq!(calls.len(), 4);
    assert!(calls[0].uri().contains("/algo/api/v2/model/list"));
    assert!(calls[1].uri().contains("/jobToken/refresh"));
    assert!(calls[2].uri().contains("/algo/api/v2/model/list"));
    assert!(calls[3].uri().contains("agent_chat_generation"));
}

#[test]
fn a2c_5_model_list_parsing() {
    let list = models::parse_list(include_bytes!("fixtures/models.json")).expect("parse");
    assert!(list.iter().any(|model| model.key == "qfmodel"));
    assert!(list.iter().any(|model| model.key == "dfmodel"));
}

#[test]
fn a2c_62_model_list_401_keeps_http_status_without_lapsing() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_with_expiry(&host, 4_000_000_000_000);
    host.http().enqueue_response(401, &[], Vec::new());
    let err = block_on(subscription(&host).list_models(&id, host.context())).expect_err("401");
    assert_eq!(err.kind(), PluginErrorKind::Status);
    assert_eq!(err.status(), Some(401));
}

#[test]
fn a2c_6_upstream_round_trip() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_with_expiry(&host, 4_000_000_000_000);
    host.http().enqueue(native_models());
    host.http().enqueue(native_chat("ok"));
    let upstream = subscription(&host)
        .upstream(&id, host.context())
        .expect("upstream");
    let query = ProbeQuery::new(ProtocolId::OPENAI_CHAT, "qfmodel").expect("query");
    block_on(upstream.probe(&query)).expect("probe");
    let calls = host.http().calls();
    assert_eq!(calls.len(), 2);
    assert!(calls[0].uri().contains("/algo/api/v2/model/list"));
    assert!(calls[1].uri().contains("agent_chat_generation"));
}

#[test]
fn a2c_7_quota_windows() {
    let windows = quota::windows(
        br#"{"displayMode":"qoder","qoderUsage":{"userQuota":{"total":100,"used":25}}}"#,
    )
    .expect("windows");
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
        vpc: String::new(),
        device_chat: false,
        device_token: SecretString::from(String::new()),
        device_refresh_token: SecretString::from(String::new()),
        pat_auth: false,
        pat: SecretString::from(String::new()),
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
    host.process().script_piped(native_script("3"));
    host.http().enqueue(
        br#"{"campaigns":[{"campaignId":"daily","actionType":"CLAIM_BENEFIT","claimStatus":"CLAIMABLE","startAt":0,"endAt":4102444800,"benefit":{"amount":100}}]}"#,
    );
    host.http()
        .enqueue(br#"{"status":"CLAIMED","benefit":{"amount":100}}"#);
    let sub = subscription_with_devices(&host, test_devices(&dir));
    let result = block_on(sub.daily_checkin(&id, host.context())).expect("checkin");
    assert_eq!(result.outcome(), hofund_plugin_api::CheckinOutcome::Claimed);
    for call in host.http().calls() {
        assert!(sensitive!(&call, "cosy-machinetoken"));
        assert!(sensitive!(&call, "cosy-machinetype"));
        assert_eq!(header!(&call, "cosy-clienttype"), Some("10"));
        assert_eq!(header!(&call, "user-agent"), Some("Qoder"));
        assert_eq!(
            call.headers()
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case("user-agent"))
                .count(),
            1
        );
    }
}

#[test]
fn a2c_10_account_status_needs_sign_in_when_lapsed_flag_set() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host_at(&dir, after_expiry());
    let id = seed_with_expiry(&host, 1);
    host.http().enqueue_response(401, &[], Vec::new());
    block_on(subscription(&host).refresh(&id, host.context())).expect_err("refused");
    let status = block_on(subscription(&host).account_status(&id, host.context())).expect("status");
    assert_eq!(
        status,
        AccountStatus::NeedsSignIn {
            reason: creds::LAPSED_REASON.to_owned()
        }
    );
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
    host.process().script_piped(native_script("3"));
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
    let sub = subscription_with_devices(&host, test_devices(&dir));
    let result = block_on(sub.daily_checkin(&id, host.context())).expect("checkin");
    assert_eq!(result.outcome(), hofund_plugin_api::CheckinOutcome::Claimed);
    let calls = host.http().calls();
    assert_eq!(calls.len(), 3);
    assert!(calls[0].uri().contains("/jobToken/refresh"));
}

#[test]
fn a2c_11_quota_without_usage_fields_is_empty() {
    let windows = quota::windows(br#"{"displayMode":"qoder","qoderUsage":{"userType":"plan"}}"#)
        .expect("windows");
    assert!(windows.is_empty());
}

#[test]
fn a2c_63_account_id_uses_uid() {
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
fn a2c_64_quota_without_total_is_empty() {
    let windows =
        quota::windows(br#"{"displayMode":"qoder","qoderUsage":{"userQuota":{"used":25}}}"#)
            .expect("windows");
    assert!(windows.is_empty());
}

#[test]
fn r54_chat_uses_cached_native_identity_and_hostname() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_with_expiry(&host, 4_000_000_000_000);
    host.process().script_piped(native_script("3"));
    host.http().enqueue(native_models());
    for _ in 0..2 {
        host.http().enqueue(native_chat("ok"));
    }
    let devices = test_devices(&dir);
    let sub = subscription_with_devices(&host, Arc::clone(&devices));
    let upstream = sub.upstream(&id, host.context()).expect("upstream");
    let query = ProbeQuery::new(ProtocolId::OPENAI_CHAT, "qfmodel").expect("query");
    let stored = block_on(Stored::load(host.context(), &id)).expect("stored");

    let cold = block_on(devices.chat_headers(host.context(), &stored.credential));
    assert_eq!(paired(&cold, "Cosy-MachineToken"), None);
    assert_eq!(paired(&cold, "Cosy-MachineType"), None);
    block_on(upstream.probe(&query)).expect("cold chat");
    assert_eq!(host.process().piped_spawns().len(), 0);

    block_on(devices.checkin_identity(host.context(), &stored.credential))
        .expect("check-in identity")
        .expect("native identity");
    assert_eq!(host.process().piped_spawns().len(), 1);
    block_on(upstream.probe(&query)).expect("cached chat");
    assert_eq!(host.process().piped_spawns().len(), 1);

    let calls = host.http().calls();
    assert_eq!(calls.len(), 3);
    for (index, call) in calls.iter().skip(1).enumerate() {
        for name in ["cosy-machinehostname", "cosy-machineid", "cache-control"] {
            assert!(sensitive!(call, name), "missing {name}");
        }
        assert!(!sensitive!(call, "cosy-clientip"));
        assert_eq!(header!(call, "cosy-clientip"), None);
        if index == 0 {
            assert!(!sensitive!(call, "cosy-machinetoken"));
            assert!(!sensitive!(call, "cosy-machinetype"));
            assert_eq!(header!(call, "cosy-machinetoken"), None);
            assert_eq!(header!(call, "cosy-machinetype"), None);
        } else {
            assert!(sensitive!(call, "cosy-machinetoken"));
            assert!(sensitive!(call, "cosy-machinetype"));
        }
    }
    let headers = block_on(devices.chat_headers(host.context(), &stored.credential));
    assert_eq!(paired(&headers, "Cache-Control"), Some("no-cache"));
    assert_eq!(
        paired(&headers, "Cosy-MachineToken"),
        Some("fixture-native-token")
    );
    assert_eq!(paired(&headers, "Cosy-MachineType"), Some("7"));
    assert_eq!(
        paired(&headers, "Cosy-MachineHostname"),
        Some("fixture-host")
    );
    assert_eq!(paired(&headers, "Cosy-MachineId"), Some("machine"));
    assert_eq!(host.process().piped_spawns().len(), 1);
}

#[test]
fn r54_chat_keeps_cached_identity_during_checkin_refresh() {
    let dir = tempfile::tempdir().expect("dir");
    let initial_host = host(&dir);
    let id = seed_with_expiry(&initial_host, 4_000_000_000_000);
    let devices = test_devices(&dir);
    let stored = block_on(Stored::load(initial_host.context(), &id)).expect("stored");
    initial_host.process().script_piped(native_script("3"));
    block_on(devices.checkin_identity(initial_host.context(), &stored.credential))
        .expect("initial identity")
        .expect("native identity");

    let refresh_host = host_at(
        &dir,
        SystemTime::UNIX_EPOCH + Duration::from_secs(60 * 60 + 1),
    );
    let expired = block_on(devices.chat_headers(refresh_host.context(), &stored.credential));
    assert_eq!(paired(&expired, "Cosy-MachineToken"), None);
    assert_eq!(paired(&expired, "Cosy-MachineType"), None);
    assert_eq!(refresh_host.process().piped_spawns().len(), 0);

    refresh_host.process().script_piped(PipedScript::new());
    let mut refresh =
        Box::pin(devices.checkin_identity(refresh_host.context(), &stored.credential));
    block_on(async {
        assert!(poll_once(refresh.as_mut()).is_pending());
    });
    assert_eq!(refresh_host.process().piped_spawns().len(), 1);

    let headers = block_on(devices.chat_headers(refresh_host.context(), &stored.credential));
    assert_eq!(
        paired(&headers, "Cosy-MachineToken"),
        Some("fixture-native-token")
    );
    assert_eq!(paired(&headers, "Cosy-MachineType"), Some("7"));
    assert_eq!(refresh_host.process().piped_spawns().len(), 1);
}

#[test]
fn r54_cancelled_checkin_refresh_can_be_retried() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_with_expiry(&host, 4_000_000_000_000);
    let devices = test_devices(&dir);
    let stored = block_on(Stored::load(host.context(), &id)).expect("stored");
    host.process().script_piped(PipedScript::new());

    let mut abandoned = Box::pin(devices.checkin_identity(host.context(), &stored.credential));
    block_on(async {
        assert!(poll_once(abandoned.as_mut()).is_pending());
    });
    assert_eq!(host.process().piped_spawns().len(), 1);
    drop(abandoned);

    host.process().script_piped(native_script("3"));
    block_on(devices.checkin_identity(host.context(), &stored.credential))
        .expect("retried identity")
        .expect("native identity");
    assert_eq!(host.process().piped_spawns().len(), 2);
    let headers = block_on(devices.chat_headers(host.context(), &stored.credential));
    assert_eq!(
        paired(&headers, "Cosy-MachineToken"),
        Some("fixture-native-token")
    );
}

#[test]
fn r54_checkin_refreshes_share_one_native_helper() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_with_expiry(&host, 4_000_000_000_000);
    let devices = test_devices(&dir);
    let stored = block_on(Stored::load(host.context(), &id)).expect("stored");
    host.process().script_piped(
        PipedScript::new().line(r#"{"machineToken":"fixture-native-token","machineType":"7"}"#),
    );

    let mut first = Box::pin(devices.checkin_identity(host.context(), &stored.credential));
    let mut second = Box::pin(devices.checkin_identity(host.context(), &stored.credential));
    block_on(async {
        assert!(poll_once(first.as_mut()).is_pending());
        assert!(poll_once(second.as_mut()).is_pending());
    });
    assert_eq!(host.process().piped_spawns().len(), 1);

    drop(first);
    let result = block_on(async {
        match poll_once(second.as_mut()) {
            std::task::Poll::Ready(result) => result,
            std::task::Poll::Pending => {
                panic!("waiter was not completed after leader cancellation")
            }
        }
    });
    let error = result.err().expect("cancelled refresh");
    assert_eq!(error.kind(), PluginErrorKind::Cancelled);
}

#[test]
fn r54_model_list_uses_reference_device_headers() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_with_expiry(&host, 4_000_000_000_000);
    host.http()
        .enqueue(include_bytes!("fixtures/models.json").as_slice());
    block_on(subscription(&host).list_models(&id, host.context())).expect("models");
    let calls = host.http().calls();
    assert_eq!(calls.len(), 1);
    for name in [
        "cosy-clientip",
        "cosy-machineid",
        "cosy-machinetoken",
        "cosy-machinetype",
    ] {
        assert!(sensitive!(&calls[0], name), "missing {name}");
    }
    assert!(!sensitive!(&calls[0], "cosy-machinehostname"));
    assert_eq!(header!(&calls[0], "cosy-machinehostname"), None);
}

#[test]
fn r54_quota_uses_reference_client_headers() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_with_expiry(&host, 4_000_000_000_000);
    host.http()
        .enqueue(br#"{"displayMode":"qoder","qoderUsage":{"userQuota":{"total":100,"used":25}}}"#);
    block_on(subscription(&host).quota(&id, host.context())).expect("quota");
    let calls = host.http().calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(header!(&calls[0], "cosy-clienttype"), Some("10"));
    assert_eq!(header!(&calls[0], "user-agent"), Some("Qoder"));
    assert_eq!(header!(&calls[0], "cosy-machinetoken"), None);
}

#[test]
fn r54_cn_checkin_reads_identity_from_cn_agent_home() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_site_with_expiry(&host, 4_000_000_000_000, Site::China);
    let global = dir.path().join("home/.qoder/.bin");
    let china = dir.path().join("home/.qoder-cn/.bin");
    std::fs::create_dir_all(&global).expect("global cache");
    std::fs::create_dir_all(&china).expect("CN cache");
    std::fs::write(global.join("runtime-info-linux-x64-global"), "fixture").expect("global");
    let cn_runtime = china.join("runtime-info-linux-x64-cn");
    std::fs::write(&cn_runtime, "fixture").expect("CN runtime");
    let devices = Arc::new(Devices::for_test(
        TestEnv::new(dir.path()).home(&dir.path().join("home")),
        "fixture-host",
    ));
    host.process().script_piped(native_script("0"));
    host.http().enqueue(
        br#"{"campaigns":[{"campaignId":"daily","actionType":"CLAIM_BENEFIT","claimStatus":"CLAIMABLE","startAt":0,"endAt":4102444800,"benefit":{"amount":100}}]}"#,
    );
    host.http()
        .enqueue(br#"{"status":"CLAIMED","benefit":{"amount":100}}"#);
    let sub = subscription_with_devices(&host, devices);
    block_on(sub.daily_checkin(&id, host.context())).expect("CN check-in");
    let spawns = host.process().piped_spawns();
    assert_eq!(spawns.len(), 1);
    assert_eq!(spawns[0].program, cn_runtime);
    for call in host.http().calls() {
        assert!(sensitive!(&call, "cosy-machinetoken"));
        assert!(sensitive!(&call, "cosy-machinetype"));
    }
}
