//! L6b configuration transaction and L6d live relisten over the control
//! listener.

use std::path::{Path, PathBuf};
use std::time::Duration;

use hofund_agent::Session;
use hofund_control_proto::{
    API_VERSION, ConfigApplyRequest, ConfigResponse, ConfigValidateRequest,
    ConfigValidationResponse, ControlErrorKind, EventEnvelope, EventPayload, ListenSource,
    ReloadRequest, StatusResponse, decode_error_envelope, decode_response,
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
    start_watching(scratch, config_path, text, true).await
}

/// Starts a daemon; `watch = false` leaves hand edits unpublished.
async fn start_watching(scratch: &Scratch, config_path: &Path, text: &str, watch: bool) -> Running {
    let mut options = options(scratch);
    options.skip_config_watch = !watch;
    start_with(config_path, text, options).await
}

/// Isolated options with control enabled, no `HOFUND_ADDR` and no watcher.
fn options(scratch: &Scratch) -> StartOptions {
    StartOptions {
        control_listen: Some("127.0.0.1:0".parse().expect("control address")),
        control_runtime: Some(ControlRuntime::new(None, scratch.runtime_dir.clone())),
        agent_session: Some(Session::new(scratch.runtime_dir.join("home")).with_path("")),
        skip_config_watch: true,
        hofund_addr: Some(String::new()),
        ..StartOptions::default()
    }
}

async fn start_with(config_path: &Path, text: &str, options: StartOptions) -> Running {
    std::fs::write(config_path, text).expect("config");
    start_with_options(config_path, options)
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
    let running = start_watching(&scratch, &scratch.path(), &original, false).await;
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

    let edited = config("token_file = \"other.token\"\nusage_file = \"other.db\"\n");
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
    assert!(applied.not_applied.is_empty(), "{:?}", applied.not_applied);

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

    // Each successful publish emits config_reloaded then snapshot_published (L6f).
    let events = next_events(&mut stream, 4).await;
    match events.as_slice() {
        [
            (_, EventPayload::ConfigReloaded(first)),
            (_, EventPayload::SnapshotPublished(_)),
            (_, EventPayload::ConfigReloaded(second)),
            (_, EventPayload::SnapshotPublished(_)),
        ] => {
            assert_eq!(first.pending_restart, applied.pending_restart);
            assert_eq!(first.not_applied, applied.not_applied);
            assert!(second.pending_restart.is_empty());
            assert!(second.not_applied.is_empty());
        }
        other => panic!("expected config_reloaded/snapshot_published pairs, got {other:?}"),
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

#[tokio::test]
async fn config_text_never_in_events_status_or_errors() {
    let scratch = Scratch::new("secret-free");
    let original = config("");
    let running = start_at(&scratch, &scratch.path(), &original).await;
    let client = Client::new(&running, &scratch);
    let mut stream = client.events().await;

    // A successful apply that adds a provider with a new secret.
    let rotated = format!(
        "{original}[[providers]]\nid = \"second\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key = \"{SECRET}-second\"\n"
    );
    let (status, body) = client.apply(&revision(&original), &rotated).await;
    let applied = ok(status, &body);
    assert_eq!(
        applied.text, rotated,
        "only the config read echoes the text"
    );

    // A build failure whose message would quote a document value.
    let broken = rotated.replace(
        "upstreams = [\"chat\"]",
        &format!("upstreams = [\"{SECRET}-route\"]"),
    );
    let (status, body) = client.apply(&applied.disk_revision, &broken).await;
    assert_eq!(failure(status, &body, 422), ControlErrorKind::InvalidConfig);
    assert!(
        !String::from_utf8_lossy(&body).contains(SECRET),
        "{}",
        String::from_utf8_lossy(&body)
    );

    // A failed reload of the same text on disk.
    std::fs::write(scratch.path(), &broken).expect("hand edit");
    let (status, body) = client.reload(&revision(&broken)).await;
    assert_eq!(failure(status, &body, 422), ControlErrorKind::InvalidConfig);
    assert!(
        !String::from_utf8_lossy(&body).contains(SECRET),
        "{}",
        String::from_utf8_lossy(&body)
    );

    // `next_events` fails on any frame that contains the secret.
    // Successful apply also emits snapshot_published (L6f).
    let events = next_events(&mut stream, 4).await;
    let names: Vec<_> = events.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        [
            "config_reloaded",
            "snapshot_published",
            "config_rejected",
            "config_rejected"
        ]
    );
    let status = client.status().await;
    assert_eq!(status.generation, 2);
    let rendered = format!("{status:?}");
    assert!(!rendered.contains(SECRET), "{rendered}");
    drop(stream);
    running.shutdown().await;
}

#[tokio::test]
async fn rotated_key_reaches_the_model_host_after_apply() {
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", "Bearer rotated-key"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"data": [{"id": "fresh"}]})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let scratch = Scratch::new("rotated");
    let original = config("").replace("http://127.0.0.1:9", &server.uri());
    let running = start_at(&scratch, &scratch.path(), &original).await;
    let client = Client::new(&running, &scratch);

    let rotated = original.replace(SECRET, "rotated-key");
    let (status, body) = client.apply(&revision(&original), &rotated).await;
    ok(status, &body);
    let (status, body) = client
        .send(
            client
                .http
                .post(format!(
                    "{}/v1/hofund/providers/chat/models/refresh",
                    client.base
                ))
                .header("content-type", "application/json"),
        )
        .await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let listed: hofund_control_proto::ModelListResponse =
        decode_response(API_VERSION, &body).expect("model list");
    let ids: Vec<_> = listed
        .models
        .iter()
        .map(|model| model.id.as_str())
        .collect();
    assert_eq!(ids, ["fresh"]);
    running.shutdown().await;
}

/// Returns a loopback address whose port was free a moment ago.
fn free_addr() -> std::net::SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("probe bind")
        .local_addr()
        .expect("probe address")
}

