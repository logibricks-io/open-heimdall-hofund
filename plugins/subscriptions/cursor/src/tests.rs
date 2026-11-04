//! Unit tests with recording backends; no network.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use hofund_kit_subscription::{PipedScript, TestHost, block_on};
use hofund_plugin_api::{
    PluginId, PluginKind, RawTable, RawValue, SignInCtx, SignInProof, Subscription,
    SubscriptionFactory,
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
    let mut raw = RawTable::new();
    raw.insert("command", RawValue::String(PROGRAM.to_owned()));
    let mut factory = factory_with_endpoints(endpoints());
    factory.test_agent_home = agent_home;
    let cfg = factory.validate(&raw).expect("config");
    factory.build(&cfg, host.context()).expect("build")
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
    });
    let ctx = ctx();
    let host_ctx = host.context().clone();
    let sub_thread = Arc::clone(&sub);
    let finish = std::thread::spawn(move || {
        block_on(sub_thread.complete_sign_in(&ctx, SignInProof::Await, &host_ctx))
    });
    while !host
        .http()
        .calls()
        .iter()
        .any(|call| call.uri().contains("/auth/poll"))
    {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    block_on(sub.cancel_sign_in(&ctx(), host.context())).expect("cancel");
    let result = finish.join().expect("thread");
    let error = result.expect_err("cancelled");
    assert_eq!(error.kind(), PluginErrorKind::Config);
    assert!(error.to_string().contains("cancelled"));
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
    host.process()
        .script_piped(PipedScript::new().line(MODELS_FIXTURE.trim()));
    let list = block_on(sub.list_models(&id, host.context())).expect("models");
    assert!(!list.models().is_empty());
    let label = id
        .to_string()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect::<String>();
    let models_dir = host
        .context()
        .dirs()
        .state_dir()
        .expect("state")
        .join("cursor-models")
        .join(label);
    let spawn = host
        .process()
        .piped_spawns()
        .into_iter()
        .next()
        .expect("models spawn");
    assert_eq!(
        spawn.env_var("CURSOR_CONFIG_DIR"),
        Some(models_dir.as_os_str())
    );
    assert_eq!(spawn.cwd, models_dir);
    assert_ne!(
        spawn.env_var("CURSOR_CONFIG_DIR"),
        Some(override_home.as_os_str())
    );
}
