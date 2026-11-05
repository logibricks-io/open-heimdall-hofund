//! Group rename key scope moves and rollback without whole-store restore.

mod support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use hofund_agent::Session;
use hofund_control_client::{Client as HttpClient, ClientError};
use hofund_control_proto::{API_VERSION, AgentConnectRequest, ControlErrorKind, GroupPatchRequest};
use hofund_daemon::{ControlRuntime, Running, StartOptions, start_with_options};
use hofund_secrets::{BLOCK_MOVE_MODEL_SENTINEL, FAIL_MOVE_MODEL_SENTINEL, KeyStore};
use serde_json::json;

const SECRET: &str = "sk-group-rename-keys";

fn config(extra: &str) -> String {
    format!(
        "listen = \"127.0.0.1:0\"\n{extra}\
[[providers]]\nid = \"chat\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key = \"{SECRET}\"\n\
[[routes]]\nmodel = \"m\"\nupstreams = [\"chat\"]\n"
    )
}

struct Scratch {
    root: PathBuf,
    runtime_dir: PathBuf,
    _root_guard: tempfile::TempDir,
    _runtime_guard: tempfile::TempDir,
}

impl Scratch {
    fn new(label: &str) -> Self {
        let root_guard = tempfile::Builder::new()
            .prefix(&format!("hofund-group-rename-{label}-cfg-"))
            .tempdir()
            .expect("config scratch");
        let runtime_guard = tempfile::Builder::new()
            .prefix(&format!("hofund-group-rename-{label}-rt-"))
            .tempdir()
            .expect("runtime scratch");
        Self {
            root: root_guard.path().to_path_buf(),
            runtime_dir: runtime_guard.path().to_path_buf(),
            _root_guard: root_guard,
            _runtime_guard: runtime_guard,
        }
    }

    fn config_path(&self) -> PathBuf {
        self.root.join("hofund.toml")
    }

    fn token_path(&self) -> PathBuf {
        self.root.join("hofund.token")
    }

    fn sentinel_root(&self) -> &Path {
        &self.root
    }
}

struct MoveModelFailSentinel(PathBuf);

impl MoveModelFailSentinel {
    fn install(root: &Path) -> Self {
        let path = root.join(FAIL_MOVE_MODEL_SENTINEL);
        std::fs::write(&path, b"").expect("fail-move-model sentinel");
        Self(path)
    }
}

