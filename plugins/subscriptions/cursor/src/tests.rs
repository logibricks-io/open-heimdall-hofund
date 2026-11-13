//! Unit tests with recording backends; no network.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use hofund_kit_subscription::{
    PipedScript, TestHost, advance_time, block_on, block_on_paused, join_all, yield_times,
};
use hofund_plugin_api::{
    PluginErrorKind, PluginId, PluginKind, RawTable, RawValue, SignInCtx, SignInProof,
    Subscription, SubscriptionFactory,
};
use hofund_plugin_sdk::oauth::{SecretObject, account_id_from};

use crate::{Endpoints, auth, factory_with_endpoints, tokens};

const SESSION: &str = "0123456789abcdef0123456789abcdef";
#[cfg(not(windows))]
const PROGRAM: &str = "/opt/cursor-agent/bin/cursor-agent";
#[cfg(windows)]
const PROGRAM: &str = r"C:\cursor-agent\bin\cursor-agent.exe";

const MODELS_FIXTURE: &str = include_str!("fixtures/models_list.txt");

fn plugin() -> PluginId {
    PluginId::from_static("cursor")
}

fn endpoints() -> Endpoints {
    Endpoints {
        website: "https://cursor.test".to_owned(),
        api: "https://api2.cursor.test".to_owned(),
        agent: "https://agent.cursor.test".to_owned(),
        client_version: "cli-test".to_owned(),
    }
}

fn host(dir: &tempfile::TempDir) -> TestHost {
    TestHost::open_for(PluginKind::Subscription, plugin(), dir.path())
}

fn subscription(host: &TestHost) -> Arc<dyn Subscription> {
    subscription_with_agent_home(host, None)
}

fn subscription_with_agent_home(
    host: &TestHost,
    agent_home: Option<PathBuf>,
) -> Arc<dyn Subscription> {
    subscription_factory(host, agent_home, true, false)
}

fn subscription_factory(
    host: &TestHost,
    agent_home: Option<PathBuf>,
    skip_keychain: bool,
    force_keychain: bool,
) -> Arc<dyn Subscription> {
    let mut raw = RawTable::new();
    raw.insert("command", RawValue::String(PROGRAM.to_owned()));
    let mut factory = factory_with_endpoints(endpoints());
    factory.test_agent_home = agent_home;
    factory.skip_keychain = skip_keychain;
    factory.force_keychain = force_keychain;
    let cfg = factory.validate(&raw).expect("config");
    factory.build(&cfg, host.context()).expect("build")
}

fn models_hash_name(id: &hofund_plugin_api::AccountId) -> String {
    let digest = hofund_plugin_sdk::digest::sha256(id.to_string().as_bytes());
    let mut name = String::new();
    for byte in digest {
        use std::fmt::Write as _;
        write!(name, "{byte:02x}").expect("hex");
    }
    name
}

fn enqueue_period(host: &TestHost) {
    host.http().enqueue_response(
        200,
        &[("content-type", "application/json")],
        br#"{"billingCycleEnd":"1792833042000","planUsage":{"autoPercentUsed":1,"apiPercentUsed":2,"totalPercentUsed":3}}"#,
    );
}

fn ctx() -> SignInCtx {
    SignInCtx::new(plugin())
        .with_session(SESSION)
        .expect("session")
}

fn b64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0_u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
        let count = chunk.len() + 1;
        for i in 0..count {
            out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]));
        }
    }
    out
}

