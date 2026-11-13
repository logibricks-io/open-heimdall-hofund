//! Offline tests for Kiro sign-in import and refresh sources.

use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use hofund_kit_subscription::{PipedScript, TestHost, block_on};
use hofund_plugin_api::{
    AccountId, PluginErrorKind, PluginId, PluginKind, SecretString, SubscriptionFactory,
};
use hofund_plugin_sdk::json::{self, Value};
use hofund_plugin_sdk::oauth::secret_text;
use rusqlite::Connection;

use crate::creds::{self, Credential};
use crate::{Endpoints, KiroConfig, cli_db, discover, factory_with_cli_command};

const NOW: std::time::SystemTime = UNIX_EPOCH;

fn host(dir: &tempfile::TempDir) -> TestHost {
    TestHost::open_for_at(
        PluginKind::Subscription,
        PluginId::from_static("kiro"),
        dir.path(),
        NOW,
    )
}

fn credential(method: &str) -> Credential {
    Credential {
        access: SecretString::from("old-access".to_owned()),
        refresh: SecretString::from("old-refresh".to_owned()),
        expires_at: Some(UNIX_EPOCH + Duration::from_secs(1)),
        region: "us-east-1".to_owned(),
        profile_arn: "arn:aws:codewhisperer:us-east-1:1:profile/P".to_owned(),
        label: "Kiro".to_owned(),
        method: method.to_owned(),
        provider: String::new(),
        client_id: "client id".to_owned(),
        client_secret: SecretString::from(String::new()),
        client_id_hash: String::new(),
        token_url: String::new(),
        ide_token_path: String::new(),
        db_key: String::new(),
        cli_db_path: String::new(),
    }
}

fn store(host: &TestHost, home: &Path, credential: &Credential) -> AccountId {
    let id = creds::account_id_for_home(home, credential);
    block_on(async {
        let accounts = host.context().accounts();
        let account = accounts
            .account(id.clone(), credential.label.clone())
            .expect("account");
        accounts
            .upsert(account, creds::secret_blob(credential).to_secret())
            .await
            .expect("store");
    });
    id
}

fn stored(host: &TestHost, id: &AccountId) -> Credential {
    let secret = block_on(host.context().accounts().secret(id))
        .expect("read")
        .expect("stored");
    creds::parse_secret(&secret).expect("credential").credential
}

fn fixture(path: &Path, key: &str, token: Value) {
    let connection = Connection::open(path).expect("fixture database");
    connection
        .execute_batch("CREATE TABLE auth_kv (key TEXT PRIMARY KEY, value TEXT)")
        .expect("fixture schema");
    connection
        .execute(
            "INSERT INTO auth_kv (key, value) VALUES (?1, ?2)",
            (key, json::to_string(&token).expect("fixture JSON")),
        )
        .expect("fixture token");
}

fn factory(command: &Path) -> crate::KiroFactory {
    factory_with_cli_command(Endpoints::default(), command.to_path_buf())
}

#[test]
fn r51b_cli_discovery_and_import_use_only_the_fixture_database() {
    let dir = tempfile::tempdir().expect("fixture directory");
    let path = dir.path().join("data.sqlite3");
    fixture(
        &path,
        "kirocli:social:token",
        json::json!({"access_token":"fixture-access","refresh_token":"fixture-refresh","expires_at":"2099-01-01T00:00:00Z"}),
    );
    let home = dir.path().join("ide-cache");
    let rows = discover::list_at(&home, Some(&path)).expect("discover");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].reference(), discover::CLI_REFERENCE);
    let host = host(&dir);
    let id = block_on(discover::import_at(
        &home,
        discover::CLI_REFERENCE,
        host.context(),
        Some(&path),
    ))
    .expect("import");
    let stored = stored(&host, &id);
    assert_eq!(secret_text(&stored.access), "fixture-access");
    assert_eq!(stored.db_key, "kirocli:social:token");
    assert_eq!(stored.cli_db_path, path.to_string_lossy());
}

#[test]
fn r51b_external_idp_refresh_posts_form_and_maps_snake_case_reply() {
    let dir = tempfile::tempdir().expect("fixture directory");
    let host = host(&dir);
    let mut credential = credential("external-idp");
    credential.refresh = SecretString::from("r +/&".to_owned());
    credential.token_url = "https://idp.example.test/v1/token".to_owned();
    let id = store(&host, dir.path(), &credential);
    let sub = factory_with_cli_command(Endpoints::default(), dir.path().join("unused-kiro-cli"))
        .build(&KiroConfig, host.context())
        .expect("build");
    host.http().enqueue(
        br#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":7200}"#
            .to_vec(),
    );
    block_on(sub.refresh(&id, host.context())).expect("refresh");
    let calls = host.http().calls();
    let call = calls.last().expect("refresh call");
    assert_eq!(call.uri(), "https://idp.example.test/v1/token");
    assert_eq!(
        call.header("content-type"),
        Some("application/x-www-form-urlencoded")
    );
    assert_eq!(
        std::str::from_utf8(call.body()).expect("form"),
        "grant_type=refresh_token&client_id=client%20id&refresh_token=r%20%2B%2F%26"
    );
    let stored = stored(&host, &id);
    assert_eq!(secret_text(&stored.access), "new-access");
    assert_eq!(secret_text(&stored.refresh), "new-refresh");
    assert_eq!(stored.expires_at, Some(NOW + Duration::from_secs(7200)));
}

