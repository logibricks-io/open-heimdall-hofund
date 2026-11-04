//! Provider id rename through the control plane.

mod support;

use std::path::{Path, PathBuf};

use hofund_control_client::Client as HttpClient;
use hofund_control_client::ClientError;
use hofund_control_proto::{
    API_VERSION, ControlErrorKind, EventEnvelope, EventPayload, PicksResponse, decode_response,
};
use hofund_daemon::{
    ControlRuntime, ProviderRenameFailAfter, Running, StartOptions, set_rename_fail_after,
    start_with_options,
};
use hofund_secrets::KeyStore;
use std::sync::LazyLock;

use tokio::sync::Mutex as AsyncMutex;

static RENAME_TEST_LOCK: LazyLock<AsyncMutex<()>> = LazyLock::new(|| AsyncMutex::new(()));
use serde_json::json;
use sha2::{Digest, Sha256};

fn config(extra: &str) -> String {
    format!(
        "listen = \"127.0.0.1:0\"\n[[providers]]\nid = \"chat\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key = \"test-key\"\n[providers.models]\nrefresh = \"off\"\n[[routes]]\nmodel = \"m\"\nupstreams = [\"chat\"]\n{extra}"
    )
}

struct Scratch(PathBuf, #[allow(dead_code)] tempfile::TempDir);

impl Scratch {
    fn new(label: &str) -> Self {
        let guard = tempfile::Builder::new()
            .prefix(&format!("hofund-rename-{label}-"))
            .tempdir()
            .expect("scratch");
        Self(guard.path().to_path_buf(), guard)
    }

    fn path(&self) -> PathBuf {
        self.0.join("hofund.toml")
    }
}

async fn start(dir: &Path, source: &str) -> Running {
    let config = dir.join("hofund.toml");
    std::fs::write(&config, source).expect("config");
    start_with_options(
        &config,
        StartOptions {
            control_listen: Some("127.0.0.1:0".parse().expect("control")),
            control_runtime: Some(ControlRuntime::new(None, dir.to_path_buf())),
            ..StartOptions::default()
        },
    )
    .await
    .expect("daemon")
}

fn state_path(dir: &Path, provider: &str) -> PathBuf {
    let slug = provider
        .to_lowercase()
        .chars()
        .map(|character| {
            if character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-' {
                character
            } else {
                '-'
            }
        })
        .take(40)
        .collect::<String>();
    let hash = format!("{:x}", Sha256::digest(provider.as_bytes()));
    dir.join(".hofund/host/models")
        .join(format!("{slug}-{}.json", &hash[..16]))
}

fn seed_state(dir: &Path, provider: &str, picks: &[&str]) {
    let list = ["a", "b"]
        .into_iter()
        .map(|id| json!({"id": id}))
        .collect::<Vec<_>>();
    let state = json!({
        "version": 1,
        "id": provider,
        "fetched_at": 1,
        "list": list,
        "picks": picks,
    });
    let path = state_path(dir, provider);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("state dir");
    std::fs::write(path, serde_json::to_vec_pretty(&state).expect("json")).expect("state");
}

fn scope_gateway_key(dir: &Path, models: &[&str], accounts: &[&str]) {
    let store = KeyStore::beside_token(dir.join("hofund.token"));
    let mut keys = store.export().expect("export");
    keys[0].models = models.iter().map(|entry| (*entry).to_owned()).collect();
    keys[0].accounts = accounts.iter().map(|entry| (*entry).to_owned()).collect();
    store.restore(&keys).expect("seed scopes");
}

struct Control {
    client: HttpClient,
}

impl Control {
    fn new(dir: &Path, running: &Running) -> Self {
        let token = std::fs::read_to_string(dir.join("hofund.token"))
            .expect("token")
            .trim()
            .to_owned();
        Self {
            client: HttpClient::tcp(running.control_local_addr().expect("control"), token),
        }
    }

    async fn picks(&self, provider: &str) -> PicksResponse {
        self.client.picks(provider).await.expect("picks")
    }

    async fn rename(&self, from: &str, to: &str) -> Result<(), ClientError> {
        self.client.rename_provider(from, to).await.map(|_| ())
    }
}

#[tokio::test]
async fn g3_8_picks_follow_provider_rename() {
    let _guard = RENAME_TEST_LOCK.lock().await;
    set_rename_fail_after(None);
    let scratch = Scratch::new("picks");
    seed_state(&scratch.0, "chat", &["a", "b"]);
    let running = start(&scratch.0, &config("")).await;
    let control = Control::new(&scratch.0, &running);

    assert_eq!(control.picks("chat").await.picks, ["a", "b"]);
    control
        .client
        .rename_provider("chat", "team")
        .await
        .expect("rename");
    assert_eq!(control.picks("team").await.picks, ["a", "b"]);
    assert!(!state_path(&scratch.0, "chat").exists());
    assert!(state_path(&scratch.0, "team").exists());
    support::shutdown(running).await;
}