fn jwt(email: &str, exp: i64) -> String {
    let claims = format!(r#"{{"email":"{email}","sub":"auth0|user","exp":{exp}}}"#);
    format!("e30.{}.sig", b64url(claims.as_bytes()))
}

fn write_auth(path: &Path, token: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    std::fs::write(path, format!(r#"{{"accessToken":"{token}"}}"#)).expect("auth");
}

struct ApiKeyTest;
impl Drop for ApiKeyTest {
    fn drop(&mut self) {
        crate::exchange::set_test_api_key(None);
    }
}

fn api_key_test() -> ApiKeyTest {
    crate::exchange::set_test_api_key(None);
    ApiKeyTest
}

fn enqueue_exchange(host: &TestHost, token: &str) {
    host.http().enqueue_response(
        200,
        &[("content-type", "application/json")],
        format!(r#"{{"accessToken":"{token}"}}"#).into_bytes(),
    );
}

fn exchange_url() -> String {
    endpoints().exchange_user_api_key()
}

macro_rules! assert_exchange_request {
    ($call:expr, $key:expr) => {{
        let call = $call;
        let key = $key;
        assert_eq!(call.method(), "POST");
        assert_eq!(call.uri(), &exchange_url());
        assert_eq!(call.body(), b"{}");
        assert!(
            call.header("connect-protocol-version").is_none(),
            "exchange omits Connect-Protocol-Version, got {:?}",
            call.headers()
        );
        assert_eq!(call.header("content-type"), Some("application/json"));
        assert_eq!(call.header("x-cursor-client-type"), Some("cli"));
        assert_eq!(call.header("x-ghost-mode"), Some("true"));
        assert!(
            call.sensitive_headers()
                .iter()
                .any(|name| name.eq_ignore_ascii_case("authorization")),
            "authorization must be marked sensitive"
        );
        let debug = format!("{call:?}");
        assert!(
            !debug.contains(key),
            "recorded request must not contain the API key"
        );
    }};
}

#[test]
fn a2a_28_import_auth_json_adds_account() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let token = jwt("user@example.test", 4_000_000_000);
    let text = format!(r#"{{"accessToken":"{token}"}}"#);
    let ids = block_on(sub.import_accounts(&text, host.context())).expect("import");
    assert_eq!(ids.len(), 1);
    let secret = block_on(host.context().accounts().secret(&ids[0]))
        .expect("read")
        .expect("stored");
    assert!(
        SecretObject::parse(&secret)
            .expect("parse")
            .secret(tokens::ACCESS)
            .is_some()
    );
}

#[test]
fn a2a_29_discover_lists_without_secrets() {
    let dir = tempfile::tempdir().expect("dir");
    let agent_home = dir.path().join("cursor-home");
    let token = jwt("discover@example.test", 4_000_000_000);
    write_auth(&agent_home.join("auth.json"), &token);
    let rows = crate::discover::list(&agent_home).expect("list");
    assert_eq!(rows.len(), 1);
    let debug = format!("{rows:?}");
    assert!(!debug.contains(&token));
}

#[test]
fn a2a_30_model_list_collapses_fixture() {
    let list = crate::models::model_list(MODELS_FIXTURE).expect("models");
    let ids: Vec<_> = list.models().iter().map(|m| m.id()).collect();
    assert!(ids.contains(&"grok-4.7"));
    assert!(ids.contains(&"composer-2.5"));
}

#[test]
fn a2a_31_refresh_runs_status_when_token_near_expiry() {
    let dir = tempfile::tempdir().expect("dir");
    let agent_home = dir.path().join("agent");
    let near = jwt("refresh@example.test", 100);
    write_auth(&agent_home.join("auth.json"), &near);
    let host = host(&dir);
    let sub = subscription_with_agent_home(&host, Some(agent_home));
    host.process().script_piped(PipedScript::new().line("ok"));
    let id = account_id_from(&["cursor", "refresh@example.test"]);
    let blob = tokens::blob_agent("refresh@example.test", None);
    let account = host
        .context()
        .accounts()
        .account(id.clone(), "refresh@example.test")
        .expect("account");
    block_on(host.context().accounts().upsert(account, blob.to_secret())).expect("upsert");
    block_on(sub.refresh(&id, host.context())).expect("refresh");
    assert!(
        !host.process().programs().is_empty(),
        "agent refresh runs cursor-agent status"
    );
}

#[test]
fn a2a_32_quota_parses_period_fixture() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let token = jwt("quota@example.test", 4_000_000_000);
    let id = account_id_from(&["cursor", "quota@example.test"]);
    let blob = tokens::blob_oauth(
        &hofund_plugin_api::SecretString::from(token.clone()),
        &hofund_plugin_api::SecretString::from("none"),
        4_000_000_000_000,
        "quota@example.test",
        None,
    );
    let account = host
        .context()
        .accounts()
        .account(id.clone(), "quota@example.test")
        .expect("account");
    block_on(host.context().accounts().upsert(account, blob.to_secret())).expect("upsert");
    let period_url = endpoints().dashboard("GetCurrentPeriodUsage");
    host.http().enqueue_response(
        200,
        &[("content-type", "application/json")],
        br#"{"billingCycleEnd":"1792833042000","planUsage":{"autoPercentUsed":12.5,"apiPercentUsed":40,"totalPercentUsed":20}}"#,
    );
    let windows = block_on(sub.quota(&id, host.context())).expect("quota");
    assert_eq!(windows.len(), 3);
    let calls = host.http().calls();
    assert!(calls.iter().any(|call| call.uri().contains(&period_url)));
}

#[test]
fn a2a_33_browser_sign_in_poll_mock() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let token = jwt("browser@example.test", 4_000_000_000);
    block_on(async {
        let challenge = sub
            .start_sign_in(&ctx(), host.context())
            .await
            .expect("start");
        assert!(matches!(
            challenge,
            hofund_plugin_api::SignInChallenge::OpenUrl { .. }
        ));
        host.http().enqueue_response(404, &[], b"");
        host.http().enqueue_response(
            200,
            &[("content-type", "application/json")],
            format!(r#"{{"accessToken":"{token}","refreshToken":"rt"}}"#).into_bytes(),
        );
        host.http().enqueue_response(
            200,
            &[("content-type", "application/json")],
            br#"{"email":"browser@example.test"}"#,
        );
        host.http().enqueue_response(
            200,
            &[("content-type", "application/json")],
            br#"{"planInfo":{"planName":"Pro"}}"#,
        );
        let id = sub
            .complete_sign_in(&ctx(), SignInProof::Await, host.context())
            .await
            .expect("complete");
        assert_eq!(id, account_id_from(&["cursor", "browser@example.test"]));
        let secret = host
            .context()
            .accounts()
            .secret(&id)
            .await
            .expect("read")
            .expect("stored");
        assert!(
            SecretObject::parse(&secret)
                .expect("parse")
                .secret(tokens::ACCESS)
                .is_some()
        );
    });
}

#[test]
fn a2a_34_errors_never_echo_access_token() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let token = jwt("secret@example.test", 4_000_000_000);
    let text = format!(r#"{{"accessToken":"{token}","note":"broken"#);
    let error = block_on(sub.import_accounts(&text, host.context())).expect_err("bad");
    assert!(!error.to_string().contains(&token));
}

#[test]
fn a2a_35_oauth_refresh_when_near_expiry() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let near = jwt("oauth@example.test", 100);
    let id = account_id_from(&["cursor", "oauth@example.test"]);
    let blob = tokens::blob_oauth(
        &hofund_plugin_api::SecretString::from(near.clone()),
        &hofund_plugin_api::SecretString::from("refresh-token"),
        100_000,
        "oauth@example.test",
        None,
    );
    let account = host
        .context()
        .accounts()
        .account(id.clone(), "oauth@example.test")
        .expect("account");
    block_on(host.context().accounts().upsert(account, blob.to_secret())).expect("upsert");
    let refresh_url = endpoints().oauth_refresh();
    host.http().enqueue_response(
        200,
        &[("content-type", "application/json")],
        br#"{"accessToken":"renewed-token","refreshToken":"refresh-token-2"}"#.to_vec(),
    );
    block_on(sub.refresh(&id, host.context())).expect("refresh");
    let calls = host.http().calls();
    assert!(calls.iter().any(|call| call.uri().contains(&refresh_url)));
    let secret = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    let parsed = SecretObject::parse(&secret).expect("blob");
    assert_eq!(
        parsed
            .secret(tokens::ACCESS)
            .map(|t| hofund_plugin_sdk::oauth::secret_text(&t).to_owned()),
        Some("renewed-token".to_owned())
    );
}

#[test]
fn a2a_36_effort_suffix_stripped_from_name() {
    let list = crate::models::model_list(MODELS_FIXTURE).expect("models");
    let grok = list
        .models()
        .iter()
        .find(|model| model.id() == "grok-4.7")
        .expect("grok family");
    assert_eq!(grok.display_name(), Some("Grok 4.7"));
}

#[test]
fn a2a_37_remove_words_does_not_panic_on_short_names() {
    let list = crate::models::model_list("auto - Auto (default)\n").expect("models");
    assert_eq!(list.models().len(), 1);
}

#[test]
fn a2a_38_reimport_keeps_stored_refresh_token() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let token = jwt("keep@example.test", 4_000_000_000);
    let id = account_id_from(&["cursor", "keep@example.test"]);
    let blob = tokens::blob_oauth(
        &hofund_plugin_api::SecretString::from(token.clone()),
        &hofund_plugin_api::SecretString::from("stored-refresh"),
        4_000_000_000_000,
        "keep@example.test",
        None,
    );
    let account = host
        .context()
        .accounts()
        .account(id.clone(), "keep@example.test")
        .expect("account");
    block_on(host.context().accounts().upsert(account, blob.to_secret())).expect("upsert");
    let import = format!(r#"{{"accessToken":"{token}"}}"#);
    block_on(sub.import_accounts(&import, host.context())).expect("import");
    let secret = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    let parsed = SecretObject::parse(&secret).expect("blob");
    assert_eq!(
        parsed
            .secret(tokens::REFRESH)
            .map(|t| hofund_plugin_sdk::oauth::secret_text(&t).to_owned()),
        Some("stored-refresh".to_owned())
    );
}

#[test]
fn a2a_39_agent_quota_reads_auth_json_without_spawning_agent() {
    let dir = tempfile::tempdir().expect("dir");
    let agent_home = dir.path().join("agent");
    let token = jwt("agent-quota@example.test", 4_000_000_000);
    write_auth(&agent_home.join("auth.json"), &token);
    let host = host(&dir);
    let sub = subscription_with_agent_home(&host, Some(agent_home));
    let id = account_id_from(&["cursor", "agent-quota@example.test"]);
    let blob = tokens::blob_agent("agent-quota@example.test", None);
    let account = host
        .context()
        .accounts()
        .account(id.clone(), "agent-quota@example.test")
        .expect("account");
    block_on(host.context().accounts().upsert(account, blob.to_secret())).expect("upsert");
    let period_url = endpoints().dashboard("GetCurrentPeriodUsage");
    host.http().enqueue_response(
        200,
        &[("content-type", "application/json")],
        br#"{"billingCycleEnd":"1792833042000","planUsage":{"autoPercentUsed":1,"apiPercentUsed":2,"totalPercentUsed":3}}"#,
    );
    let windows = block_on(sub.quota(&id, host.context())).expect("quota");
    assert_eq!(windows.len(), 3);
    assert!(
        host.process().programs().is_empty(),
        "agent quota must not spawn cursor-agent when auth.json is fresh"
    );
    let calls = host.http().calls();
    assert!(calls.iter().any(|call| call.uri().contains(&period_url)));
}

#[test]
fn a2a_40_browser_sign_in_cancel_during_poll() {
    use hofund_plugin_api::PluginErrorKind;

    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    block_on(async {
        sub.start_sign_in(&ctx(), host.context())
            .await
            .expect("start");
        for _ in 0..30 {
            host.http().enqueue_response(404, &[], b"");
        }
        let ctx = ctx();
        join_all(vec![
            Box::pin(async {
                let error = sub
                    .complete_sign_in(&ctx, SignInProof::Await, host.context())
                    .await
                    .expect_err("cancelled");
                assert_eq!(error.kind(), PluginErrorKind::Config);
                assert!(error.to_string().contains("cancelled"));
            }),
            Box::pin(async {
                loop {
                    if host
                        .http()
                        .calls()
                        .iter()
                        .any(|call| call.uri().contains("/auth/poll"))
                    {
                        sub.cancel_sign_in(&ctx, host.context())
                            .await
                            .expect("cancel");
                        return;
                    }
                    yield_times(1).await;
                }
            }),
        ])
        .await;
    });
}

#[test]
fn a2a_41_agent_quota_spawns_status_when_token_near_expiry() {
    let dir = tempfile::tempdir().expect("dir");
    let agent_home = dir.path().join("agent");
    let near = jwt("quota-near@example.test", 100);
    write_auth(&agent_home.join("auth.json"), &near);
    let host = host(&dir);
    let sub = subscription_with_agent_home(&host, Some(agent_home));
    host.process().script_piped(PipedScript::new().line("ok"));
    let id = account_id_from(&["cursor", "quota-near@example.test"]);
    let blob = tokens::blob_agent("quota-near@example.test", None);
    let account = host
        .context()
        .accounts()
        .account(id.clone(), "quota-near@example.test")
        .expect("account");
    block_on(host.context().accounts().upsert(account, blob.to_secret())).expect("upsert");
    let period_url = endpoints().dashboard("GetCurrentPeriodUsage");
    host.http().enqueue_response(
        200,
        &[("content-type", "application/json")],
        br#"{"billingCycleEnd":"1792833042000","planUsage":{"autoPercentUsed":1,"apiPercentUsed":2,"totalPercentUsed":3}}"#,
    );
    let windows = block_on(sub.quota(&id, host.context())).expect("quota");
    assert_eq!(windows.len(), 3);
    assert!(
        !host.process().programs().is_empty(),
        "agent quota must spawn cursor-agent status when auth.json is near expiry"
    );
    let calls = host.http().calls();
    assert!(calls.iter().any(|call| call.uri().contains(&period_url)));
}

#[test]
fn a2a_42_list_models_spawn_uses_isolated_models_dir() {
    let dir = tempfile::tempdir().expect("dir");
    let override_home = dir.path().join("daemon-cursor-home");
    auth::set_test_config_dir(Some(override_home.clone()));
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            auth::set_test_config_dir(None);
        }
    }
    let _restore = Restore;

    let host = host(&dir);
    let sub = subscription(&host);
    let token = jwt("models@example.test", 4_000_000_000);
    let id = account_id_from(&["cursor", "models@example.test"]);
    let blob = tokens::blob_oauth(
        &hofund_plugin_api::SecretString::from(token),
        &hofund_plugin_api::SecretString::from("none"),
        4_000_000_000_000,
        "models@example.test",
        None,
    );
    let account = host
        .context()
        .accounts()
        .account(id.clone(), "models@example.test")
        .expect("account");
    block_on(host.context().accounts().upsert(account, blob.to_secret())).expect("upsert");
    host.process().script_piped(
        PipedScript::new()
            .line("auto - Auto (default)")
            .exits(|_| vec![]),
    );
    let list = block_on(sub.list_models(&id, host.context())).expect("models");
    assert!(!list.models().is_empty());
    let spawn = host
        .process()
        .piped_spawns()
        .into_iter()
        .next()
        .expect("models spawn");
    let expected_dir = PathBuf::from(spawn.env_var("CURSOR_CONFIG_DIR").expect("config dir"));
    assert_eq!(
        expected_dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned()),
        Some(models_hash_name(&id))
    );
    assert_eq!(spawn.cwd, expected_dir);
    assert_ne!(
        spawn.env_var("CURSOR_CONFIG_DIR"),
        Some(override_home.as_os_str())
    );
    assert!(
        !expected_dir.exists(),
        "models temp dir must be removed after success"
    );
    let state = host.context().dirs().state_dir().expect("state");
    assert!(
        !state.join("cursor-models").exists(),
        "state dir must not keep cursor-models"
    );
}

