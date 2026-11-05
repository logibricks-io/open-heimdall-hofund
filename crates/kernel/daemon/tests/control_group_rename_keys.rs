//! Group rename must rewrite gateway key model scopes and roll back on failure.

mod support;

use std::path::PathBuf;
use std::sync::LazyLock;

use hofund_control_client::{Client as HttpClient, ClientError};
use hofund_control_proto::{API_VERSION, ControlErrorKind, GroupPatchRequest};
use hofund_daemon::{
    ControlRuntime, GroupRenameFailAfter, Running, StartOptions, set_group_rename_fail_after,
    start_with_options,
};
use hofund_secrets::KeyStore;
use serde_json::json;
use tokio::sync::Mutex as AsyncMutex;

static GROUP_RENAME_TEST_LOCK: LazyLock<AsyncMutex<()>> = LazyLock::new(|| AsyncMutex::new(()));

const SECRET: &str = "sk-group-rename-keys";

fn config(extra: &str) -> String {
    format!(
        "listen = \"127.0.0.1:0\"\n{extra}\
[[providers]]\nid = \"chat\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key = \"{SECRET}\"\n\
[[routes]]\nmodel = \"m\"\nupstreams = [\"chat\"]\n"
    )
}

struct Scratch(PathBuf, #[allow(dead_code)] tempfile::TempDir);

impl Scratch {
    fn new(label: &str) -> Self {
        let guard = tempfile::Builder::new()
            .prefix(&format!("hofund-group-rename-{label}-"))
            .tempdir()
            .expect("scratch");
        Self(guard.path().to_path_buf(), guard)
    }

    fn path(&self) -> PathBuf {
        self.0.join("hofund.toml")
    }

    fn token_path(&self) -> PathBuf {
        self.0.join("hofund.token")
    }
}

async fn start(dir: &Scratch, source: &str) -> Running {
    std::fs::write(dir.path(), source).expect("config");
    start_with_options(
        &dir.path(),
        StartOptions {
            control_listen: Some("127.0.0.1:0".parse().expect("control")),
            control_runtime: Some(ControlRuntime::new(None, dir.0.clone())),
            skip_config_watch: true,
            hofund_addr: Some(String::new()),
            ..StartOptions::default()
        },
    )
    .await
    .expect("daemon")
}

fn scope_gateway_key(dir: &Scratch, models: &[&str]) {
    let store = KeyStore::beside_token(dir.token_path());
    let mut keys = store.export().expect("export");
    keys[0].models = models.iter().map(|entry| (*entry).to_owned()).collect();
    store.restore(&keys).expect("seed scopes");
}

fn group_fixture() -> String {
    config(
        "[[routes]]\nmodel = \"group/old\"\nname = \"Old\"\nrouting = \"smart\"\nupstreams = [\"chat\"]\n",
    )
}

fn group_patch(value: serde_json::Value) -> GroupPatchRequest {
    serde_json::from_value(value).expect("group patch request")
}

#[tokio::test]
async fn r7a_rr11_group_rename_moves_exact_key_scope_and_keeps_wildcard() {
    let _guard = GROUP_RENAME_TEST_LOCK.lock().await;
    set_group_rename_fail_after(None);
    let scratch = Scratch::new("keys-success");
    let running = start(&scratch, &group_fixture()).await;
    scope_gateway_key(&scratch, &["group/old", "group/*", "m"]);
    let token = std::fs::read_to_string(scratch.token_path())
        .expect("token")
        .trim()
        .to_owned();
    let client = HttpClient::tcp(running.control_local_addr().expect("control"), token);
    let read = client.config().await.expect("config");
    let patch = group_patch(json!({
        "version": API_VERSION,
        "expected_revision": read.disk_revision,
        "rename_to": "new",
    }));
    client
        .patch_group("old", &patch)
        .await
        .expect("rename");
    let text = std::fs::read_to_string(scratch.path()).expect("config");
    assert!(text.contains("model = \"group/new\""), "{text}");
    let store = KeyStore::beside_token(scratch.token_path());
    let key = store.list().expect("keys")[0].clone();
    assert_eq!(key.models, ["group/new", "group/*", "m"]);
    support::shutdown(running).await;
}

#[tokio::test]
async fn r7a_rr11_group_rename_key_failure_restores_config_and_key_scopes() {
    let _guard = GROUP_RENAME_TEST_LOCK.lock().await;
    set_group_rename_fail_after(None);
    let scratch = Scratch::new("keys-rollback");
    let config_before = group_fixture();
    let running = start(&scratch, &config_before).await;
    scope_gateway_key(&scratch, &["GROUP/old", "group/*"]);
    let token = std::fs::read_to_string(scratch.token_path())
        .expect("token")
        .trim()
        .to_owned();
    let client = HttpClient::tcp(running.control_local_addr().expect("control"), token);
    let read = client.config().await.expect("config");
    let patch = group_patch(json!({
        "version": API_VERSION,
        "expected_revision": read.disk_revision,
        "rename_to": "new",
    }));
    set_group_rename_fail_after(Some(GroupRenameFailAfter::Keys));
    let error = client
        .patch_group("old", &patch)
        .await
        .expect_err("rename must fail");
    set_group_rename_fail_after(None);
    match error {
        ClientError::Server { error, .. } => {
            assert_eq!(error.error.kind, ControlErrorKind::Internal);
            assert!(
                error.error.message.contains("keys failure"),
                "{}",
                error.error.message
            );
            assert!(
                !error.error.message.contains(SECRET),
                "error must not leak secrets"
            );
        }
        other => panic!("expected server error, got {other}"),
    }
    assert_eq!(
        std::fs::read_to_string(scratch.path()).expect("config"),
        config_before
    );
    let store = KeyStore::beside_token(scratch.token_path());
    let key = store.list().expect("keys")[0].clone();
    assert_eq!(key.models, ["GROUP/old", "group/*"]);
    support::shutdown(running).await;
}
