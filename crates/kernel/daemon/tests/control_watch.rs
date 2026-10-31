//! L6c: the configuration watcher inside a running daemon.

mod support;

use std::path::PathBuf;
use std::time::Duration;

use hofund_agent::Session;
use hofund_control_proto::{
    API_VERSION, ConfigApplyRequest, ConfigResponse, EventEnvelope, EventPayload, StatusResponse,
    decode_response,
};
use hofund_daemon::{ControlRuntime, Running, StartOptions, start_with_options};
use sha2::{Digest, Sha256};

const SECRET: &str = "sk-control-watch-provider-secret";

fn config(extra: &str) -> String {
    format!(
        "listen = \"127.0.0.1:0\"\n{extra}\
[[providers]]\nid = \"chat\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key = \"{SECRET}\"\n\
[[routes]]\nmodel = \"m\"\nupstreams = [\"chat\"]\n"
    )
}

fn revision(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

struct Daemon {
    running: Running,
    http: reqwest::Client,
    base: String,
    token: String,
    path: PathBuf,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

impl Daemon {
    async fn start(text: &str) -> Self {
        Self::start_cased(text, "hofund.toml").await
    }

    async fn start_cased(text: &str, start_name: &str) -> Self {
        let config_dir = tempfile::tempdir().expect("config dir");
        let runtime_dir = tempfile::tempdir().expect("runtime dir");
        std::fs::create_dir_all(runtime_dir.path().join("home")).expect("home");
        let path = config_dir.path().join("hofund.toml");
        std::fs::write(&path, text).expect("config");
        let start_path = config_dir.path().join(start_name);
        let running = start_with_options(
            &start_path,
            StartOptions {
                control_listen: Some("127.0.0.1:0".parse().expect("control address")),
                control_runtime: Some(ControlRuntime::new(None, runtime_dir.path().to_path_buf())),
                agent_session: Some(Session::new(runtime_dir.path().join("home")).with_path("")),
                ..StartOptions::default()
            },
        )
        .await
        .expect("daemon");
        let token = std::fs::read_to_string(runtime_dir.path().join("hofund.token"))
            .or_else(|_| std::fs::read_to_string(config_dir.path().join("hofund.token")))
            .expect("token")
            .trim()
            .to_owned();
        Self {
            base: format!("http://{}", running.control_local_addr().expect("control")),
            running,
            http: support::client(),
            token,
            path,
            _dirs: (config_dir, runtime_dir),
        }
    }

    async fn get<T: hofund_control_proto::Response>(&self, route: &str) -> T {
        let response = self
            .http
            .get(format!("{}{route}", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .expect("send");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        decode_response(API_VERSION, &response.bytes().await.expect("body")).expect("DTO")
    }

    async fn config(&self) -> ConfigResponse {
        self.get("/v1/hofund/config").await
    }

    async fn status(&self) -> StatusResponse {
        self.get("/v1/hofund/status").await
    }

    async fn events(&self) -> reqwest::Response {
        let response = self
            .http
            .get(format!("{}/v1/hofund/events", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .expect("events");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        response
    }
}

/// Reads the next named event within `limit`, skipping the snapshot.
async fn next_event(
    response: &mut reqwest::Response,
    buffer: &mut String,
    limit: Duration,
) -> Option<(String, EventPayload)> {
    let until = tokio::time::Instant::now() + limit;
    loop {
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
            assert!(!data.contains(SECRET), "{data}");
            if name.is_empty() || name == "snapshot" {
                continue;
            }
            let envelope = decode_response::<EventEnvelope>(API_VERSION, data.as_bytes())
                .expect("event envelope");
            return Some((name, envelope.payload));
        }
        let chunk = tokio::time::timeout_at(until, response.chunk())
            .await
            .ok()?;
        let chunk = chunk.expect("chunk").expect("stream open");
        buffer.push_str(std::str::from_utf8(&chunk).expect("utf-8"));
    }
}

#[tokio::test]
async fn watcher_invalid_edit_keeps_last_good_and_emits_config_rejected() {
    let original = config("");
    let daemon = Daemon::start(&original).await;
    let mut stream = daemon.events().await;
    let mut buffer = String::new();

    let invalid = format!("{original}[[routes]]\nmodel = \"x\"\nupstreams = [\"missing\"]\n");
    std::fs::write(&daemon.path, &invalid).expect("invalid edit");
    match next_event(&mut stream, &mut buffer, Duration::from_secs(3)).await {
        Some((name, EventPayload::ConfigRejected(event))) => {
            assert_eq!(name, "config_rejected");
            assert_eq!(event.revision, revision(&invalid));
        }
        other => panic!("expected config_rejected, got {other:?}"),
    }
    let status = daemon.status().await;
    assert_eq!(status.generation, 1);
    assert_eq!(status.runtime_revision, revision(&original));
    assert_eq!(status.disk_revision, revision(&invalid));
    assert_eq!(
        std::fs::read_to_string(&daemon.path).expect("read"),
        invalid
    );

    // Fixing the edit by hand publishes it without an explicit reload.
    let fixed = config("# fixed by hand\n");
    std::fs::write(&daemon.path, &fixed).expect("fixed edit");
    match next_event(&mut stream, &mut buffer, Duration::from_secs(3)).await {
        Some((_, EventPayload::ConfigReloaded(event))) => {
            assert_eq!(event.generation, 2);
            assert_eq!(event.runtime_revision, revision(&fixed));
        }
        other => panic!("expected config_reloaded, got {other:?}"),
    }
    assert_eq!(daemon.config().await.generation, 2);
    drop(stream);
    support::shutdown(daemon.running).await;
}

#[tokio::test]
async fn watcher_skips_daemon_write_when_revisions_match() {
    let original = config("");
    let daemon = Daemon::start(&original).await;
    let mut stream = daemon.events().await;
    let mut buffer = String::new();

    let candidate = config("# applied over control\n");
    let response = daemon
        .http
        .post(format!("{}/v1/hofund/config/apply", daemon.base))
        .bearer_auth(&daemon.token)
        .json(&ConfigApplyRequest::new(revision(&original), &candidate))
        .send()
        .await
        .expect("apply");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let applied: ConfigResponse =
        decode_response(API_VERSION, &response.bytes().await.expect("body")).expect("DTO");
    assert_eq!(applied.generation, 2);
    match next_event(&mut stream, &mut buffer, Duration::from_secs(3)).await {
        Some((_, EventPayload::ConfigReloaded(event))) => assert_eq!(event.generation, 2),
        other => panic!("expected config_reloaded, got {other:?}"),
    }
    // L6f: successful publish also emits snapshot_published on the control topic.
    match next_event(&mut stream, &mut buffer, Duration::from_secs(3)).await {
        Some((_, EventPayload::SnapshotPublished(_))) => {}
        other => panic!("expected snapshot_published, got {other:?}"),
    }
    // The watcher sees the daemon's own write after the debounce and skips it.
    let quiet = next_event(&mut stream, &mut buffer, Duration::from_millis(1200)).await;
    assert!(
        quiet.is_none(),
        "watcher republished the daemon write: {quiet:?}"
    );
    let status = daemon.status().await;
    assert_eq!(status.generation, 2);
    assert_eq!(status.runtime_revision, revision(&candidate));
    drop(stream);
    support::shutdown(daemon.running).await;
}

#[tokio::test]
async fn watcher_matches_on_disk_name_after_differently_cased_edit() {
    let probe_dir = tempfile::tempdir().expect("case probe dir");
    let probe = probe_dir.path().join("case-probe");
    std::fs::write(&probe, "probe").expect("case probe");
    if !probe_dir.path().join("CASE-PROBE").exists() {
        return;
    }
    drop(probe_dir);

    let original = config("");
    let daemon = Daemon::start_cased(&original, "HOFUND.TOML").await;
    let mut stream = daemon.events().await;
    let mut buffer = String::new();
    let edited = config("# edited through another case\n");
    std::fs::write(daemon.path.with_file_name("HoFuNd.ToMl"), &edited).expect("hand edit");

    match next_event(&mut stream, &mut buffer, Duration::from_secs(3)).await {
        Some((name, EventPayload::ConfigReloaded(event))) => {
            assert_eq!(name, "config_reloaded");
            assert_eq!(event.generation, 2);
            assert_eq!(event.runtime_revision, revision(&edited));
        }
        other => panic!("expected config_reloaded, got {other:?}"),
    }
    support::shutdown(daemon.running).await;
}