#[test]
fn r8b_list_models_removes_temp_dir_when_the_cli_fails() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let token = jwt("models-fail@example.test", 4_000_000_000);
    let id = account_id_from(&["cursor", "models-fail@example.test"]);
    let blob = tokens::blob_oauth(
        &hofund_plugin_api::SecretString::from(token),
        &hofund_plugin_api::SecretString::from("none"),
        4_000_000_000_000,
        "models-fail@example.test",
        None,
    );
    let account = host
        .context()
        .accounts()
        .account(id.clone(), "models-fail@example.test")
        .expect("account");
    block_on(host.context().accounts().upsert(account, blob.to_secret())).expect("upsert");
    host.process()
        .script_piped(PipedScript::new().exits(|_| vec![]));
    let error = block_on(sub.list_models(&id, host.context())).expect_err("empty models");
    assert!(error.to_string().contains("model list"));
    let spawn = host
        .process()
        .piped_spawns()
        .into_iter()
        .next()
        .expect("models spawn");
    let expected_dir = PathBuf::from(spawn.env_var("CURSOR_CONFIG_DIR").expect("config dir"));
    assert_eq!(
        expected_dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned()),
        Some(models_hash_name(&id))
    );
    assert!(
        !expected_dir.exists(),
        "models temp dir must be removed after a failing CLI"
    );
    let state = host.context().dirs().state_dir().expect("state");
    assert!(
        !state.join("cursor-models").exists(),
        "state dir must not keep cursor-models after a failing CLI"
    );
}