#[test]
fn r51b_external_idp_without_https_token_url_fails_closed() {
    for token_url in [
        "",
        "http://idp.example.test/token",
        "https://user:pass@idp.example.test/token",
    ] {
        let dir = tempfile::tempdir().expect("fixture directory");
        let host = host(&dir);
        let mut credential = credential("external-idp");
        credential.token_url = token_url.to_owned();
        let id = store(&host, dir.path(), &credential);
        let sub = factory(&dir.path().join("unused-kiro-cli"))
            .build(&KiroConfig, host.context())
            .expect("build");
        let error = block_on(sub.refresh(&id, host.context())).expect_err("refused");
        assert_eq!(error.kind(), PluginErrorKind::Auth);
        assert!(host.http().calls().is_empty());
    }
}

#[test]
fn r51b_cli_delegated_refresh_runs_debug_command_and_reads_changed_token() {
    let dir = tempfile::tempdir().expect("fixture directory");
    let path = dir.path().join("data.sqlite3");
    let key = "kirocli:social:token";
    fixture(
        &path,
        key,
        json::json!({"access_token":"old-access","refresh_token":"old-refresh","expires_at":"1970-01-01T00:00:01Z"}),
    );
    let credential = cli_db::read_at(&path).expect("read").expect("credential");
    let host = host(&dir);
    let id = store(&host, dir.path(), &credential);
    let program = dir.path().join("fake-kiro-cli");
    let updated_path = path.clone();
    host.process().script_piped(PipedScript::new().exits(move |spawn| {
        assert_eq!(spawn.arg_strings(), ["debug", "refresh-auth-token"]);
        let connection = Connection::open(&updated_path).expect("fixture database");
        connection
            .execute(
                "UPDATE auth_kv SET value = ?1 WHERE key = ?2",
                (json::to_string(&json::json!({"access_token":"delegated-access","refresh_token":"delegated-refresh","expires_at":"2099-01-01T00:00:00Z"})).expect("JSON"), key),
            )
            .expect("delegated refresh");
        Vec::new()
    }));
    let sub = factory(&program)
        .build(&KiroConfig, host.context())
        .expect("build");
    block_on(sub.refresh(&id, host.context())).expect("refresh");
    assert!(host.http().calls().is_empty());
    let stored = stored(&host, &id);
    assert_eq!(secret_text(&stored.access), "delegated-access");
    assert_eq!(secret_text(&stored.refresh), "delegated-refresh");
    assert_eq!(host.process().piped_spawns().len(), 1);
}

#[test]
fn r51b_offline_cli_refresh_writes_back_after_delegation_makes_no_change() {
    let dir = tempfile::tempdir().expect("fixture directory");
    let path = dir.path().join("data.sqlite3");
    let key = "kirocli:external-idp:token";
    fixture(
        &path,
        key,
        json::json!({"access_token":"old-access","refresh_token":"old-refresh","expires_at":"1970-01-01T00:00:01Z","client_id":"external-client","token_endpoint":"https://idp.example.test/token","custom":"kept"}),
    );
    let credential = cli_db::read_at(&path).expect("read").expect("credential");
    let host = host(&dir);
    let id = store(&host, dir.path(), &credential);
    let program = dir.path().join("fake-kiro-cli");
    host.process()
        .script_piped(PipedScript::new().exits(|_| Vec::new()));
    let sub = factory(&program)
        .build(&KiroConfig, host.context())
        .expect("build");
    host.http().enqueue(
        br#"{"access_token":"offline-access","refresh_token":"offline-refresh","expires_in":3600}"#
            .to_vec(),
    );
    block_on(sub.refresh(&id, host.context())).expect("refresh");
    let connection = Connection::open(&path).expect("fixture database");
    let body: String = connection
        .query_row("SELECT value FROM auth_kv WHERE key = ?1", [key], |row| {
            row.get(0)
        })
        .expect("updated row");
    let updated: Value = json::from_str(&body).expect("updated JSON");
    assert_eq!(updated["access_token"], "offline-access");
    assert_eq!(updated["refresh_token"], "offline-refresh");
    assert_eq!(updated["custom"], "kept");
    assert_eq!(secret_text(&stored(&host, &id).access), "offline-access");
}
