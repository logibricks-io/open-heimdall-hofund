use std::time::Duration;

use hofund_kit_subscription::{TestHost, block_on};
use hofund_plugin_api::{
    AccountId, PluginErrorKind, PluginId, PluginKind, RawTable, SecretString, SubscriptionFactory,
};
use hofund_plugin_sdk::oauth::{SecretObject, account_id_from};

use crate::{Endpoints, factory_with_endpoints, project};

const TOKEN_URL: &str = "https://oauth.test/token";
const USERINFO_URL: &str = "https://oauth.test/userinfo";
const CODE_ASSIST: &str = "https://code-assist.test";

fn endpoints() -> Endpoints {
    Endpoints {
        authorize: "https://accounts.test/o/oauth2/v2/auth".to_owned(),
        token: TOKEN_URL.to_owned(),
        userinfo: USERINFO_URL.to_owned(),
        code_assist: CODE_ASSIST.to_owned(),
        onboard_poll: Duration::from_millis(1),
    }
}

fn host(dir: &tempfile::TempDir) -> TestHost {
    TestHost::open_for(
        PluginKind::Subscription,
        PluginId::from_static("gemini"),
        dir.path(),
    )
}

fn subscription(host: &TestHost) -> std::sync::Arc<dyn hofund_plugin_api::Subscription> {
    let factory = factory_with_endpoints(endpoints());
    let config = factory.validate(&RawTable::new()).expect("config");
    factory.build(&config, host.context()).expect("build")
}

fn token_reply(access: &str) -> Vec<u8> {
    format!(r#"{{"access_token":"{access}","token_type":"Bearer","expires_in":3600}}"#).into_bytes()
}

fn userinfo(email: &str) -> Vec<u8> {
    format!(r#"{{"email":"{email}"}}"#).into_bytes()
}

fn stored(host: &TestHost, id: &AccountId) -> SecretObject {
    let secret = block_on(host.context().accounts().secret(id))
        .expect("read")
        .expect("stored");
    SecretObject::parse(&secret).expect("blob")
}

fn seed(host: &TestHost, email: &str, refresh: &str) -> AccountId {
    let id = account_id_from(&["google", &email.to_ascii_lowercase()]);
    let blob = SecretObject::new()
        .with_number("v", 1)
        .with_secret("refresh_token", &SecretString::from(refresh.to_owned()))
        .with_text("email", email)
        .with_text(project::PROJECT, "stored-project");
    let account = host
        .context()
        .accounts()
        .account(id.clone(), email)
        .expect("account");
    block_on(host.context().accounts().upsert(account, blob.to_secret())).expect("seed");
    id
}

#[test]
fn r49d_imports_manager_and_cliproxy_exports_with_project_fallback() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    host.http().enqueue(token_reply("access-a"));
    host.http().enqueue(userinfo("a@example.test"));
    host.http().enqueue(
        br#"{"currentTier":{"id":"standard-tier","name":"Gemini Code Assist"},"cloudaicompanionProject":"found-a"}"#
            .to_vec(),
    );
    host.http().enqueue(token_reply("access-b"));
    host.http().enqueue_response(401, &[], Vec::new());
    host.http().enqueue_response(503, &[], Vec::new());
    let text = r#"[
        {"email":"a@example.test","token":{"refresh_token":"refresh-a","project_id":"file-a"}},
        {"type":"gemini","email":"b@example.test","refresh_token":"refresh-b","project_id":"file-b"}
    ]"#;
    let ids = block_on(sub.import_accounts(text, host.context())).expect("import");
    assert_eq!(
        ids,
        [
            account_id_from(&["google", "a@example.test"]),
            account_id_from(&["google", "b@example.test"]),
        ]
    );
    assert_eq!(
        stored(&host, &ids[0]).text(project::PROJECT),
        Some("found-a")
    );
    assert_eq!(
        stored(&host, &ids[1]).text(project::PROJECT),
        Some("file-b")
    );
    assert_eq!(stored(&host, &ids[1]).text("email"), Some("b@example.test"));
    assert!(stored(&host, &ids[0]).secret("access_token").is_none());
    assert!(stored(&host, &ids[1]).secret("access_token").is_none());
    let calls = host.http().calls();
    assert_eq!(calls.len(), 6);
    assert!(calls[0].uri().starts_with(TOKEN_URL));
    assert!(calls[1].uri().starts_with(USERINFO_URL));
    assert!(calls[3].uri().starts_with(TOKEN_URL));
    assert!(calls[4].uri().starts_with(USERINFO_URL));
}

#[test]
fn r49d_existing_refresh_is_not_refreshed_and_a_new_one_replaces_it() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = seed(&host, "u@example.test", "refresh-old");
    let text = r#"{"email":"u@example.test","refresh_token":"refresh-old"}"#;
    let ids = block_on(sub.import_accounts(text, host.context())).expect("same import");
    assert_eq!(ids, vec![id.clone()]);
    assert!(host.http().calls().is_empty());

    host.http().enqueue(token_reply("access-new"));
    host.http().enqueue(userinfo("u@example.test"));
    host.http().enqueue_response(503, &[], Vec::new());
    let text = r#"{"email":"u@example.test","refresh_token":"refresh-new"}"#;
    let ids = block_on(sub.import_accounts(text, host.context())).expect("replacement");
    assert_eq!(ids, vec![id.clone()]);
    assert_eq!(
        stored(&host, &id)
            .secret("refresh_token")
            .map(|token| hofund_plugin_sdk::oauth::secret_text(&token).to_owned())
            .as_deref(),
        Some("refresh-new")
    );
}

#[test]
fn r49d_wrong_app_and_bad_entries_fail_before_http_without_token_text() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let error = block_on(sub.import_accounts(
        r#"{"type":"antigravity","refresh_token":"secret-refresh","email":"a@example.test"}"#,
        host.context(),
    ))
    .expect_err("wrong app");
    assert_eq!(error.kind(), PluginErrorKind::Config);
    assert!(!error.message().contains("secret-refresh"));
    assert!(host.http().calls().is_empty());

    let error = block_on(sub.import_accounts(
        r#"[{"email":"a@example.test","refresh_token":"duplicate"},{"email":"A@example.test","refresh_token":"other"}]"#,
        host.context(),
    ))
    .expect_err("duplicate");
    assert!(error.message().contains("in the file twice"));
    assert!(!error.message().contains("duplicate"));
    assert!(!error.message().contains("other"));
    assert!(host.http().calls().is_empty());
}
