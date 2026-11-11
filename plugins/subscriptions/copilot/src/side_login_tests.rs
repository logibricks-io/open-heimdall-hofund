//! Offline editor and CLI side-login fixtures.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use hofund_kit_subscription::{PipedScript, TestHost, block_on};
use hofund_plugin_api::{
    AccountId, AccountStatus, PluginErrorKind, PluginId, PluginKind, RawTable, SignInCtx,
    SignInProof, Subscription, SubscriptionFactory,
};
use hofund_plugin_sdk::json::json;
use hofund_plugin_sdk::oauth::{SecretObject, account_id_from};

use crate::CopilotFactory;
use crate::discover;
use crate::session::{LAPSED, LAPSED_REASON};

const API: &str = "https://api.individual.githubcopilot.com";

struct Fixture {
    _dir: tempfile::TempDir,
    host: TestHost,
    home: PathBuf,
    config: PathBuf,
}

fn fixture(label: &str) -> Fixture {
    let dir = tempfile::Builder::new()
        .prefix(&format!("hofund-r48-copilot-{label}-"))
        .tempdir()
        .expect("fixture");
    let root = dir.path().join("root");
    let home = root.join(".copilot");
    let config = root.join(".config");
    let host = TestHost::open_for(
        PluginKind::Subscription,
        PluginId::from_static("copilot"),
        dir.path(),
    );
    Fixture {
        _dir: dir,
        host,
        home,
        config,
    }
}

fn subscription(host: &TestHost) -> Arc<dyn Subscription> {
    let cfg = CopilotFactory.validate(&RawTable::new()).expect("config");
    CopilotFactory.build(&cfg, host.context()).expect("build")
}

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, body).expect("write");
}

fn editor(fixture: &Fixture, body: &str) -> PathBuf {
    let path = fixture.config.join("github-copilot/apps.json");
    write(&path, body);
    path
}

fn import(fixture: &Fixture, label: &str) -> AccountId {
    let rows = discover::list_at(&fixture.home, &fixture.config).expect("discover");
    let row = rows
        .iter()
        .find(|row| row.label() == label)
        .unwrap_or_else(|| panic!("{label} in {rows:?}"));
    block_on(discover::import_at(
        &fixture.home,
        &fixture.config,
        row.reference(),
        fixture.host.context(),
    ))
    .expect("import")
}

fn stored(host: &TestHost, id: &AccountId) -> SecretObject {
    let secret = block_on(host.context().accounts().secret(id))
        .expect("read")
        .expect("stored");
    SecretObject::parse(&secret).expect("blob")
}

fn session_reply() -> Vec<u8> {
    json!({"token":"tid=editor","expires_at":3600,"endpoints":{"api":API}})
        .to_string()
        .into_bytes()
}

#[test]
fn editor_and_ghe_discovery_import_is_read_only() {
    let fixture = fixture("editor");
    let path = editor(
        &fixture,
        r#"{
          "github.com:Iv1.one":{"user":"octo","oauth_token":"gho-editor"},
          "acme.ghe.com:Iv1.one":{"user":"octo","oauth_token":"gho-acme"}
        }"#,
    );
    let before = std::fs::read(&path).expect("before");
    let rows = discover::list_at(&fixture.home, &fixture.config).expect("list");
    let labels: Vec<_> = rows.iter().map(|row| row.label()).collect();
    assert_eq!(labels, ["octo", "octo@acme.ghe.com"]);
    assert_ne!(rows[0].id(), rows[1].id());
    let github = import(&fixture, "octo");
    let enterprise = import(&fixture, "octo@acme.ghe.com");
    assert_eq!(github, account_id_from(&["github.com", "octo"]));
    assert_eq!(enterprise, account_id_from(&["acme.ghe.com", "octo"]));
    for (id, host) in [(&github, "github.com"), (&enterprise, "acme.ghe.com")] {
        let blob = stored(&fixture.host, id);
        assert_eq!(blob.text("source"), Some("editor"));
        assert_eq!(blob.text("host"), Some(host));
        assert!(blob.secret("oauth_token").is_none());
    }
    let sub = subscription(&fixture.host);
    fixture.host.http().enqueue(
        json!({"copilot_plan":"free","quota_snapshots":{"chat":{"entitlement":10,"quota_remaining":5}}})
            .to_string()
            .into_bytes(),
    );
    fixture.host.http().enqueue(
        json!({"copilot_plan":"individual_max","quota_snapshots":{"chat":{"entitlement":20,"quota_remaining":5}}})
            .to_string()
            .into_bytes(),
    );
    let github_windows =
        block_on(sub.quota(&github, fixture.host.context())).expect("github quota");
    let enterprise_windows =
        block_on(sub.quota(&enterprise, fixture.host.context())).expect("GHE quota");
    assert_eq!(github_windows[0].name(), "Free · Chat requests");
    assert_eq!(enterprise_windows[0].name(), "Max · Chat requests");
    assert_eq!(
        fixture
            .host
            .http()
            .calls()
            .iter()
            .map(|call| call.uri())
            .collect::<Vec<_>>(),
        [
            "https://api.github.com/copilot_internal/user",
            "https://api.acme.ghe.com/copilot_internal/user"
        ]
    );
    let error = block_on(discover::import_at(
        &fixture.home,
        &fixture.config,
        "editor:missing",
        fixture.host.context(),
    ))
    .expect_err("unknown reference");
    assert_eq!(error.kind(), PluginErrorKind::Config);
    assert_eq!(std::fs::read(&path).expect("after"), before);
}

