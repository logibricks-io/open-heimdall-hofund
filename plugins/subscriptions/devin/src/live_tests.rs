use std::path::{Path, PathBuf};
use std::sync::Arc;

use hofund_kit_subscription::{TestHost, block_on};
use hofund_plugin_api::{
    AccountId, Completion, PluginId, PluginKind, ProbeQuery, ProtocolId, RawTable, Request,
    SecretString, SubscriptionFactory, UpstreamProvider,
};
use hofund_plugin_sdk::oauth::{SecretObject, secret_text};
use hofund_plugin_sdk::protobuf::{FLAG_END, ProtoBuf, fields, frame};

use crate::creds::{self, Credential};
use crate::{DevinFactory, connect};

fn host(dir: &tempfile::TempDir) -> TestHost {
    TestHost::open_for(
        PluginKind::Subscription,
        PluginId::from_static("devin"),
        dir.path(),
    )
}

fn credential(key: &str, server: &str, label: &str) -> Credential {
    Credential {
        key: SecretString::from(key.to_owned()),
        api_server: server.to_owned(),
        devin_api: "https://api.devin.ai".to_owned(),
        label: label.to_owned(),
        plan: "Pro".to_owned(),
        org_id: "org-1".to_owned(),
        team_id: "team-1".to_owned(),
    }
}

fn seed(host: &TestHost, credential: &Credential, cli_path: Option<&Path>) -> AccountId {
    let id = creds::account_id(credential);
    let blob = match cli_path {
        Some(path) => creds::cli_secret_blob(credential, path),
        None => creds::secret_blob(credential),
    };
    block_on(async {
        let account = host
            .context()
            .accounts()
            .account(id.clone(), credential.label.clone())
            .expect("account");
        host.context()
            .accounts()
            .upsert(account, blob.to_secret())
            .await
            .expect("upsert");
    });
    id
}

fn upstream(host: &TestHost, id: &AccountId) -> Arc<dyn UpstreamProvider> {
    let cfg = DevinFactory.validate(&RawTable::new()).expect("config");
    DevinFactory
        .build(&cfg, host.context())
        .expect("build")
        .upstream(id, host.context())
        .expect("upstream")
}

fn request() -> Request {
    let mut request = ProbeQuery::new(ProtocolId::OPENAI_CHAT, "swe-2")
        .expect("query")
        .minimal_request();
    request.stream = true;
    request
}

fn enqueue_chat(host: &TestHost) {
    host.http().enqueue_response(
        200,
        &[("content-type", "application/proto")],
        connect::testdata::MODEL_CONFIGS,
    );
    let text = ProtoBuf::new().string(3, "ok").finish();
    let body = [
        frame(0, &text).expect("text"),
        frame(FLAG_END, b"{}").expect("end"),
    ]
    .concat();
    host.http().enqueue(body);
}

fn execute(host: &TestHost, id: &AccountId) -> Completion {
    block_on(upstream(host, id).execute(request(), ProtocolId::OPENAI_CHAT)).expect("execute")
}

fn chat_key(body: &[u8]) -> String {
    let request = hofund_plugin_sdk::protobuf::deframe(body).expect("request frame");
    let outer = fields(&request.payload);
    let metadata = outer
        .iter()
        .find(|field| field.number == 1)
        .and_then(|field| field.as_bytes())
        .expect("metadata");
    fields(metadata)
        .iter()
        .find(|field| field.number == 3)
        .and_then(|field| field.as_string())
        .expect("session token")
}