fn with_listen(listen: &str) -> String {
    config("").replacen(
        "listen = \"127.0.0.1:0\"",
        &format!("listen = \"{listen}\""),
        1,
    )
}

async fn health(addr: std::net::SocketAddr) -> Result<reqwest::StatusCode, reqwest::Error> {
    reqwest::Client::new()
        .get(format!("http://{addr}/health"))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .map(|response| response.status())
}

#[tokio::test]
async fn relisten_port_change_moves_the_model_listener_and_status() {
    let scratch = Scratch::new("relisten-move");
    let original = config("");
    let running = start_with(&scratch.path(), &original, options(&scratch)).await;
    let client = Client::new(&running, &scratch);
    let old = running.listen_addr();
    assert_eq!(
        client.status().await.model_listener.running,
        Some(old.to_string())
    );

    let to = free_addr();
    let edited = with_listen(&to.to_string());
    let (status, body) = client.apply(&revision(&original), &edited).await;
    let applied = ok(status, &body);
    assert!(applied.not_applied.is_empty(), "{:?}", applied.not_applied);
    assert!(applied.pending_restart.is_empty());

    let listener = client.status().await.model_listener;
    assert_eq!(listener.configured, to.to_string());
    assert_eq!(listener.effective, to.to_string());
    assert_eq!(listener.running, Some(to.to_string()));
    assert_eq!(listener.effective_source, ListenSource::Config);
    assert_eq!(running.listen_addr(), to);
    assert_eq!(
        health(to).await.expect("new listener"),
        reqwest::StatusCode::OK
    );
    assert!(health(old).await.is_err(), "the old listener is closed");
    running.shutdown().await;
}

#[tokio::test]
async fn relisten_failure_publishes_rest_and_reports_not_applied() {
    let scratch = Scratch::new("relisten-fail");
    let original = config("");
    let running = start_with(&scratch.path(), &original, options(&scratch)).await;
    let client = Client::new(&running, &scratch);
    let old = running.listen_addr();
    let mut stream = client.events().await;

    let held = std::net::TcpListener::bind("127.0.0.1:0").expect("held port");
    let target = held.local_addr().expect("held address");
    let edited = with_listen(&target.to_string()).replace("model = \"m\"", "model = \"m2\"");
    let (status, body) = client.apply(&revision(&original), &edited).await;
    let applied = ok(status, &body);
    // The rest of the candidate is live even though the listener stayed put.
    assert_eq!(applied.generation, 2);
    assert_eq!(applied.runtime_revision, revision(&edited));
    assert_eq!(applied.not_applied.len(), 1, "{:?}", applied.not_applied);
    assert_eq!(applied.not_applied[0].field, "listen");
    assert_eq!(applied.not_applied[0].reason, "bind_failed");
    assert!(!applied.not_applied[0].message.contains(SECRET));

    let status = client.status().await;
    assert_eq!(status.not_applied, applied.not_applied);
    assert_eq!(status.model_listener.configured, target.to_string());
    assert_eq!(status.model_listener.running, Some(old.to_string()));
    assert_eq!(client.config().await.not_applied, applied.not_applied);
    assert_eq!(
        health(old).await.expect("old listener"),
        reqwest::StatusCode::OK
    );

    let events = next_events(&mut stream, 3).await;
    match events.as_slice() {
        [
            (first, EventPayload::ConfigReloaded(reloaded)),
            (_, EventPayload::SnapshotPublished(_)),
            (third, EventPayload::ListenerRelistenFailed(failed)),
        ] => {
            assert_eq!(first, "config_reloaded");
            assert_eq!(third, "listener_relisten_failed");
            assert_eq!(reloaded.not_applied, applied.not_applied);
            assert_eq!(failed.issue, applied.not_applied[0]);
            assert_eq!(failed.running, old.to_string());
            assert_eq!(failed.generation, 2);
            assert_eq!(failed.runtime_revision, applied.runtime_revision);
            assert_eq!(failed.disk_revision, applied.disk_revision);
        }
        other => panic!(
            "expected config_reloaded, snapshot_published, listener_relisten_failed; got {other:?}"
        ),
    }

    // Once the port frees up, an explicit reload of the same revision retries.
    drop(held);
    let (status, body) = client.reload(&applied.disk_revision).await;
    let retried = ok(status, &body);
    assert_eq!(retried.generation, 3);
    assert!(retried.not_applied.is_empty(), "{:?}", retried.not_applied);
    assert_eq!(
        client.status().await.model_listener.running,
        Some(target.to_string())
    );
    assert_eq!(running.listen_addr(), target);
    drop(stream);
    running.shutdown().await;
}

