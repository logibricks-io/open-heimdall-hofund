//! D-221: new Antigravity accounts are refused; a saved account still serves.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use hofund_kit_subscription::{TestHost, block_on};
use hofund_plugin_api::{
    AccountEnrollment, AccountId, AccountStatus, PluginErrorKind, PluginId, PluginKind, RawTable,
    RawValue, SecretString, SignInChallenge, SignInCtx, SignInProof, Subscription,
    SubscriptionFactory,
};
use hofund_plugin_sdk::oauth::SecretObject;

use crate::project;
use crate::{ENROLLMENT_REFUSAL, Endpoints, RISK_LINE, client, factory_with_endpoints, version};

const SESSION: &str = "0123456789abcdef0123456789abcdef";
const TOKEN_URL: &str = "https://oauth.test/token";
const TEST_CLIENT_ID: &str = "hofund-test-antigravity-client";
const TEST_CLIENT_SECRET: &str = "hofund-test-antigravity-secret";

fn endpoints() -> Endpoints {
    Endpoints {
        authorize: "https://accounts.test/o/oauth2/v2/auth".to_owned(),
        token: TOKEN_URL.to_owned(),
        userinfo: "https://oauth.test/userinfo".to_owned(),
        load_code_assist: "https://code-assist-prod.test".to_owned(),
        code_assist: "https://code-assist-daily.test".to_owned(),
        version_manifest: "https://antigravity-version.test/manifest/latest-arm64-mac.yml"
            .to_owned(),
        onboard_poll: Duration::from_millis(1),
    }
}

fn host(dir: &tempfile::TempDir) -> TestHost {
    version::warm_cache_for_test(version::FALLBACK_VERSION, SystemTime::UNIX_EPOCH);
    TestHost::open_for(
        PluginKind::Subscription,
        PluginId::from_static("antigravity"),
        dir.path(),
    )
}

fn build(host: &TestHost, allow_new: bool) -> Arc<dyn Subscription> {
    let mut factory = factory_with_endpoints(endpoints());
    if allow_new {
        factory = factory.with_new_accounts_for_test();
    }
    let mut raw = RawTable::new();
    raw.insert(client::ID_KEY, RawValue::String(TEST_CLIENT_ID.to_owned()));
    raw.insert(
        client::SECRET_KEY,
        RawValue::String(TEST_CLIENT_SECRET.to_owned()),
    );
    let config = factory.validate(&raw).expect("config");
    factory.build(&config, host.context()).expect("build")
}

fn ctx() -> SignInCtx {
    SignInCtx::new(PluginId::from_static("antigravity"))
        .with_session(SESSION)
        .expect("session")
}

fn token_reply() -> Vec<u8> {
    br#"{"access_token":"acc-1","token_type":"Bearer","expires_in":3600}"#.to_vec()
}

fn seed(host: &TestHost) -> AccountId {
    let id = AccountId::try_from("a-0000000000000001").expect("account id");
    let blob = SecretObject::new()
        .with_number("v", 1)
        .with_secret("refresh_token", &SecretString::from("refresh-1".to_owned()))
        .with_text("email", "u@example.test")
        .with_text(project::PROJECT, "proj-1")
        .with_text(project::TIER, "free-tier")
        .with_text(project::PAID, "");
    block_on(async {
        let accounts = host.context().accounts();
        let account = accounts
            .account(id.clone(), "u@example.test")
            .expect("account");
        accounts
            .upsert(account, blob.to_secret())
            .await
            .expect("upsert");
    });
    id
}

#[test]
fn d221_production_enrollment_is_withdrawn_and_config_cannot_opt_in() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = build(&host, false);
    assert_eq!(
        sub.account_enrollment(),
        AccountEnrollment::Withdrawn {
            risk: RISK_LINE,
            refusal: ENROLLMENT_REFUSAL,
        }
    );
    assert!(RISK_LINE.contains("agy CLI"));
    assert!(RISK_LINE.contains("Gemini CLI is unchanged"));
    assert!(ENROLLMENT_REFUSAL.contains("agy CLI"));
    assert!(ENROLLMENT_REFUSAL.contains("Gemini CLI is unchanged"));
    let factory = factory_with_endpoints(endpoints());
    let mut raw = RawTable::new();
    raw.insert("allow_new_accounts", RawValue::Boolean(true));
    assert!(factory.validate(&raw).is_err());
}

