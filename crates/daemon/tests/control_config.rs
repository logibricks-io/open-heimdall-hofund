//! L6b configuration transaction over the control listener.

use std::path::{Path, PathBuf};
use std::time::Duration;

use hofund_agent::Session;
use hofund_control_proto::{
    API_VERSION, ConfigApplyRequest, ConfigResponse, ConfigValidateRequest,
    ConfigValidationResponse, ControlErrorKind, EventEnvelope, EventPayload, ReloadRequest,
    StatusResponse, decode_error_envelope, decode_response,
};
use hofund_daemon::{ControlRuntime, Running, StartOptions, start_with_options};
use sha2::{Digest, Sha256};

const SECRET: &str = "sk-control-config-provider-secret";

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

struct Scratch {
    /// Holds the configuration file.
    config_dir: PathBuf,
    /// Holds the control runtime files and the agent home.
    runtime_dir: PathBuf,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

impl Scratch {
    fn new(label: &str) -> Self {
        let make = |kind: &str| {
            tempfile::Builder::new()
                .prefix(&format!("hofund-daemon-config-{label}-{kind}-"))
                .tempdir()
                .expect("scratch dir")
        };
        let config = make("config");
        let runtime = make("runtime");
        std::fs::create_dir_all(runtime.path().join("home")).expect("scratch home");
        Self {
            config_dir: config.path().to_path_buf(),
            runtime_dir: runtime.path().to_path_buf(),
            _dirs: (config, runtime),
        }
    }