#[test]
fn a2a_43_agent_quota_uses_auth_json_when_keychain_read_fails() {
    let dir = tempfile::tempdir().expect("dir");
    let agent_home = dir.path().join("agent");
    let token = jwt("keychain-fallback@example.test", 4_000_000_000);
    write_auth(&agent_home.join("auth.json"), &token);
    let host = host(&dir);
    let sub = subscription_factory(&host, Some(agent_home), false, true);
    let id = account_id_from(&["cursor", "keychain-fallback@example.test"]);
    let blob = tokens::blob_agent("keychain-fallback@example.test", None);
    let account = host
        .context()
        .accounts()
        .account(id.clone(), "keychain-fallback@example.test")
        .expect("account");
    block_on(host.context().accounts().upsert(account, blob.to_secret())).expect("upsert");
    host.process()
        .script_piped(PipedScript::new().exits(|_| vec![]).status(1));
    let period_url = endpoints().dashboard("GetCurrentPeriodUsage");
    enqueue_period(&host);
    let windows = block_on(sub.quota(&id, host.context())).expect("quota");
    assert_eq!(windows.len(), 3);
    let keychain = host
        .process()
        .piped_spawns()
        .into_iter()
        .next()
        .expect("keychain spawn");
    assert!(
        keychain.program.to_string_lossy().ends_with("security"),
        "keychain failure must spawn security on every OS, got {}",
        keychain.program.display()
    );
    assert!(
        host.process().programs().is_empty(),
        "quota must not spawn cursor-agent when auth.json is fresh"
    );
    let calls = host.http().calls();
    assert!(calls.iter().any(|call| call.uri().contains(&period_url)));
}

