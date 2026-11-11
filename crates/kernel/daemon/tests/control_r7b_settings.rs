//! G-5 settings routes against a running daemon.

mod support;

use hofund_control_proto::{
    API_VERSION, ControlErrorKind, SettingsPatchRequest, SettingsResponse, StatusResponse,
    decode_error_envelope, decode_response,
};
use hofund_daemon::{ControlRuntime, Running, StartOptions, start_with_options};
use serde_json::json;

struct Scratch {
    config: std::path::PathBuf,
    runtime: std::path::PathBuf,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

impl Scratch {
    fn new(label: &str) -> Self {
        let config = tempfile::Builder::new()
            .prefix(&format!("r7b-settings-{label}-config-"))
            .tempdir()
            .expect("config dir");
        let runtime = tempfile::Builder::new()
            .prefix(&format!("r7b-settings-{label}-runtime-"))
            .tempdir()
            .expect("runtime dir");
        std::fs::create_dir_all(runtime.path().join("home")).expect("runtime home");
        let path = config.path().join("hofund.toml");
        std::fs::write(&path, "listen = \"127.0.0.1:0\"\n").expect("config");
        Self {
            config: path,
            runtime: runtime.path().to_path_buf(),
            _dirs: (config, runtime),
        }
    }
}

async fn start(scratch: &Scratch) -> Running {
    start_with_options(
        &scratch.config,
        StartOptions {
            control_listen: Some("127.0.0.1:0".parse().expect("control")),
            control_enabled: true,
            control_runtime: Some(ControlRuntime::new(None, scratch.runtime.clone())),
            skip_config_watch: true,
            hofund_addr: Some(String::new()),
            ..StartOptions::default()
        },
    )
    .await
    .expect("daemon")
}

struct Client {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl Client {
    fn new(running: &Running, scratch: &Scratch) -> Self {
        let token = std::fs::read_to_string(
            scratch
                .config
                .parent()
                .expect("parent")
                .join("hofund.token"),
        )
        .or_else(|_| std::fs::read_to_string(scratch.runtime.join("hofund.token")))
        .expect("token")
        .trim()
        .to_owned();
        Self {
            http: support::client(),
            base: format!("http://{}", running.control_local_addr().expect("control")),
            token,
        }
    }