    fn path(&self) -> PathBuf {
        self.config_dir.join("hofund.toml")
    }
}

async fn start_at(scratch: &Scratch, config_path: &Path, text: &str) -> Running {
    std::fs::write(config_path, text).expect("config");
    start_with_options(
        config_path,
        StartOptions {
            control_listen: Some("127.0.0.1:0".parse().expect("control address")),
            control_runtime: Some(ControlRuntime::new(None, scratch.runtime_dir.clone())),
            agent_session: Some(Session::new(scratch.runtime_dir.join("home")).with_path("")),
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
        let token = std::fs::read_to_string(scratch.runtime_dir.join("hofund.token"))
            .or_else(|_| std::fs::read_to_string(scratch.config_dir.join("hofund.token")))
            .expect("token")
            .trim()
            .to_owned();
        Self {
            http: reqwest::Client::new(),
            base: format!("http://{}", running.control_local_addr().expect("control")),
            token,
        }
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> (reqwest::StatusCode, Vec<u8>) {
        let response = request.bearer_auth(&self.token).send().await.expect("send");
        let status = response.status();
        (status, response.bytes().await.expect("body").to_vec())
    }

    async fn get(&self, path: &str) -> (reqwest::StatusCode, Vec<u8>) {
        self.send(self.http.get(format!("{}{path}", self.base)))
            .await
    }

    async fn post<T: serde::Serialize>(
        &self,
        path: &str,
        body: &T,
    ) -> (reqwest::StatusCode, Vec<u8>) {
        self.send(self.http.post(format!("{}{path}", self.base)).json(body))
            .await
    }

    async fn config(&self) -> ConfigResponse {
        let (status, body) = self.get("/v1/hofund/config").await;
        assert_eq!(status, reqwest::StatusCode::OK);
        decode_response(API_VERSION, &body).expect("config DTO")
    }

    async fn status(&self) -> StatusResponse {
        let (status, body) = self.get("/v1/hofund/status").await;
        assert_eq!(status, reqwest::StatusCode::OK);
        decode_response(API_VERSION, &body).expect("status DTO")
    }

    async fn apply(&self, expected: &str, text: &str) -> (reqwest::StatusCode, Vec<u8>) {
        self.post(
            "/v1/hofund/config/apply",
            &ConfigApplyRequest::new(expected, text),
        )
        .await
    }

    async fn reload(&self, expected: &str) -> (reqwest::StatusCode, Vec<u8>) {
        self.post("/v1/hofund/config/reload", &ReloadRequest::new(expected))
            .await
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

fn ok(status: reqwest::StatusCode, body: &[u8]) -> ConfigResponse {
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(body)
    );
    decode_response(API_VERSION, body).expect("config DTO")
}

fn failure(status: reqwest::StatusCode, body: &[u8], expected: u16) -> ControlErrorKind {
    assert_eq!(
        status.as_u16(),
        expected,
        "{}",
        String::from_utf8_lossy(body)
    );
    decode_error_envelope(API_VERSION, body)
        .expect("error envelope")
        .error
        .kind
}

/// Reads SSE events until `count` named events arrive, skipping the snapshot.
async fn next_events(
    response: &mut reqwest::Response,
    count: usize,
) -> Vec<(String, EventPayload)> {
    let mut buffer = String::new();
    let mut events = Vec::new();
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
            events.push((name, envelope.payload));
            if events.len() == count {
                return events;
            }
        }
        let chunk = tokio::time::timeout(Duration::from_secs(10), response.chunk())
            .await
            .expect("event stream progresses")
            .expect("chunk")
            .expect("stream open");
        buffer.push_str(std::str::from_utf8(&chunk).expect("utf-8"));
    }
}

#[tokio::test]
async fn apply_writes_exact_bytes_publishes_and_reports_one_generation_everywhere() {
    let scratch = Scratch::new("apply");
    let original = config("");
    let running = start_at(&scratch, &scratch.path(), &original).await;
    let client = Client::new(&running, &scratch);
    let mut stream = client.events().await;

    let read = client.config().await;
    assert_eq!(read.text, original);
    assert_eq!(read.generation, 1);
    assert_eq!(read.disk_revision, revision(&original));
    assert_eq!(read.runtime_revision, read.disk_revision);

    // No trailing newline and CRLF line endings must survive byte for byte.
    let candidate = original.replace("\n[[routes]]", "\r\n# comment kept\r\n[[routes]]") + "# end";
    let (status, body) = client.apply(&read.disk_revision, &candidate).await;
    let applied = ok(status, &body);
    assert_eq!(applied.generation, 2);
    assert_eq!(applied.text, candidate);
    assert_eq!(applied.disk_revision, revision(&candidate));
    assert_eq!(applied.runtime_revision, applied.disk_revision);
    assert_eq!(
        std::fs::read(scratch.path()).expect("read"),
        candidate.as_bytes()
    );

    let status = client.status().await;
    assert_eq!(status.generation, 2);
    assert_eq!(status.disk_revision, applied.disk_revision);
    assert_eq!(status.runtime_revision, applied.runtime_revision);

    let events = next_events(&mut stream, 1).await;
    match events.as_slice() {
        [(name, EventPayload::ConfigReloaded(event))] => {
            assert_eq!(name, "config_reloaded");
            assert_eq!(event.generation, 2);
            assert_eq!(event.runtime_revision, applied.runtime_revision);
            assert_eq!(event.disk_revision, applied.disk_revision);
        }
        other => panic!("expected config_reloaded, got {other:?}"),
    }

    // An explicit reload at an equal revision still republishes.
    let (status, body) = client.reload(&applied.disk_revision).await;
    assert_eq!(ok(status, &body).generation, 3);
    // An open stream would hold the graceful drain of the control listener.
    drop(stream);
    running.shutdown().await;
}

#[tokio::test]
async fn apply_stale_disk_revision_returns_409_without_overwriting_hand_edit() {
    let scratch = Scratch::new("stale");
    let original = config("");
    let running = start_at(&scratch, &scratch.path(), &original).await;
    let client = Client::new(&running, &scratch);
    let hand_edit = format!("{original}# edited by hand\n");
    std::fs::write(scratch.path(), &hand_edit).expect("hand edit");

    let (status, body) = client
        .apply(&revision(&original), &config("# from the client\n"))
        .await;
    assert_eq!(failure(status, &body, 409), ControlErrorKind::Conflict);
    assert_eq!(
        std::fs::read_to_string(scratch.path()).expect("read"),
        hand_edit
    );
    let (status, body) = client.reload(&revision(&original)).await;
    assert_eq!(failure(status, &body, 409), ControlErrorKind::Conflict);

    let status = client.status().await;
    assert_eq!(status.generation, 1);
    assert_eq!(status.disk_revision, revision(&hand_edit));
    assert_eq!(status.runtime_revision, revision(&original));
    running.shutdown().await;
}

#[tokio::test]
async fn apply_bad_toml_is_422_rejected_and_never_echoes_config_text() {
    let scratch = Scratch::new("invalid");
    let original = config("");
    let running = start_at(&scratch, &scratch.path(), &original).await;
    let client = Client::new(&running, &scratch);
    let mut stream = client.events().await;

    let candidate = format!("{original}[[providers]\napi_key = \"{SECRET}-again\"\n");
    let (status, body) = client.apply(&revision(&original), &candidate).await;
    assert_eq!(failure(status, &body, 422), ControlErrorKind::InvalidConfig);
    assert!(!String::from_utf8_lossy(&body).contains(SECRET));
    assert_eq!(
        std::fs::read_to_string(scratch.path()).expect("read"),
        original
    );

    let (status, body) = client
        .post(
            "/v1/hofund/config/validate",
            &ConfigValidateRequest::new(candidate.as_str()),
        )
        .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(!String::from_utf8_lossy(&body).contains(SECRET));
    let validation: ConfigValidationResponse = decode_response(API_VERSION, &body).expect("DTO");
    assert!(!validation.valid);
    assert!(!validation.issues.is_empty());

    let events = next_events(&mut stream, 1).await;
    match events.as_slice() {
        [(name, EventPayload::ConfigRejected(event))] => {
            assert_eq!(name, "config_rejected");
            assert_eq!(event.revision, revision(&candidate));
        }
        other => panic!("expected config_rejected, got {other:?}"),
    }
    let status = client.status().await;
    assert_eq!(status.generation, 1);
    let rendered = format!("{status:?}");
    assert!(!rendered.contains(SECRET), "{rendered}");
    // An open stream would hold the graceful drain of the control listener.
    drop(stream);
    running.shutdown().await;
}

#[tokio::test]
async fn pending_restart_fields_agree_across_response_status_and_events() {
    let scratch = Scratch::new("pending");
    let original = config("");
    let running = start_at(&scratch, &scratch.path(), &original).await;
    let client = Client::new(&running, &scratch);
    let mut stream = client.events().await;

    let edited = config("token_file = \"other.token\"\nusage_file = \"other.db\"\n").replacen(
        "listen = \"127.0.0.1:0\"",
        "listen = \"127.0.0.1:1\"",
        1,
    );
    let (status, body) = client
        .post(
            "/v1/hofund/config/validate",
            &ConfigValidateRequest::new(edited.as_str()),
        )
        .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let validation: ConfigValidationResponse = decode_response(API_VERSION, &body).expect("DTO");
    assert!(validation.valid, "{:?}", validation.issues);
    assert_eq!(validation.pending_restart, ["token_file", "usage_file"]);
    assert_eq!(client.status().await.generation, 1);

    let (status, body) = client.apply(&revision(&original), &edited).await;
    let applied = ok(status, &body);
    assert_eq!(applied.pending_restart, ["token_file", "usage_file"]);
    assert_eq!(applied.not_applied.len(), 1);
    assert_eq!(applied.not_applied[0].field, "listen");
    assert_eq!(applied.not_applied[0].reason, "pending_l6d");

    let status = client.status().await;
    assert_eq!(status.pending_restart, applied.pending_restart);
    assert_eq!(status.not_applied, applied.not_applied);
    let read = client.config().await;
    assert_eq!(read.pending_restart, applied.pending_restart);
    assert_eq!(read.not_applied, applied.not_applied);

    let (status, body) = client.apply(&revision(&edited), &original).await;
    let reverted = ok(status, &body);
    assert!(reverted.pending_restart.is_empty());
    assert!(reverted.not_applied.is_empty());
    assert!(client.status().await.pending_restart.is_empty());

    let events = next_events(&mut stream, 2).await;
    match events.as_slice() {
        [
            (_, EventPayload::ConfigReloaded(first)),
            (_, EventPayload::ConfigReloaded(second)),
        ] => {
            assert_eq!(first.pending_restart, applied.pending_restart);
            assert_eq!(first.not_applied, applied.not_applied);
            assert!(second.pending_restart.is_empty());
            assert!(second.not_applied.is_empty());
        }
        other => panic!("expected two config_reloaded events, got {other:?}"),
    }
    // An open stream would hold the graceful drain of the control listener.
    drop(stream);
    running.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn atomic_apply_preserves_symlink_mode_owner_and_exact_bytes_and_fsyncs() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let scratch = Scratch::new("symlink");
    let original = config("");
    let target = scratch.config_dir.join("real.toml");
    std::fs::write(&target, &original).expect("target");
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).expect("mode");
    let link = scratch.config_dir.join("hofund.toml");
    std::os::unix::fs::symlink(&target, &link).expect("symlink");
    let before = std::fs::metadata(&target).expect("metadata");

    let running = start_with_options(
        &link,
        StartOptions {
            control_listen: Some("127.0.0.1:0".parse().expect("control address")),
            control_runtime: Some(ControlRuntime::new(None, scratch.runtime_dir.clone())),
            agent_session: Some(Session::new(scratch.runtime_dir.join("home")).with_path("")),
            ..StartOptions::default()
        },
    )
    .await
    .expect("daemon");
    let client = Client::new(&running, &scratch);
    let candidate = format!("{original}# applied through a link\n");
    let (status, body) = client.apply(&revision(&original), &candidate).await;
    ok(status, &body);

    assert!(
        std::fs::symlink_metadata(&link)
            .expect("link")
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::read(&target).expect("target"),
        candidate.as_bytes()
    );
    let after = std::fs::metadata(&target).expect("metadata");
    assert_eq!(after.permissions().mode() & 0o777, 0o640);
    assert_eq!((after.uid(), after.gid()), (before.uid(), before.gid()));
    assert!(scratch.config_dir.join("real.toml.lock").exists());
    let leftovers: Vec<_> = std::fs::read_dir(&scratch.config_dir)
        .expect("dir")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    running.shutdown().await;
}

/// Makes `dir` read-only, or returns false when this user can still write it.
#[cfg(unix)]
fn read_only(dir: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555)).expect("chmod");
    if std::fs::write(dir.join("probe"), "").is_ok() {
        let _ = std::fs::remove_file(dir.join("probe"));
        writable(dir);
        return false;
    }
    true
}