fn upsert_agent(
    host: &TestHost,
    email: &str,
    agent_home: &Path,
    exp: i64,
) -> hofund_plugin_api::AccountId {
    let token = jwt(email, exp);
    write_auth(&agent_home.join("auth.json"), &token);
    let id = account_id_from(&["cursor", email]);
    let blob = tokens::blob_agent(email, None);
    let account = host
        .context()
        .accounts()
        .account(id.clone(), email)
        .expect("account");
    block_on(host.context().accounts().upsert(account, blob.to_secret())).expect("upsert");
    id
}

#[test]
fn r8b_keychain_exit_44_falls_through_and_can_be_retried() {
    let dir = tempfile::tempdir().expect("dir");
    let agent_home = dir.path().join("agent");
    let host = host(&dir);
    let sub = subscription_factory(&host, Some(agent_home.clone()), false, true);
    let id = upsert_agent(&host, "absent@example.test", &agent_home, 4_000_000_000);
    host.process()
        .script_piped(PipedScript::new().exits(|_| vec![]).status(44));
    host.process()
        .script_piped(PipedScript::new().exits(|_| vec![]).status(44));
    enqueue_period(&host);
    enqueue_period(&host);
    block_on(sub.quota(&id, host.context())).expect("first quota");
    block_on(sub.quota(&id, host.context())).expect("second quota");
    assert_eq!(
        host.process().piped_spawns().len(),
        2,
        "exit 44 must not start a denial period"
    );
}

#[test]
fn r8b_keychain_denial_skips_the_next_spawn() {
    let dir = tempfile::tempdir().expect("dir");
    let agent_home = dir.path().join("agent");
    let host = host(&dir);
    let sub = subscription_factory(&host, Some(agent_home.clone()), false, true);
    let id = upsert_agent(&host, "denied@example.test", &agent_home, 4_000_000_000);
    host.process()
        .script_piped(PipedScript::new().exits(|_| vec![]).status(1));
    enqueue_period(&host);
    enqueue_period(&host);
    block_on(sub.quota(&id, host.context())).expect("first quota");
    block_on(sub.quota(&id, host.context())).expect("second quota");
    assert_eq!(
        host.process().piped_spawns().len(),
        1,
        "a non-44 keychain failure must not spawn again during the denial period"
    );
}

#[test]
fn r8b_keychain_timeout_does_not_set_denial() {
    let dir = tempfile::tempdir().expect("dir");
    let agent_home = dir.path().join("agent");
    let host = TestHost::open_for_with_tokio_monotonic(
        hofund_plugin_api::PluginKind::Subscription,
        plugin(),
        dir.path(),
    );
    let sub = subscription_factory(&host, Some(agent_home.clone()), false, true);
    let id = upsert_agent(&host, "timeout@example.test", &agent_home, 4_000_000_000);
    // Hanging child: timeout must drop it and fall through. The empty result
    // is cached for 10s, then a later call may spawn again.
    host.process().script_piped(PipedScript::new());
    host.process()
        .script_piped(PipedScript::new().exits(|_| vec![]).status(44));
    enqueue_period(&host);
    enqueue_period(&host);
    enqueue_period(&host);
    block_on_paused(async {
        let first = sub.quota(&id, host.context());
        let drive = async {
            yield_times(4).await;
            advance_time(std::time::Duration::from_secs(11)).await;
        };
        join_all(vec![
            Box::pin(async {
                first.await.expect("quota after timeout");
            }),
            Box::pin(drive),
        ])
        .await;
        // Within the 10s empty cache, a second call must not spawn.
        sub.quota(&id, host.context())
            .await
            .expect("cached empty keychain");
        assert_eq!(host.process().piped_spawns().len(), 1);
        advance_time(std::time::Duration::from_secs(10)).await;
        sub.quota(&id, host.context())
            .await
            .expect("retry after cache");
        assert_eq!(
            host.process().piped_spawns().len(),
            2,
            "a timeout must not start a 10-minute denial"
        );
    });
}

#[test]
fn r8b_keychain_fresh_bypass_reads_during_denial() {
    let dir = tempfile::tempdir().expect("dir");
    let agent_home = dir.path().join("agent");
    let host = host(&dir);
    let sub = subscription_factory(&host, Some(agent_home.clone()), false, true);
    let id = upsert_agent(&host, "fresh@example.test", &agent_home, 100);
    host.process()
        .script_piped(PipedScript::new().exits(|_| vec![]).status(1));
    // status reread uses fresh=true and must spawn keychain again
    host.process()
        .script_piped(PipedScript::new().line("ok").exits(|_| vec![]).status(0));
    host.process().script_piped(PipedScript::new().line("ok"));
    enqueue_period(&host);
    // near-expiry token forces agent status + fresh keychain reread
    let error = block_on(sub.quota(&id, host.context()));
    // After status, auth.json is still expired (exp=100) if the fake status
    // did not write a new token, so quota may fail. The assertion is the
    // spawn count: denial spawn + fresh reread.
    let _ = error;
    assert!(
        host.process().piped_spawns().len() >= 2,
        "fresh must bypass the denial period, spawns={}",
        host.process().piped_spawns().len()
    );
}