#[test]
fn d226_setup_token_paste_is_unsupported_even_when_new_accounts_are_allowed() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = build(&host, true);
    let token = "sk-ant-oat01-abcdefghijklmnopqrstuvwxyz0123456789";
    for text in [
        token,
        &format!("CLAUDE_CODE_OAUTH_TOKEN={token}"),
        &format!("[\"{token}\"]"),
    ] {
        let error = block_on(sub.import_accounts(text, host.context())).expect_err("setup");
        assert_eq!(error.kind(), PluginErrorKind::Unsupported);
        assert!(!error.to_string().contains(token), "{error}");
    }
    assert!(host.http().calls().is_empty());
    let listed = block_on(host.context().accounts().list()).expect("list");
    assert!(listed.is_empty());
    let long = format!("sk-ant-oat{}", "a".repeat(600));
    let non_ascii = "sk-ant-oat01-café";
    for text in [long.as_str(), non_ascii] {
        let error = block_on(sub.import_accounts(text, host.context())).expect_err("shape");
        assert_eq!(error.kind(), PluginErrorKind::Unsupported);
        assert!(!error.to_string().contains("sk-ant-oat"), "{error}");
    }
    assert!(host.http().calls().is_empty());
}

#[test]
fn d221_sign_in_import_and_add_are_refused_before_any_request() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = build(&host, false);
    let started = block_on(sub.start_sign_in(&ctx(), host.context())).expect_err("start");
    assert_eq!(started.kind(), PluginErrorKind::Config);
    assert_eq!(started.message(), ENROLLMENT_REFUSAL);
    let completed = block_on(sub.complete_sign_in(
        &ctx(),
        SignInProof::callback("code", "state"),
        host.context(),
    ))
    .expect_err("complete");
    assert_eq!(completed.message(), ENROLLMENT_REFUSAL);
    let imported = block_on(sub.import_accounts(
        r#"{"type":"antigravity","refresh_token":"refresh-synthetic","email":"u@example.test"}"#,
        host.context(),
    ))
    .expect_err("import");
    assert_eq!(imported.message(), ENROLLMENT_REFUSAL);
    let added = block_on(sub.import_agent_sign_in(dir.path(), "agy-reference", host.context()))
        .expect_err("add");
    assert_eq!(added.message(), ENROLLMENT_REFUSAL);
    assert!(host.http().calls().is_empty());
    let accounts = block_on(host.context().accounts().list()).expect("list");
    assert!(accounts.is_empty());
}

#[test]
fn d221_a_gemini_export_keeps_the_type_error() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = build(&host, false);
    let error = block_on(sub.import_accounts(
        r#"{"type":"gemini","refresh_token":"refresh-synthetic","email":"g@example.test"}"#,
        host.context(),
    ))
    .expect_err("gemini export");
    assert_eq!(error.kind(), PluginErrorKind::Config);
    assert_ne!(error.message(), ENROLLMENT_REFUSAL);
    assert!(
        error
            .message()
            .contains("a gemini sign-in, not Antigravity's"),
        "{}",
        error.message()
    );
    assert!(host.http().calls().is_empty());
}

#[test]
fn d221_a_saved_account_still_refreshes_lists_models_and_serves() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host);
    let sub = build(&host, false);
    assert_eq!(
        block_on(sub.account_status(&id, host.context())).expect("status"),
        AccountStatus::Ready
    );
    host.http().enqueue(token_reply());
    block_on(sub.refresh(&id, host.context())).expect("refresh");
    host.http().enqueue(
        br#"{"models":{"gemini-3.8-flash-high":{"displayName":"Gemini 3.8 Flash (High)","maxTokens":1048576,"quotaInfo":{"remainingFraction":1}}}}"#
            .to_vec(),
    );
    let list = block_on(sub.list_models(&id, host.context())).expect("models");
    assert_eq!(list.models()[0].id(), "gemini-3.8-flash-high");
    sub.upstream(&id, host.context()).expect("upstream");
    let again = block_on(sub.start_sign_in(&ctx(), host.context())).expect_err("still withdrawn");
    assert_eq!(again.message(), ENROLLMENT_REFUSAL);
}

#[test]
fn d221_a_test_opt_in_can_still_start_sign_in() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = build(&host, true);
    assert_eq!(sub.account_enrollment(), AccountEnrollment::Open);
    let challenge = block_on(sub.start_sign_in(&ctx(), host.context())).expect("start");
    assert!(matches!(challenge, SignInChallenge::OpenUrl { .. }));
    block_on(sub.cancel_sign_in(&ctx(), host.context())).expect("cancel");
}