#[test]
fn cli_execute_uses_rotated_key_and_server_without_rewriting_the_store() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir
        .path()
        .join("agent")
        .join("devin")
        .join("credentials.toml");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    std::fs::write(
        &path,
        "windsurf_api_key = \"rotated-key\"\napi_server_url = \"https://rotated.test/\"\n",
    )
    .expect("credentials");
    let host = host(&dir);
    let original = credential("saved-key", "https://saved.test", "cli@example.test");
    let id = seed(&host, &original, Some(&path));
    enqueue_chat(&host);
    drop(execute(&host, &id));

    let calls = host.http().calls();
    assert_eq!(calls.len(), 2);
    assert!(calls[0].uri().starts_with("https://rotated.test/"));
    assert!(calls[1].uri().starts_with("https://rotated.test/"));
    assert_eq!(chat_key(calls[1].body()), "rotated-key");
    let secret = block_on(host.context().accounts().secret(&id))
        .expect("secret")
        .expect("stored");
    let blob = SecretObject::parse(&secret).expect("blob");
    assert_eq!(
        blob.secret("api_key").as_ref().map(secret_text),
        Some("saved-key")
    );
    assert_eq!(blob.text("api_server"), Some("https://saved.test"));
    assert_eq!(blob.text("label"), Some("cli@example.test"));
    assert_eq!(blob.text("plan"), Some("Pro"));
    assert_eq!(blob.text("cli"), Some("true"));
}

#[test]
fn missing_or_keyless_cli_file_keeps_the_saved_credentials() {
    for body in [None, Some("api_server_url = \"https://ignored.test/\"\n")] {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("devin").join("credentials.toml");
        if let Some(body) = body {
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
            std::fs::write(&path, body).expect("credentials");
        }
        let host = host(&dir);
        let original = credential("saved-key", "https://saved.test", "cli@example.test");
        let id = seed(&host, &original, Some(&path));
        let live = block_on(creds::Stored::live(host.context(), &id)).expect("live");
        assert_eq!(secret_text(&live.credential.key), "saved-key");
        assert_eq!(live.credential.api_server, "https://saved.test");
        assert_eq!(live.cli_path.as_deref(), Some(path.as_path()));
    }
}

#[test]
fn browser_account_does_not_follow_a_cli_file() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("devin").join("credentials.toml");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    std::fs::write(&path, "windsurf_api_key = \"rotated-key\"\n").expect("credentials");
    let host = host(&dir);
    let original = credential(
        "browser-key",
        "https://browser.test",
        "browser@example.test",
    );
    let id = seed(&host, &original, None);
    let live = block_on(creds::Stored::live(host.context(), &id)).expect("live");
    assert_eq!(secret_text(&live.credential.key), "browser-key");
    assert_eq!(live.credential.api_server, "https://browser.test");
    assert_eq!(live.cli_path, None);
}

#[test]
fn live_reread_waits_for_the_account_lease() {
    let dir = tempfile::tempdir().expect("dir");
    let path = PathBuf::from(dir.path()).join("credentials.toml");
    std::fs::write(&path, "windsurf_api_key = \"rotated-key\"\n").expect("credentials");
    let host = host(&dir);
    let original = credential("saved-key", "https://saved.test", "cli@example.test");
    let id = seed(&host, &original, Some(&path));
    block_on(async {
        let lease = host
            .context()
            .accounts()
            .lease(&id, std::time::Duration::ZERO)
            .await
            .expect("held lease");
        let context = host.context().clone();
        let account = id.clone();
        let waiting = std::thread::spawn(move || {
            block_on(creds::Stored::live(&context, &account)).expect("live")
        });
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(!waiting.is_finished());
        drop(lease);
        let live = waiting.join().expect("join");
        assert_eq!(secret_text(&live.credential.key), "rotated-key");
    });
}

#[test]
fn chat_401_marks_needs_sign_in_without_deleting_the_secret() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let original = credential("saved-key", "https://saved.test", "saved@example.test");
    let id = seed(&host, &original, None);
    host.http().enqueue_response(
        200,
        &[("content-type", "application/proto")],
        connect::testdata::MODEL_CONFIGS,
    );
    host.http().enqueue_response(
        401,
        &[],
        br#"{"code":"unauthenticated","message":"expired"}"#,
    );
    let error = match block_on(upstream(&host, &id).execute(request(), ProtocolId::OPENAI_CHAT)) {
        Err(error) => error,
        Ok(_) => panic!("expired chat must fail"),
    };
    assert_eq!(error.status(), Some(401));
    let stored = block_on(creds::Stored::load(host.context(), &id)).expect("stored");
    assert!(stored.needs_sign_in);
    assert_eq!(secret_text(&stored.credential.key), "saved-key");
}