    async fn get<T: hofund_control_proto::Response>(&self, path: &str) -> T {
        let response = self
            .http
            .get(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .expect("GET");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        decode_response(API_VERSION, &response.bytes().await.expect("body")).expect("DTO")
    }

    async fn settings(&self) -> SettingsResponse {
        self.get("/v1/hofund/settings").await
    }

    async fn patch(&self, request: &SettingsPatchRequest) -> (reqwest::StatusCode, Vec<u8>) {
        let response = self
            .http
            .patch(format!("{}/v1/hofund/settings", self.base))
            .bearer_auth(&self.token)
            .json(request)
            .send()
            .await
            .expect("PATCH");
        let status = response.status();
        (status, response.bytes().await.expect("body").to_vec())
    }
}

fn patch(revision: &str) -> SettingsPatchRequest {
    SettingsPatchRequest::new(revision)
}

#[tokio::test]
async fn settings_get_returns_defaults() {
    let scratch = Scratch::new("defaults");
    let running = start(&scratch).await;
    let client = Client::new(&running, &scratch);
    let settings = client.settings().await;
    assert_eq!(settings.listen, "127.0.0.1:0");
    assert_eq!(settings.currency, "usd");
    assert!(!settings.quota_left);
    assert!(!settings.discover_accounts);
    assert!(settings.lan_allow.is_empty());
    assert!(!settings.pending_restart);
    assert_eq!(settings.disk_revision.len(), 64);
    support::shutdown(running).await;
}

#[tokio::test]
async fn settings_patch_checks_revision_and_validates_currency() {
    let scratch = Scratch::new("revision");
    let running = start(&scratch).await;
    let client = Client::new(&running, &scratch);
    let before = client.settings().await;
    let mut update = patch(&before.disk_revision);
    update.currency = Some("cny".to_owned());
    update.quota_left = Some(true);
    update.discover_accounts = Some(true);
    update.lan_allow = Some(vec!["192.168.1.0/24".to_owned(), "localhost".to_owned()]);
    let (status, body) = client.patch(&update).await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let updated: SettingsResponse = decode_response(API_VERSION, &body).expect("updated");
    assert_ne!(updated.disk_revision, before.disk_revision);
    assert_eq!(updated.currency, "cny");
    assert!(updated.quota_left);
    assert!(updated.discover_accounts);
    assert_eq!(updated.lan_allow, update.lan_allow.expect("list"));

    let mut stale = patch(&before.disk_revision);
    stale.currency = Some("usd".to_owned());
    let (status, body) = client.patch(&stale).await;
    assert_eq!(status, reqwest::StatusCode::CONFLICT);
    assert_eq!(
        decode_error_envelope(API_VERSION, &body)
            .expect("error")
            .error
            .kind,
        ControlErrorKind::Conflict
    );
    assert_eq!(client.settings().await.disk_revision, updated.disk_revision);
    assert_eq!(client.settings().await.currency, "cny");

    let mut invalid = patch(&updated.disk_revision);
    invalid.currency = Some("eur".to_owned());
    let (status, body) = client.patch(&invalid).await;
    assert_eq!(status, reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        decode_error_envelope(API_VERSION, &body)
            .expect("error")
            .error
            .kind,
        ControlErrorKind::Unprocessable
    );
    assert_eq!(client.settings().await.disk_revision, updated.disk_revision);
    support::shutdown(running).await;
}

#[tokio::test]
async fn listen_change_is_restart_pending_without_rebinding_and_lan_is_not_enforced() {
    let scratch = Scratch::new("listen");
    let running = start(&scratch).await;
    let bound = running.local_addr;
    let client = Client::new(&running, &scratch);
    let before = client.settings().await;
    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("free port");
    let new_listen = probe.local_addr().expect("new listen");
    drop(probe);

    let mut update = patch(&before.disk_revision);
    update.listen = Some(new_listen.to_string());
    update.lan_allow = Some(vec!["203.0.113.0/24".to_owned()]);
    let (status, body) = client.patch(&update).await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let updated: SettingsResponse = decode_response(API_VERSION, &body).expect("updated");
    assert!(updated.pending_restart);
    assert_eq!(updated.listen, new_listen.to_string());
    assert_eq!(running.local_addr, bound);
    assert!(
        std::net::TcpListener::bind(new_listen).is_ok(),
        "the new port was rebound"
    );

    let status: StatusResponse = client.get("/v1/hofund/status").await;
    assert!(status.pending_restart.iter().any(|field| field == "listen"));
    assert_eq!(
        status.model_listener.running.as_deref(),
        Some(bound.to_string().as_str())
    );
    let health = client
        .http
        .get(format!("http://{bound}/health"))
        .bearer_auth(&client.token)
        .send()
        .await
        .expect("model health after LAN setting");
    assert_eq!(
        health.status(),
        reqwest::StatusCode::OK,
        "LAN allow-list was enforced early"
    );
    support::shutdown(running).await;
}

#[tokio::test]
async fn settings_patch_rejects_unknown_fields() {
    let scratch = Scratch::new("unknown");
    let running = start(&scratch).await;
    let client = Client::new(&running, &scratch);
    let current = client.settings().await;
    let response = client
        .http
        .patch(format!("{}/v1/hofund/settings", client.base))
        .bearer_auth(&client.token)
        .json(&json!({
            "version": 1,
            "expected_revision": current.disk_revision,
            "unexpected": true
        }))
        .send()
        .await
        .expect("PATCH");
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    support::shutdown(running).await;
}