#[test]
fn r8b_overlapping_keychain_reads_share_one_spawn() {
    let dir = tempfile::tempdir().expect("dir");
    let agent_home = dir.path().join("agent");
    let host = host(&dir);
    let sub = subscription_factory(&host, Some(agent_home.clone()), false, true);
    let id = upsert_agent(
        &host,
        "single-flight@example.test",
        &agent_home,
        4_000_000_000,
    );
    // Hang until both calls have started, then we cannot easily release a
    // Hang script. Use a normal exit: join_all still overlaps the first
    // await enough that the second should attach to the in-flight spawn
    // if the first has not finished publishing. A hanging script plus
    // timeout would also work; here a delayed exits callback is enough
    // if both are polled before the first spawn returns. Queue one script
    // only — a second spawn would fail.
    host.process()
        .script_piped(PipedScript::new().exits(|_| vec![]).status(1));
    enqueue_period(&host);
    enqueue_period(&host);
    block_on(async {
        join_all(vec![
            Box::pin(async {
                sub.quota(&id, host.context()).await.expect("first");
            }),
            Box::pin(async {
                sub.quota(&id, host.context()).await.expect("second");
            }),
        ])
        .await;
    });
    assert_eq!(
        host.process().piped_spawns().len(),
        1,
        "overlapping keychain reads must share one spawn"
    );
}

#[test]
fn r8b_overlapping_oauth_refresh_posts_once() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let near = jwt("lease@example.test", 100);
    let id = account_id_from(&["cursor", "lease@example.test"]);
    let blob = tokens::blob_oauth(
        &hofund_plugin_api::SecretString::from(near),
        &hofund_plugin_api::SecretString::from("refresh-token"),
        100_000,
        "lease@example.test",
        None,
    );
    let account = host
        .context()
        .accounts()
        .account(id.clone(), "lease@example.test")
        .expect("account");
    block_on(host.context().accounts().upsert(account, blob.to_secret())).expect("upsert");
    let refresh_url = endpoints().oauth_refresh();
    host.http().enqueue_response(
        200,
        &[("content-type", "application/json")],
        br#"{"accessToken":"renewed-token","refreshToken":"refresh-token-2"}"#.to_vec(),
    );
    block_on(async {
        join_all(vec![
            Box::pin(async {
                sub.refresh(&id, host.context()).await.expect("first");
            }),
            Box::pin(async {
                sub.refresh(&id, host.context()).await.expect("second");
            }),
        ])
        .await;
    });
    let posts = host
        .http()
        .calls()
        .into_iter()
        .filter(|call| call.uri().contains(&refresh_url))
        .count();
    assert_eq!(
        posts, 1,
        "overlapping near-expiry refreshes must renew once"
    );
}

#[test]
fn r8c_import_api_key_stores_key_not_access_token() {
    let _guard = api_key_test();
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let key = "cursor-test-api-key-alpha";
    let token = jwt("api@example.test", 4_000_000_000);
    enqueue_exchange(&host, &token);
    let ids = block_on(sub.import_accounts(key, host.context())).expect("import");
    assert_eq!(ids.len(), 1);
    assert_eq!(ids[0], account_id_from(&["cursor", "api@example.test"]));
    let secret = block_on(host.context().accounts().secret(&ids[0]))
        .expect("read")
        .expect("stored");
    let blob = SecretObject::parse(&secret).expect("parse");
    assert!(tokens::is_api_key(&blob));
    assert_eq!(
        blob.secret(tokens::API_KEY)
            .as_ref()
            .map(hofund_plugin_sdk::oauth::secret_text),
        Some(key)
    );
    assert!(
        blob.secret(tokens::ACCESS).is_none(),
        "exchanged accessToken must not be written into the account secret"
    );
    let calls = host.http().calls();
    assert_eq!(calls.len(), 1);
    assert_exchange_request!(&calls[0], key);
    let debug = format!("{blob:?}");
    assert!(!debug.contains(key), "blob Debug must redact the API key");
}

#[test]
fn r8c_import_reads_cursor_api_key_env_once() {
    let _guard = api_key_test();
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let key = "cursor-test-api-key-env";
    let token = jwt("env@example.test", 4_000_000_000);
    crate::exchange::set_test_api_key(Some(key));
    enqueue_exchange(&host, &token);
    let ids = block_on(sub.import_accounts("  ", host.context())).expect("import env");
    assert_eq!(ids[0], account_id_from(&["cursor", "env@example.test"]));
    crate::exchange::set_test_api_key(Some("cursor-test-api-key-other"));
    enqueue_period(&host);
    block_on(sub.quota(&ids[0], host.context())).expect("quota uses stored key");
    let exchanges: Vec<_> = host
        .http()
        .calls()
        .into_iter()
        .filter(|call| call.uri() == exchange_url())
        .collect();
    assert_eq!(
        exchanges.len(),
        1,
        "execute/quota must not read CURSOR_API_KEY or re-exchange a fresh cache"
    );
    assert_exchange_request!(&exchanges[0], key);
}

#[test]
fn r8c_sign_in_token_stores_key_not_access_token() {
    let _guard = api_key_test();
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let key = "cursor-test-api-key-sign-in";
    let token = jwt("token@example.test", 4_000_000_000);
    enqueue_exchange(&host, &token);
    let id = block_on(sub.complete_sign_in(
        &ctx(),
        SignInProof::Token(hofund_plugin_api::SecretString::from(key.to_owned())),
        host.context(),
    ))
    .expect("complete");
    assert_eq!(id, account_id_from(&["cursor", "token@example.test"]));
    let secret = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    let blob = SecretObject::parse(&secret).expect("parse");
    assert!(blob.secret(tokens::ACCESS).is_none());
    assert!(tokens::is_api_key(&blob));
}