#[test]
fn cli_jsonc_import_uses_direct_token_and_cli_headers() {
    let fixture = fixture("cli");
    let path = fixture.home.join("config.json");
    write(
        &path,
        r#"// Copilot CLI's own file
        {
          "lastLoggedInUser":{"host":"https://github.com","login":"octo"},
          "loggedInUsers":[],
          "copilotTokens":{"https://github.com:octo":"gho-cli"}, // plaintext choice
        }"#,
    );
    let before = std::fs::read(&path).expect("before");
    let id = import(&fixture, "octo");
    let sub = subscription(&fixture.host);
    let discovered = block_on(sub.discover_agent_sign_in(&fixture.home)).expect("trait discovery");
    assert_eq!(discovered.len(), 1);
    assert_eq!(discovered[0].label(), "octo");
    assert_eq!(discovered[0].id(), &id);
    fixture.host.http().enqueue(
        json!({"copilot_plan":"individual","endpoints":{"api":API}})
            .to_string()
            .into_bytes(),
    );
    fixture.host.http().enqueue(
        br#"{"data":[{"id":"gpt-4.1","model_picker_enabled":true,"capabilities":{"type":"chat"}}]}"#
            .to_vec(),
    );
    block_on(sub.refresh(&id, fixture.host.context())).expect("direct session");
    let list = block_on(sub.list_models(&id, fixture.host.context())).expect("models");
    assert_eq!(list.models()[0].id(), "gpt-4.1");
    let calls = fixture.host.http().calls();
    assert_eq!(
        calls[0].uri(),
        "https://api.github.com/copilot_internal/user"
    );
    assert_eq!(calls[1].uri(), format!("{API}/models"));
    assert_eq!(
        calls[1].header("Copilot-Integration-Id"),
        Some("copilot-developer-cli")
    );
    assert_eq!(calls[1].header("Editor-Version"), Some("copilot/1.0.88"));
    assert_eq!(calls[1].header("X-GitHub-Api-Version"), Some("2026-07-01"));
    assert!(stored(&fixture.host, &id).secret("oauth_token").is_none());
    assert_eq!(std::fs::read(&path).expect("after"), before);
}

#[cfg(target_os = "linux")]
#[test]
fn cli_keychain_tries_account_then_username() {
    let fixture = fixture("keychain");
    write(
        &fixture.home.join("config.json"),
        r#"{"lastLoggedInUser":{"host":"https://github.com","login":"octo"}}"#,
    );
    let id = import(&fixture, "octo");
    fixture
        .host
        .process()
        .script_piped(PipedScript::new().exits(|_| Vec::new()));
    fixture
        .host
        .process()
        .script_piped(PipedScript::new().line("gho-keychain").exits(|spawn| {
            assert_eq!(spawn.program, PathBuf::from("/usr/bin/secret-tool"));
            let args = spawn.arg_strings();
            assert!(
                args.windows(2)
                    .any(|pair| pair == ["username", "https://github.com:octo"])
            );
            Vec::new()
        }));
    fixture
        .host
        .http()
        .enqueue(json!({"endpoints":{"api":API}}).to_string().into_bytes());
    block_on(subscription(&fixture.host).refresh(&id, fixture.host.context()))
        .expect("keychain session");
}