#[tokio::test]
async fn g3_9_rename_failure_restores_config_state_and_keys() {
    let _guard = RENAME_TEST_LOCK.lock().await;
    set_rename_fail_after(None);
    let scratch = Scratch::new("rollback");
    seed_state(&scratch.0, "chat", &["keep"]);
    let running = start(&scratch.0, &config("")).await;
    scope_gateway_key(&scratch.0, &["chat/m", "other/x"], &["chat/account"]);
    let control = Control::new(&scratch.0, &running);
    let config_before = std::fs::read_to_string(scratch.path()).expect("config");

    set_rename_fail_after(Some(ProviderRenameFailAfter::Keys));
    let error = control.rename("chat", "team").await.expect_err("fail");
    set_rename_fail_after(None);
    match error {
        ClientError::Server { error, .. } => {
            assert_eq!(error.error.kind, ControlErrorKind::Internal);
            assert!(
                error.error.message.contains("keys failure"),
                "{}",
                error.error.message
            );
        }
        other => panic!("expected server error, got {other}"),
    }

    assert_eq!(
        std::fs::read_to_string(scratch.path()).expect("config"),
        config_before
    );
    assert!(state_path(&scratch.0, "chat").exists());
    assert!(!state_path(&scratch.0, "team").exists());
    let store = KeyStore::beside_token(scratch.0.join("hofund.token"));
    let key = store.list().expect("keys")[0].clone();
    assert_eq!(key.models, ["chat/m", "other/x"]);
    assert_eq!(key.accounts, ["chat/account"]);
    support::shutdown(running).await;
}

#[tokio::test]
async fn g3_10_rename_success_moves_config_state_and_key_scopes() {
    let _guard = RENAME_TEST_LOCK.lock().await;
    set_rename_fail_after(None);
    let scratch = Scratch::new("success");
    seed_state(&scratch.0, "chat", &["picked"]);
    let running = start(&scratch.0, &config("")).await;
    scope_gateway_key(&scratch.0, &["chat/m"], &["chat/account"]);
    let control = Control::new(&scratch.0, &running);

    control
        .client
        .rename_provider("chat", "team")
        .await
        .expect("rename");

    let text = std::fs::read_to_string(scratch.path()).expect("config");
    assert!(text.contains("id = \"team\""), "{text}");
    assert!(text.contains("upstreams = [\"team\"]"), "{text}");
    assert!(!state_path(&scratch.0, "chat").exists());
    assert!(state_path(&scratch.0, "team").exists());
    assert_eq!(control.picks("team").await.picks, ["picked"]);
    let store = KeyStore::beside_token(scratch.0.join("hofund.token"));
    let key = store.list().expect("keys")[0].clone();
    assert_eq!(key.models, ["team/m"]);
    assert_eq!(key.accounts, ["team/account"]);
    support::shutdown(running).await;
}

#[tokio::test]
async fn g3_11_rename_republishes_live_configuration() {
    let _guard = RENAME_TEST_LOCK.lock().await;
    set_rename_fail_after(None);
    let scratch = Scratch::new("reload");
    let running = start(&scratch.0, &config("")).await;
    let http = support::client();
    let token = std::fs::read_to_string(scratch.0.join("hofund.token"))
        .expect("token")
        .trim()
        .to_owned();
    let base = format!("http://{}", running.control_local_addr().expect("control"));
    let mut events = http
        .get(format!("{base}/v1/hofund/events"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("events");
    assert_eq!(events.status(), reqwest::StatusCode::OK);

    let before = HttpClient::tcp(
        running.control_local_addr().expect("control"),
        token.clone(),
    )
    .status()
    .await
    .expect("status");
    let client = HttpClient::tcp(running.control_local_addr().expect("control"), token);
    let response = client
        .rename_provider("chat", "team")
        .await
        .expect("rename");

    let after = client.status().await.expect("status");
    assert_eq!(after.generation, before.generation + 1);
    assert_eq!(after.disk_revision, response.disk_revision);
    assert_eq!(after.runtime_revision, response.disk_revision);

    let listed = client.providers().await.expect("providers");
    assert!(listed.providers.iter().any(|entry| entry.id == "team"));
    assert!(!listed.providers.iter().any(|entry| entry.id == "chat"));

    let mut buffer = String::new();
    let mut saw_reload = false;
    let deadline = std::time::Instant::now() + support::REQUEST;
    while !saw_reload && std::time::Instant::now() < deadline {
        while let Some(end) = buffer.find("\n\n") {
            let block: String = buffer.drain(..end + 2).collect();
            let mut name = String::new();
            let mut data = String::new();
            for line in block.lines() {
                if let Some(value) = line.strip_prefix("event: ") {
                    name = value.to_owned();
                } else if let Some(value) = line.strip_prefix("data: ") {
                    data.push_str(value);
                }
            }
            if name.is_empty() || name == "snapshot" {
                continue;
            }
            if name == "config_reloaded" {
                let envelope =
                    decode_response::<EventEnvelope>(API_VERSION, data.as_bytes()).expect("event");
                match envelope.payload {
                    EventPayload::ConfigReloaded(event) => {
                        assert_eq!(event.generation, after.generation);
                        assert_eq!(event.disk_revision, response.disk_revision);
                        saw_reload = true;
                    }
                    other => panic!("expected config_reloaded, got {other:?}"),
                }
            }
        }
        if saw_reload {
            break;
        }
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(10), events.chunk())
            .await
            .expect("event stream progresses")
            .expect("chunk")
            .expect("stream open");
        buffer.push_str(std::str::from_utf8(&chunk).expect("utf-8"));
    }
    assert!(
        saw_reload,
        "rename should emit config_reloaded like other provider edits"
    );
    support::shutdown(running).await;
}