#[tokio::test]
async fn relisten_restore_failure_exits_nonzero() {
    let scratch = Scratch::new("relisten-restore");
    let original = config("");
    let mut options = options(&scratch);
    options.relisten_hook = Some(hofund_daemon::BindHook::new(|_| {
        Err(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            "test hook refuses every bind",
        ))
    }));
    let running = start_with(&scratch.path(), &original, options).await;
    let client = Client::new(&running, &scratch);
    let port = running.listen_addr().port();

    // Same port, different host: the old socket closes first, so a failed
    // bind needs a restore, and the hook makes that fail too.
    let edited = with_listen(&format!("0.0.0.0:{port}"));
    let (status, body) = client.apply(&revision(&original), &edited).await;
    let applied = ok(status, &body);
    assert_eq!(applied.not_applied.len(), 1, "{:?}", applied.not_applied);
    assert_eq!(applied.not_applied[0].reason, "restore_failed");

    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        hofund_daemon::serve_until_shutdown(running),
    )
    .await
    .expect("a lost model listener ends the daemon");
    assert!(outcome.is_err(), "restore failure must be fatal");
}

#[tokio::test]
async fn reload_listen_with_hofund_addr_reports_not_applied_env_override() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let scratch = Scratch::new("relisten-env");
    let original = config("");
    let env = free_addr();
    let binds = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&binds);
    let mut options = options(&scratch);
    options.hofund_addr = Some(format!(" {env} "));
    options.relisten_hook = Some(hofund_daemon::BindHook::new(move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }));
    let running = start_with(&scratch.path(), &original, options).await;
    let client = Client::new(&running, &scratch);
    assert_eq!(running.listen_addr(), env);

    let status = client.status().await;
    assert_eq!(
        status.model_listener.effective_source,
        ListenSource::Environment
    );
    assert_eq!(status.model_listener.configured, "127.0.0.1:0");
    assert_eq!(status.model_listener.effective, env.to_string());
    assert_eq!(status.model_listener.running, Some(env.to_string()));
    assert_eq!(status.not_applied.len(), 1, "{:?}", status.not_applied);
    assert_eq!(status.not_applied[0].field, "listen");
    assert_eq!(status.not_applied[0].reason, "env_override");

    let mut stream = client.events().await;
    let elsewhere = free_addr();
    let edited = with_listen(&elsewhere.to_string());
    let (status, body) = client.apply(&revision(&original), &edited).await;
    let applied = ok(status, &body);
    assert_eq!(applied.not_applied.len(), 1, "{:?}", applied.not_applied);
    assert_eq!(applied.not_applied[0].reason, "env_override");
    assert_eq!(running.listen_addr(), env);
    assert_eq!(
        binds.load(Ordering::SeqCst),
        0,
        "HOFUND_ADDR never relistens"
    );

    // Override is not a relisten failure; each apply emits snapshot_published (L6f).
    let (status, body) = client.apply(&revision(&edited), &original).await;
    ok(status, &body);
    let events = next_events(&mut stream, 4).await;
    match events.as_slice() {
        [
            (_, EventPayload::ConfigReloaded(first)),
            (_, EventPayload::SnapshotPublished(_)),
            (_, EventPayload::ConfigReloaded(_)),
            (_, EventPayload::SnapshotPublished(_)),
        ] => assert_eq!(first.not_applied, applied.not_applied),
        other => panic!("expected config_reloaded/snapshot_published pairs, got {other:?}"),
    }
    drop(stream);
    running.shutdown().await;
}