#[test]
fn stale_editor_needs_sign_in_without_touching_file_then_recovers() {
    let fixture = fixture("stale");
    let path = editor(
        &fixture,
        r#"{"github.com:Iv1.one":{"user":"octo","oauth_token":"gho-stale"}}"#,
    );
    let before = std::fs::read(&path).expect("before");
    let id = import(&fixture, "octo");
    let sub = subscription(&fixture.host);
    fixture
        .host
        .http()
        .enqueue_response(401, &[], br#"{"message":"Bad credentials"}"#.to_vec());
    let error = block_on(sub.refresh(&id, fixture.host.context())).expect_err("stale");
    assert_eq!(error.kind(), PluginErrorKind::Auth);
    assert!(stored(&fixture.host, &id).flag(LAPSED));
    assert_eq!(
        block_on(sub.account_status(&id, fixture.host.context())).expect("status"),
        AccountStatus::NeedsSignIn {
            reason: LAPSED_REASON.to_owned()
        }
    );
    assert_eq!(std::fs::read(&path).expect("unchanged"), before);

    write(
        &path,
        r#"{"github.com:Iv1.one":{"user":"octo","oauth_token":"gho-fresh"}}"#,
    );
    let recovered = std::fs::read(&path).expect("recovered");
    fixture.host.http().enqueue(session_reply());
    block_on(sub.refresh(&id, fixture.host.context())).expect("fresh token");
    assert!(!stored(&fixture.host, &id).flag(LAPSED));
    assert_eq!(
        block_on(sub.account_status(&id, fixture.host.context())).expect("status"),
        AccountStatus::Ready
    );
    assert_eq!(std::fs::read(&path).expect("still unchanged"), recovered);
}

fn device_replies(fixture: &Fixture) {
    fixture.host.http().enqueue(
        br#"{"device_code":"dev-1","user_code":"ABCD-1234","verification_uri":"https://github.com/login/device","expires_in":900,"interval":1}"#
            .to_vec(),
    );
    fixture.host.http().enqueue(
        br#"{"access_token":"gho-device","token_type":"bearer","scope":"read:user"}"#.to_vec(),
    );
    fixture
        .host
        .http()
        .enqueue(br#"{"login":"octo","id":42}"#.to_vec());
    fixture
        .host
        .http()
        .enqueue(br#"{"copilot_plan":"individual"}"#.to_vec());
}

#[test]
fn valid_editor_sign_in_is_not_replaced_by_device_sign_in() {
    let fixture = fixture("valid-editor");
    let path = editor(
        &fixture,
        r#"{"github.com:Iv1.one":{"user":"octo","oauth_token":"gho-editor"}}"#,
    );
    let before = std::fs::read(&path).expect("before");
    let id = import(&fixture, "octo");
    let sub = subscription(&fixture.host);
    let ctx = SignInCtx::new(PluginId::from_static("copilot"));
    device_replies(&fixture);
    block_on(sub.start_sign_in(&ctx, fixture.host.context())).expect("start");
    fixture.host.http().enqueue(session_reply());
    let signed = block_on(sub.complete_sign_in(&ctx, SignInProof::Await, fixture.host.context()))
        .expect("complete");
    assert_eq!(signed, id);
    assert!(stored(&fixture.host, &id).secret("oauth_token").is_none());
    assert_eq!(std::fs::read(&path).expect("unchanged"), before);
}

#[test]
fn device_sign_in_replaces_stale_editor_reference_at_same_id() {
    let fixture = fixture("stale-device");
    let path = editor(
        &fixture,
        r#"{"github.com:Iv1.one":{"user":"octo","oauth_token":"gho-stale"}}"#,
    );
    let before = std::fs::read(&path).expect("before");
    let id = import(&fixture, "octo");
    let sub = subscription(&fixture.host);
    let ctx = SignInCtx::new(PluginId::from_static("copilot"));
    device_replies(&fixture);
    block_on(sub.start_sign_in(&ctx, fixture.host.context())).expect("start");
    fixture
        .host
        .http()
        .enqueue_response(401, &[], br#"{"message":"Bad credentials"}"#.to_vec());
    let signed = block_on(sub.complete_sign_in(&ctx, SignInProof::Await, fixture.host.context()))
        .expect("complete");
    assert_eq!(signed, id);
    assert!(stored(&fixture.host, &id).secret("oauth_token").is_some());
    assert_eq!(std::fs::read(&path).expect("unchanged"), before);
}

#[test]
fn device_and_editor_accounts_coexist_and_legacy_ghe_label_keeps_id() {
    let fixture = fixture("coexist");
    editor(
        &fixture,
        r#"{"github.com:Iv1.one":{"user":"editor","oauth_token":"gho-editor"}}"#,
    );
    let imported = import(&fixture, "editor");
    let device = AccountId::from_static("a-0000000000000042");
    let blob = SecretObject::new()
        .with_number("v", 1)
        .with_secret("oauth_token", &"gho-device".to_owned().into())
        .with_text("host", "acme.ghe.com")
        .with_text("login", "octo")
        .to_secret();
    block_on(async {
        let account = fixture
            .host
            .context()
            .accounts()
            .account(device.clone(), "octo")
            .expect("account");
        fixture
            .host
            .context()
            .accounts()
            .upsert(account, blob)
            .await
            .expect("device");
    });
    let sub = subscription(&fixture.host);
    assert_eq!(
        block_on(sub.account_status(&device, fixture.host.context())).expect("status"),
        AccountStatus::Ready
    );
    let accounts = block_on(fixture.host.context().accounts().list()).expect("accounts");
    assert_eq!(accounts.len(), 2);
    let legacy = accounts
        .iter()
        .find(|account| account.id() == &device)
        .expect("legacy");
    assert_eq!(legacy.label(), "octo@acme.ghe.com");
    assert!(accounts.iter().any(|account| account.id() == &imported));
}