#[test]
fn r8c_sign_in_reads_env_key_and_hides_it_from_challenge() {
    let _guard = api_key_test();
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let key = "cursor-test-api-key-challenge";
    let token = jwt("challenge@example.test", 4_000_000_000);
    crate::exchange::set_test_api_key(Some(key));
    let challenge = block_on(sub.start_sign_in(&ctx(), host.context())).expect("start");
    match challenge {
        hofund_plugin_api::SignInChallenge::Instructions { text } => {
            assert!(text.contains("CURSOR_API_KEY"));
            assert!(!text.contains(key));
        }
        other => panic!("expected instructions, got {other:?}"),
    }
    enqueue_exchange(&host, &token);
    let id = block_on(sub.complete_sign_in(&ctx(), SignInProof::Await, host.context()))
        .expect("complete");
    assert_eq!(id, account_id_from(&["cursor", "challenge@example.test"]));
}

#[test]
fn r8c_cached_exchange_is_reused_until_near_expiry() {
    let _guard = api_key_test();
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let key = "cursor-test-api-key-cache";
    let fresh = jwt("cache@example.test", 4_000_000_000);
    enqueue_exchange(&host, &fresh);
    let ids = block_on(sub.import_accounts(key, host.context())).expect("import");
    enqueue_period(&host);
    enqueue_period(&host);
    block_on(sub.quota(&ids[0], host.context())).expect("first");
    block_on(sub.quota(&ids[0], host.context())).expect("second");
    let exchanges = host
        .http()
        .calls()
        .into_iter()
        .filter(|call| call.uri() == exchange_url())
        .count();
    assert_eq!(exchanges, 1, "a fresh exchanged token must be reused");
}

#[test]
fn r8c_near_expiry_cache_exchanges_again() {
    let _guard = api_key_test();
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let key = "cursor-test-api-key-soon";
    // TestHost clock is UNIX_EPOCH; exp=100s is inside the 5-minute margin.
    let soon = jwt("soon@example.test", 100);
    let later = jwt("soon@example.test", 4_000_000_000);
    enqueue_exchange(&host, &soon);
    let ids = block_on(sub.import_accounts(key, host.context())).expect("import");
    enqueue_exchange(&host, &later);
    enqueue_period(&host);
    block_on(sub.quota(&ids[0], host.context())).expect("quota");
    let exchanges = host
        .http()
        .calls()
        .into_iter()
        .filter(|call| call.uri() == exchange_url())
        .count();
    assert_eq!(
        exchanges, 2,
        "a token within the 5-minute margin must be exchanged again"
    );
}

#[test]
fn r8c_exchange_http_500_and_connect_are_status_502() {
    let _guard = api_key_test();
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let key = "cursor-test-api-key-502";
    host.http()
        .enqueue_response(500, &[("content-type", "application/json")], b"{}");
    let error = block_on(sub.import_accounts(key, host.context())).expect_err("500");
    assert_eq!(error.kind(), PluginErrorKind::Status);
    assert_eq!(error.status(), Some(502));
    host.http().enqueue_connect("offline");
    let error = block_on(sub.import_accounts(key, host.context())).expect_err("connect");
    assert_eq!(error.kind(), PluginErrorKind::Status);
    assert_eq!(error.status(), Some(502));
    assert!(error.message().contains("couldn't reach Cursor's API"));
}

#[test]
fn r8c_exchange_without_access_token_is_status_401() {
    let _guard = api_key_test();
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let key = "cursor-test-api-key-401";
    host.http().enqueue_response(
        200,
        &[("content-type", "application/json")],
        br#"{"ok":true}"#,
    );
    let error = block_on(sub.import_accounts(key, host.context())).expect_err("no token");
    assert_eq!(error.kind(), PluginErrorKind::Status);
    assert_eq!(error.status(), Some(401));
    host.http()
        .enqueue_response(400, &[("content-type", "application/json")], b"{}");
    let error = block_on(sub.import_accounts(key, host.context())).expect_err("400");
    assert_eq!(error.kind(), PluginErrorKind::Status);
    assert_eq!(error.status(), Some(401));
}

#[test]
fn r8c_refresh_updates_cache_without_writing_access_token() {
    let _guard = api_key_test();
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let key = "cursor-test-api-key-refresh";
    // TestHost clock is UNIX_EPOCH; exp=100s is inside the 5-minute margin.
    let soon = jwt("refresh-api@example.test", 100);
    let later = jwt("refresh-api@example.test", 4_000_000_000);
    enqueue_exchange(&host, &soon);
    let ids = block_on(sub.import_accounts(key, host.context())).expect("import");
    enqueue_exchange(&host, &later);
    block_on(sub.refresh(&ids[0], host.context())).expect("refresh");
    let secret = block_on(host.context().accounts().secret(&ids[0]))
        .expect("read")
        .expect("stored");
    let blob = SecretObject::parse(&secret).expect("parse");
    assert!(blob.secret(tokens::ACCESS).is_none());
    assert_eq!(
        blob.secret(tokens::API_KEY)
            .as_ref()
            .map(hofund_plugin_sdk::oauth::secret_text),
        Some(key)
    );
}

#[test]
fn r8c_account_status_ready_for_api_key() {
    let _guard = api_key_test();
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let key = "cursor-test-api-key-status";
    let token = jwt("status@example.test", 4_000_000_000);
    enqueue_exchange(&host, &token);
    let ids = block_on(sub.import_accounts(key, host.context())).expect("import");
    let status = block_on(sub.account_status(&ids[0], host.context())).expect("status");
    assert_eq!(status, hofund_plugin_api::AccountStatus::Ready);
}