impl Drop for MoveModelFailSentinel {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

struct MoveModelBlockSentinel(PathBuf);

impl MoveModelBlockSentinel {
    fn install(root: &Path) -> Self {
        let path = root.join(BLOCK_MOVE_MODEL_SENTINEL);
        std::fs::write(&path, b"").expect("block-move-model sentinel");
        Self(path)
    }
}

impl Drop for MoveModelBlockSentinel {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

async fn start(scratch: &Scratch, source: &str, session: Option<Session>) -> Running {
    std::fs::write(scratch.config_path(), source).expect("config");
    start_with_options(
        &scratch.config_path(),
        StartOptions {
            control_listen: Some("127.0.0.1:0".parse().expect("control")),
            control_runtime: Some(ControlRuntime::new(None, scratch.runtime_dir.clone())),
            agent_session: session,
            skip_config_watch: true,
            hofund_addr: Some(String::new()),
            ..StartOptions::default()
        },
    )
    .await
    .expect("daemon")
}

fn scope_gateway_key(scratch: &Scratch, models: &[&str]) {
    let store = KeyStore::beside_token(scratch.token_path());
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

fn agent_settings(scratch: &Scratch) -> PathBuf {
    scratch
        .runtime_dir
        .join("home")
        .join(".claude")
        .join("settings.json")
}

fn read_optional_file_bytes(path: &Path) -> Option<Vec<u8>> {
    std::fs::read(path).ok()
}

fn agent_tree_snapshot(main: &Path) -> BTreeMap<String, Option<Vec<u8>>> {
    let mut snapshot = BTreeMap::new();
    snapshot.insert("main".to_owned(), read_optional_file_bytes(main));
    for suffix in [
        ".hofund.bak",
        ".hofund.absent",
        ".hofund.wrote",
        ".hofund.credential",
    ] {
        let mut name = main.as_os_str().to_os_string();
        name.push(suffix);
        snapshot.insert(
            suffix.to_owned(),
            read_optional_file_bytes(&PathBuf::from(name)),
        );
    }
    snapshot
}

#[tokio::test]
async fn r7a_rr11_group_rename_moves_exact_key_scope_and_keeps_wildcard() {
    let scratch = Scratch::new("keys-success");
    let running = start(&scratch, &group_fixture(), None).await;
    scope_gateway_key(&scratch, &["GROUP/old", "group/*", "m"]);
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
    client.patch_group("old", &patch).await.expect("rename");
    let text = std::fs::read_to_string(scratch.config_path()).expect("config");
    assert!(text.contains("model = \"group/new\""), "{text}");
    let store = KeyStore::beside_token(scratch.token_path());
    let key = store.list().expect("keys")[0].clone();
    assert_eq!(key.models, ["group/new", "group/*", "m"]);
    support::shutdown(running).await;
}

#[tokio::test]
async fn r7a_rr12_move_model_save_failure_restores_config_agent_tree_and_key_scopes() {
    let scratch = Scratch::new("move-model-fail");
    let config_before = group_fixture();
    let settings = agent_settings(&scratch);
    std::fs::create_dir_all(settings.parent().expect("parent")).expect("dir");
    std::fs::write(&settings, "{\"model\": \"group/old\"}\n").expect("settings");
    let tree_before = agent_tree_snapshot(&settings);
    let session = Session::new(scratch.runtime_dir.join("home")).with_path("");
    let _fail = MoveModelFailSentinel::install(scratch.sentinel_root());
    let running = start(&scratch, &config_before, Some(session)).await;
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
    let error = client
        .patch_group("old", &patch)
        .await
        .expect_err("rename must fail at move_model save");
    match error {
        ClientError::Server { error, .. } => {
            assert_eq!(error.error.kind, ControlErrorKind::Internal);
            assert!(
                error.error.message.contains("move_model save failure"),
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
        std::fs::read_to_string(scratch.config_path()).expect("config"),
        config_before
    );
    let runtime = client.config().await.expect("runtime config after failure");
    assert_eq!(
        runtime.text, config_before,
        "runtime must publish rolled config after sidecar work failure, not staged rename text"
    );
    assert_eq!(
        std::fs::read_to_string(&settings).expect("settings"),
        "{\"model\": \"group/old\"}\n"
    );
    assert_eq!(agent_tree_snapshot(&settings), tree_before);
    let store = KeyStore::beside_token(scratch.token_path());
    let key = store.list().expect("keys")[0].clone();
    assert_eq!(key.models, ["GROUP/old", "group/*"]);
    support::shutdown(running).await;
}

#[tokio::test]
async fn r7a_rr12_concurrent_key_edit_survives_failed_group_rename() {
    let scratch = Scratch::new("concurrent-keys");
    let config_before = group_fixture();
    let settings = agent_settings(&scratch);
    std::fs::create_dir_all(settings.parent().expect("parent")).expect("dir");
    std::fs::write(&settings, "{\"model\": \"group/old\"}\n").expect("settings");
    let session = Session::new(scratch.runtime_dir.join("home")).with_path("");
    let block = MoveModelBlockSentinel::install(scratch.sentinel_root());
    let running = start(&scratch, &config_before, Some(session)).await;
    scope_gateway_key(&scratch, &["group/old"]);
    let token = std::fs::read_to_string(scratch.token_path())
        .expect("token")
        .trim()
        .to_owned();
    let client = HttpClient::tcp(
        running.control_local_addr().expect("control"),
        token.clone(),
    );
    let read = client.config().await.expect("config");
    let patch = group_patch(json!({
        "version": API_VERSION,
        "expected_revision": read.disk_revision,
        "rename_to": "new",
    }));
    let rename = tokio::spawn({
        let client = HttpClient::tcp(running.control_local_addr().expect("control"), token);
        async move { client.patch_group("old", &patch).await }
    });
    let deadline = std::time::Instant::now() + support::REQUEST;
    while std::time::Instant::now() < deadline {
        if std::fs::read_to_string(&settings)
            .ok()
            .is_some_and(|text| text.contains("group/new"))
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        std::fs::read_to_string(&settings)
            .expect("settings mid-rename")
            .contains("group/new"),
        "rename should reach agent rewrite before move_model runs"
    );
    let store = KeyStore::beside_token(scratch.token_path());
    let key_id = store.list().expect("keys")[0].id.clone();
    let mut models = store.list().expect("keys")[0].models.clone();
    models.push("chat/concurrent-marker".to_owned());
    store
        .set_models(&key_id, &models)
        .expect("concurrent key edit");
    let _fail = MoveModelFailSentinel::install(scratch.sentinel_root());
    drop(block);
    let error = support::within(
        "group rename after concurrent key edit",
        support::REQUEST,
        rename,
    )
    .await
    .expect("join")
    .expect_err("rename must fail");
    match error {
        ClientError::Server { .. } => {}
        other => panic!("expected server error, got {other}"),
    }
    assert_eq!(
        std::fs::read_to_string(scratch.config_path()).expect("config"),
        config_before
    );
    assert_eq!(
        std::fs::read_to_string(&settings).expect("settings"),
        "{\"model\": \"group/old\"}\n"
    );
    let key = store.list().expect("keys")[0].clone();
    assert!(
        key.models
            .iter()
            .any(|model| model == "chat/concurrent-marker"),
        "concurrent key edit must survive failed rename, got {:?}",
        key.models
    );
    assert!(
        key.models
            .iter()
            .any(|model| model.eq_ignore_ascii_case("group/old")),
        "group scope must stay on the old id, got {:?}",
        key.models
    );
    assert!(
        !key.models
            .iter()
            .any(|model| model.eq_ignore_ascii_case("group/new")),
        "move_model save failure must not persist new scope"
    );
    support::shutdown(running).await;
}

#[cfg(unix)]
#[tokio::test]
async fn r7a_rr12_agent_rename_failure_restores_config_disk_runtime_and_agent_tree() {
    use std::os::unix::fs::PermissionsExt;

    let scratch = Scratch::new("agent-rename-fail");
    let config_before = group_fixture();
    let settings = agent_settings(&scratch);
    std::fs::create_dir_all(settings.parent().expect("parent")).expect("dir");
    std::fs::write(&settings, "{\"model\": \"group/old\"}\n").expect("settings");
    let codex_config = scratch
        .runtime_dir
        .join("home")
        .join(".codex")
        .join("config.toml");
    std::fs::create_dir_all(codex_config.parent().expect("parent")).expect("dir");
    std::fs::write(
        &codex_config,
        "model = \"group/old\"\nmodel_provider = \"openai\"\n",
    )
    .expect("codex config");
    let session = Session::new(scratch.runtime_dir.join("home")).with_path("");
    let running = start(&scratch, &config_before, Some(session)).await;
    let token = std::fs::read_to_string(scratch.token_path())
        .expect("token")
        .trim()
        .to_owned();
    let client = HttpClient::tcp(running.control_local_addr().expect("control"), token);
    client
        .connect_agent("claude-code", AgentConnectRequest::new(None))
        .await
        .expect("connect claude");
    let tree_before = agent_tree_snapshot(&settings);
    std::fs::set_permissions(&codex_config, std::fs::Permissions::from_mode(0o000))
        .expect("codex inaccessible");
    let read = client.config().await.expect("config");
    let patch = group_patch(json!({
        "version": API_VERSION,
        "expected_revision": read.disk_revision,
        "rename_to": "new",
    }));
    let error = client
        .patch_group("old", &patch)
        .await
        .expect_err("rename must fail during agent rewrite");
    match error {
        ClientError::Server { .. } => {}
        other => panic!("expected server error, got {other}"),
    }
    assert_eq!(
        std::fs::read_to_string(scratch.config_path()).expect("config"),
        config_before
    );
    let runtime = client.config().await.expect("runtime config after failure");
    assert_eq!(
        runtime.text, config_before,
        "runtime must match rolled disk after agent rename failure"
    );
    assert_eq!(agent_tree_snapshot(&settings), tree_before);
    support::shutdown(running).await;
}