/// Whether a `config-*.lock` sidecar exists anywhere below `dir`.
#[cfg(unix)]
fn has_config_lock(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .any(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            (name.starts_with("config-") && name.ends_with(".lock"))
                || (path.is_dir() && has_config_lock(&path))
        })
}

#[cfg(unix)]
fn writable(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

#[cfg(unix)]
#[tokio::test]
async fn read_only_config_dir_reload_uses_runtime_sidecar_lock() {
    let scratch = Scratch::new("ro-reload");
    let original = config("");
    let running = start_at(&scratch, &scratch.path(), &original).await;
    let client = Client::new(&running, &scratch);
    if !read_only(&scratch.config_dir) {
        running.shutdown().await;
        return;
    }
    let (status, body) = client.reload(&revision(&original)).await;
    writable(&scratch.config_dir);
    assert_eq!(ok(status, &body).generation, 2);
    assert!(!scratch.config_dir.join("hofund.toml.lock").exists());
    let fallback = has_config_lock(&scratch.runtime_dir);
    assert!(fallback, "runtime sidecar lock was not used");
    running.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn read_only_config_dir_write_fails_clearly() {
    let scratch = Scratch::new("ro-write");
    let original = config("");
    let running = start_at(&scratch, &scratch.path(), &original).await;
    let client = Client::new(&running, &scratch);
    if !read_only(&scratch.config_dir) {
        running.shutdown().await;
        return;
    }
    let (status, body) = client
        .apply(&revision(&original), &format!("{original}# change\n"))
        .await;
    writable(&scratch.config_dir);
    assert_eq!(failure(status, &body, 500), ControlErrorKind::Internal);
    let error = decode_error_envelope(API_VERSION, &body)
        .expect("error")
        .error;
    let dir = std::fs::canonicalize(&scratch.config_dir).expect("dir");
    assert!(
        error.message.contains(&dir.display().to_string()),
        "{}",
        error.message
    );
    assert!(!error.message.contains(SECRET));
    assert_eq!(
        std::fs::read_to_string(scratch.path()).expect("read"),
        original
    );
    assert_eq!(client.status().await.generation, 1);
    running.shutdown().await;
}