fn jwt_with_sub(email: &str, sub: &str, exp: i64) -> String {
    let claims = format!(r#"{{"email":"{email}","sub":"{sub}","exp":{exp}}}"#);
    format!("e30.{}.sig", b64url(claims.as_bytes()))
}

fn write_cli_config(dir: &Path, body: &str) {
    std::fs::create_dir_all(dir).expect("team dir");
    std::fs::write(dir.join("cli-config.json"), body).expect("cli-config");
}

fn upsert_oauth(host: &TestHost, email: &str, token: &str) -> hofund_plugin_api::AccountId {
    let id = account_id_from(&["cursor", email]);
    let blob = tokens::blob_oauth(
        &hofund_plugin_api::SecretString::from(token.to_owned()),
        &hofund_plugin_api::SecretString::from("none"),
        4_000_000_000_000,
        email,
        None,
    );
    let account = host
        .context()
        .accounts()
        .account(id.clone(), email)
        .expect("account");
    block_on(host.context().accounts().upsert(account, blob.to_secret())).expect("upsert");
    id
}

fn run_quota_with_cli_config(
    config_dir: &Path,
    cli_body: &str,
    email: &str,
    sub: &str,
) -> (Vec<u8>, Option<String>) {
    write_cli_config(config_dir, cli_body);
    let _guard = crate::team::begin_test(Some(config_dir.to_path_buf()));
    crate::team::set_test_now_ms(Some(1_000));
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub_plugin = subscription(&host);
    let token = jwt_with_sub(email, sub, 4_000_000_000);
    let id = upsert_oauth(&host, email, &token);
    host.http().enqueue_response(
        200,
        &[("content-type", "application/json")],
        br#"{"billingCycleEnd":"1792833042000","planUsage":{"autoPercentUsed":1,"apiPercentUsed":2,"totalPercentUsed":3}}"#,
    );
    block_on(sub_plugin.quota(&id, host.context())).expect("quota");
    let period_url = endpoints().dashboard("GetCurrentPeriodUsage");
    let call = host
        .http()
        .calls()
        .into_iter()
        .find(|call| call.uri().contains(&period_url))
        .expect("period usage call");
    (
        call.body().to_vec(),
        call.header("x-cursor-team-id").map(str::to_owned),
    )
}

#[test]
fn r8d_quota_sends_decimal_team_header_when_auth_id_matches_sub() {
    let config = tempfile::tempdir().expect("config");
    let (body, team) = run_quota_with_cli_config(
        config.path(),
        r#"{"authInfo":{"authId":"auth0|user","activeTeamId":42}}"#,
        "team@example.test",
        "auth0|user",
    );
    assert_eq!(body, b"{}");
    assert_eq!(team.as_deref(), Some("42"));
}

#[test]
fn r8d_quota_omits_team_header_when_auth_id_does_not_match_jwt_sub() {
    let config = tempfile::tempdir().expect("config");
    let (body, team) = run_quota_with_cli_config(
        config.path(),
        r#"{"authInfo":{"authId":"auth0|other","activeTeamId":42}}"#,
        "team-mismatch@example.test",
        "auth0|user",
    );
    assert_eq!(body, b"{}");
    assert_eq!(team, None);
}

#[test]
fn r8d_quota_omits_team_header_when_active_team_id_is_zero() {
    let config = tempfile::tempdir().expect("config");
    let (_, team) = run_quota_with_cli_config(
        config.path(),
        r#"{"authInfo":{"authId":"auth0|user","activeTeamId":0}}"#,
        "team-zero@example.test",
        "auth0|user",
    );
    assert_eq!(team, None);
}

#[test]
fn r8d_quota_period_body_stays_empty_object_when_team_header_is_sent() {
    let config = tempfile::tempdir().expect("config");
    let (body, team) = run_quota_with_cli_config(
        config.path(),
        r#"{"authInfo":{"authId":"auth0|user","activeTeamId":7}}"#,
        "team-body@example.test",
        "auth0|user",
    );
    assert_eq!(
        body, b"{}",
        "GetCurrentPeriodUsage body stays {{}} when the team id is sent"
    );
    assert_eq!(team.as_deref(), Some("7"));
}

#[test]
fn r8f3_upstream_execute_honors_keychain_enablement() {
    {
        let dir = tempfile::tempdir().expect("dir");
        let agent_home = dir.path().join("agent");
        let host = host(&dir);
        let sub = subscription_factory(&host, Some(agent_home.clone()), false, true);
        let id = upsert_agent(
            &host,
            "upstream-kc@example.test",
            &agent_home,
            4_000_000_000,
        );
        host.process()
            .script_piped(PipedScript::new().exits(|_| vec![]).status(44));
        let provider = sub.upstream(&id, host.context()).expect("provider");
        let _ = block_on(provider.execute(
            hofund_plugin_api::Request::new("auto"),
            hofund_plugin_api::ProtocolId::OPENAI_CHAT,
        ));
        assert_eq!(
            host.process().piped_spawns().len(),
            1,
            "enabled keychain must be consulted on upstream execute"
        );
    }

    {
        let dir = tempfile::tempdir().expect("dir");
        let agent_home = dir.path().join("agent");
        let host = host(&dir);
        let sub = subscription_factory(&host, Some(agent_home.clone()), true, false);
        let id = upsert_agent(
            &host,
            "upstream-skip@example.test",
            &agent_home,
            4_000_000_000,
        );
        host.process()
            .script_piped(PipedScript::new().exits(|_| vec![]).status(44));
        let provider = sub.upstream(&id, host.context()).expect("provider");
        let _ = block_on(provider.execute(
            hofund_plugin_api::Request::new("auto"),
            hofund_plugin_api::ProtocolId::OPENAI_CHAT,
        ));
        assert!(
            host.process().piped_spawns().is_empty(),
            "skip_keychain must not spawn a keychain read on upstream execute"
        );
    }
}
