//! Picks, manual refresh, and provider test control routes.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use hofund_control_proto::{
    API_VERSION, ControlErrorKind, ModelListResponse, PicksResponse, ProviderTestResponse,
    decode_error_envelope, decode_response,
};
use hofund_daemon::{ControlRuntime, Running, StartOptions, start_with_options};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use wiremock::matchers::{method, path};
use wiremock::{Match, Mock, MockServer, Request, ResponseTemplate};

struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = PathBuf::from("/tmp").join(format!(
            "hofund-daemon-picks-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("scratch directory");
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                self.0.join(".hofund/host/models"),
                std::fs::Permissions::from_mode(0o700),
            );
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct ModelBody(&'static str);

impl Match for ModelBody {
    fn matches(&self, request: &Request) -> bool {
        serde_json::from_slice::<Value>(&request.body)
            .ok()
            .and_then(|body| body.get("model").and_then(Value::as_str).map(str::to_owned))
            .is_some_and(|model| model == self.0)
    }
}

fn config(base_url: &str, extra: &str) -> String {
    format!(
        "listen = \"127.0.0.1:0\"\n[[providers]]\nid = \"chat\"\nprotocol = \"openai-chat\"\nbase_url = \"{base_url}/v1\"\napi_key = \"test-key\"\n[providers.models]\nrefresh = \"off\"\n{extra}"
    )
}

async fn start(dir: &Path, source: &str) -> Running {
    let config = dir.join("hofund.toml");
    std::fs::write(&config, source).expect("config");
    start_with_options(
        &config,
        StartOptions {
            control_listen: Some("127.0.0.1:0".parse().expect("control address")),
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

fn seed(dir: &Path, provider: &str, state: &Value) {
    let path = state_path(dir, provider);
    std::fs::create_dir_all(path.parent().expect("state parent")).expect("state directory");
    std::fs::write(&path, serde_json::to_vec_pretty(state).expect("state JSON"))
        .expect("model state");
}

fn seed_list(dir: &Path, ids: &[&str], picks: &[&str]) {
    let list = ids.iter().map(|id| json!({"id": id})).collect::<Vec<_>>();
    seed(
        dir,
        "chat",
        &json!({
            "version": 1,
            "id": "chat",
            "fetched_at": 1234,
            "list": list,
            "picks": picks,
            "delisted": ["former"]
        }),
    );
}

struct Control {
    client: reqwest::Client,
    base: String,
    token: String,
}

impl Control {
    fn new(dir: &Path, running: &Running) -> Self {
        Self {
            client: reqwest::Client::new(),
            base: format!(
                "http://{}/v1/hofund/providers",
                running.control_local_addr().expect("control")
            ),
            token: std::fs::read_to_string(dir.join("hofund.token"))
                .expect("token")
                .trim()
                .to_owned(),
        }
    }

    async fn get(&self, route: &str) -> (reqwest::StatusCode, bytes::Bytes) {
        let response = self
            .client
            .get(format!("{}/{route}", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .expect("GET");
        (response.status(), response.bytes().await.expect("body"))
    }

    async fn send(
        &self,
        method: reqwest::Method,
        route: &str,
        body: Option<Value>,
    ) -> (reqwest::StatusCode, bytes::Bytes) {
        let mut request = self
            .client
            .request(method, format!("{}/{route}", self.base))
            .bearer_auth(&self.token);
        if let Some(body) = body {
            request = request.json(&body);
        } else {
            request = request.header(reqwest::header::CONTENT_TYPE, "application/json");
        }
        let response = request.send().await.expect("request");
        (response.status(), response.bytes().await.expect("body"))
    }

    async fn picks(&self) -> PicksResponse {
        let (status, bytes) = self.get("chat/picks").await;
        assert_eq!(status, reqwest::StatusCode::OK);
        decode_response(API_VERSION, &bytes).expect("picks")
    }

    async fn put(
        &self,
        revision: &str,
        form: &str,
        ids: &[&str],
    ) -> (reqwest::StatusCode, bytes::Bytes) {
        self.send(
            reqwest::Method::PUT,
            "chat/picks",
            Some(json!({"version": API_VERSION, "expected_picks_revision": revision, form: ids})),
        )
        .await
    }
}

fn file_revision(dir: &Path) -> String {
    format!(
        "{:x}",
        Sha256::digest(std::fs::read(state_path(dir, "chat")).expect("state file"))
    )
}

fn error_kind(bytes: &[u8]) -> (ControlErrorKind, Vec<String>, Option<u64>) {
    let envelope = decode_error_envelope(API_VERSION, bytes).expect("error envelope");
    (
        envelope.error.kind,
        envelope
            .error
            .details
            .iter()
            .map(|issue| issue.reason.clone())
            .collect(),
        envelope.error.retry_after_ms,
    )
}

#[tokio::test]
async fn picks_route_sets_and_returns_revision() {
    let server = MockServer::start().await;
    let dir = Scratch::new("set");
    seed_list(&dir.0, &["a", "b", "c"], &[]);
    let running = start(&dir.0, &config(&server.uri(), "")).await;
    let control = Control::new(&dir.0, &running);
    let before = control.picks().await;
    assert!(before.picks.is_empty());
    assert_eq!(before.delisted, ["former"]);
    let (status, bytes) = control
        .put(&before.picks_revision, "ids", &["b", "a", "b"])
        .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let set: PicksResponse = decode_response(API_VERSION, &bytes).expect("set picks");
    assert_eq!(set.picks, ["b", "a"]);
    assert_eq!(set.picked_from, ["a", "b", "c"]);
    assert_eq!(set.delisted, ["former"]);
    assert_eq!(set.picks_revision, file_revision(&dir.0));
    assert_ne!(set.picks_revision, before.picks_revision);
    let (status, bytes) = control.put(&set.picks_revision, "ids", &["former"]).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let restored: PicksResponse = decode_response(API_VERSION, &bytes).expect("restored");
    assert_eq!(restored.picks, ["former"]);
    assert!(restored.delisted.is_empty());
    let read = control.picks().await;
    assert_eq!(read, restored);
    assert_eq!(read.picks_revision, file_revision(&dir.0));
    running.shutdown().await;
}

#[tokio::test]
async fn picks_route_stale_revision_returns_409() {
    let server = MockServer::start().await;
    let dir = Scratch::new("stale");
    seed_list(&dir.0, &["a", "b"], &["a"]);
    let running = start(&dir.0, &config(&server.uri(), "")).await;
    let control = Control::new(&dir.0, &running);
    let current = control.picks().await;
    let file = std::fs::read(state_path(&dir.0, "chat")).expect("state file");
    let stale = "0".repeat(64);
    assert_ne!(current.picks_revision, stale);
    let (status, bytes) = control.put(&stale, "ids", &["b"]).await;
    assert_eq!(status, reqwest::StatusCode::CONFLICT);
    let (kind, reasons, _) = error_kind(&bytes);
    assert_eq!(kind, ControlErrorKind::Conflict);
    assert_eq!(reasons, ["stale"]);
    assert_eq!(
        std::fs::read(state_path(&dir.0, "chat")).expect("state file"),
        file
    );
    assert_eq!(control.picks().await, current);
    running.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn picks_route_save_failure_returns_500_without_revision() {
    use std::os::unix::fs::PermissionsExt;
    let server = MockServer::start().await;
    let dir = Scratch::new("read-only");
    seed_list(&dir.0, &["a", "b"], &[]);
    let running = start(&dir.0, &config(&server.uri(), "")).await;
    let control = Control::new(&dir.0, &running);
    let before = control.picks().await;
    let models = dir.0.join(".hofund/host/models");
    std::fs::set_permissions(&models, std::fs::Permissions::from_mode(0o500))
        .expect("read-only state directory");
    let (status, bytes) = control.put(&before.picks_revision, "ids", &["a"]).await;
    std::fs::set_permissions(&models, std::fs::Permissions::from_mode(0o700))
        .expect("writable state directory");
    assert_eq!(status, reqwest::StatusCode::INTERNAL_SERVER_ERROR);
    assert!(!String::from_utf8_lossy(&bytes).contains("picks_revision"));
    let (kind, _, _) = error_kind(&bytes);
    assert_eq!(kind, ControlErrorKind::Internal);
    assert_eq!(control.picks().await, before);
    running.shutdown().await;

    let dir = Scratch::new("mismatched");
    seed(
        &dir.0,
        "chat",
        &json!({"version": 1, "id": "other", "list": [{"id": "a"}], "picks": ["a"]}),
    );
    let file = std::fs::read(state_path(&dir.0, "chat")).expect("state file");
    let running = start(&dir.0, &config(&server.uri(), "")).await;
    let control = Control::new(&dir.0, &running);
    let before = control.picks().await;
    assert!(before.picks.is_empty());
    let (status, bytes) = control.put(&before.picks_revision, "ids", &["b"]).await;
    assert_eq!(status, reqwest::StatusCode::INTERNAL_SERVER_ERROR);
    assert!(!String::from_utf8_lossy(&bytes).contains("picks_revision"));
    assert_eq!(control.picks().await, before);
    assert_eq!(
        std::fs::read(state_path(&dir.0, "chat")).expect("state file"),
        file
    );
    running.shutdown().await;
}

#[tokio::test]
async fn provider_picks_edit_forms_match_reference() {
    let server = MockServer::start().await;
    let dir = Scratch::new("edit");
    seed_list(&dir.0, &["a", "b", "c"], &[]);
    let running = start(&dir.0, &config(&server.uri(), "")).await;
    let control = Control::new(&dir.0, &running);

    let revision = control.picks().await.picks_revision;
    let (status, bytes) = control.put(&revision, "edit", &["-b"]).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let edited: PicksResponse = decode_response(API_VERSION, &bytes).expect("edit");
    assert_eq!(edited.picks, ["a", "c"]);

    let (status, bytes) = control
        .put(&edited.picks_revision, "edit", &["+chat/d", "+a"])
        .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let edited: PicksResponse = decode_response(API_VERSION, &bytes).expect("add");
    assert_eq!(edited.picks, ["a", "c", "d"]);

    let (status, bytes) = control.put(&edited.picks_revision, "edit", &["-zz"]).await;
    assert_eq!(status, reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error_kind(&bytes).1, ["not_exposed"]);

    let (status, bytes) = control
        .put(&edited.picks_revision, "edit", &["-a", "-chat/c", "-d"])
        .await;
    assert_eq!(status, reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error_kind(&bytes).1, ["empty_result"]);
    assert_eq!(control.picks().await, edited);

    let (status, bytes) = control
        .put(&edited.picks_revision, "ids", &["chat/x"])
        .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let replaced: PicksResponse = decode_response(API_VERSION, &bytes).expect("replace");
    assert_eq!(replaced.picks, ["chat/x"]);

    let (status, bytes) = control
        .put(&replaced.picks_revision, "edit", &["all"])
        .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let reset: PicksResponse = decode_response(API_VERSION, &bytes).expect("reset");
    assert!(reset.picks.is_empty());
    assert_eq!(reset.delisted, ["former"]);
    running.shutdown().await;
}

#[tokio::test]
async fn picks_get_unknown_provider_404_creates_nothing() {
    let server = MockServer::start().await;
    let dir = Scratch::new("unknown");
    let running = start(&dir.0, &config(&server.uri(), "")).await;
    let control = Control::new(&dir.0, &running);
    let (status, bytes) = control.get("missing/picks").await;
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND);
    assert_eq!(error_kind(&bytes).0, ControlErrorKind::NotFound);
    let (status, _) = control
        .send(
            reqwest::Method::PUT,
            "missing/picks",
            Some(json!({"version": API_VERSION, "expected_picks_revision": "0".repeat(64), "ids": ["a"]})),
        )
        .await;
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND);
    let files = std::fs::read_dir(dir.0.join(".hofund/host/models"))
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(files, 0);
    running.shutdown().await;
}

#[tokio::test]
async fn models_refresh_drops_gone_picks_applies_auto_include_and_saves() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"object": "list", "data": [{"id": "b"}, {"id": "new-x"}, {"id": "other"}]}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    let dir = Scratch::new("refresh");
    seed_list(&dir.0, &["a", "b"], &["a", "b"]);
    let running = start(
        &dir.0,
        &config(&server.uri(), "auto_include = [\"new-*\"]\n"),
    )
    .await;
    let control = Control::new(&dir.0, &running);
    let (status, bytes) = control
        .send(reqwest::Method::POST, "chat/models/refresh", None)
        .await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    let list: ModelListResponse = decode_response(API_VERSION, &bytes).expect("refresh");
    assert_eq!(list.provider, "chat");
    assert_eq!(list.removed, ["a"]);
    assert_eq!(list.added, ["new-x"]);
    assert!(list.delisted.contains(&"a".to_owned()));
    assert!(list.fetched_at_ms.is_some_and(|at| at > 1234));
    let ids = list
        .models
        .iter()
        .map(|model| model.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, ["b", "new-x", "other"]);
    let saved: Value =
        serde_json::from_slice(&std::fs::read(state_path(&dir.0, "chat")).expect("state file"))
            .expect("state JSON");
    assert_eq!(saved["picks"], json!(["new-x", "b"]));
    assert!(
        saved["delisted"]
            .as_array()
            .expect("delisted")
            .contains(&json!("a"))
    );
    assert_eq!(control.picks().await.picks_revision, file_revision(&dir.0));
    running.shutdown().await;
    server.verify().await;
}

#[tokio::test]
async fn refresh_upstream_429_maps_to_429_with_retry_after_ms() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "7")
                .set_body_json(json!({"error": {"message": "slow down"}})),
        )
        .mount(&server)
        .await;
    let dir = Scratch::new("refresh-429");
    let running = start(&dir.0, &config(&server.uri(), "")).await;
    let control = Control::new(&dir.0, &running);
    let (status, bytes) = control
        .send(reqwest::Method::POST, "chat/models/refresh", None)
        .await;
    assert_eq!(
        status,
        reqwest::StatusCode::TOO_MANY_REQUESTS,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    let (kind, _, retry_after_ms) = error_kind(&bytes);
    assert_eq!(kind, ControlErrorKind::Upstream);
    assert_eq!(retry_after_ms, Some(7000));
    assert!(!String::from_utf8_lossy(&bytes).contains("test-key"));
    running.shutdown().await;
}

#[tokio::test]
async fn provider_test_route_probes_mock_upstream_exact_request() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"object": "list", "data": [{"id": "only"}]})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(ModelBody("only"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"choices": []})))
        .expect(1)
        .mount(&server)
        .await;
    let dir = Scratch::new("test-one");
    let running = start(&dir.0, &config(&server.uri(), "")).await;
    let control = Control::new(&dir.0, &running);
    let (status, bytes) = control
        .send(
            reqwest::Method::POST,
            "chat/test",
            Some(json!({"version": API_VERSION, "models": null})),
        )
        .await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    let tested: ProviderTestResponse = decode_response(API_VERSION, &bytes).expect("test");
    assert_eq!(tested.provider, "chat");
    assert_eq!(tested.results.len(), 1);
    let result = &tested.results[0];
    assert_eq!(result.model, "only");
    assert!(result.ok);
    assert!(result.latency_ms.is_some());
    assert!(result.error.is_none());
    let requests = server.received_requests().await.expect("recorded requests");
    let probes = requests
        .iter()
        .filter(|request| request.method == wiremock::http::Method::POST)
        .collect::<Vec<_>>();
    assert_eq!(probes.len(), 1);
    assert_eq!(probes[0].url.path(), "/v1/chat/completions");
    let body: Value = serde_json::from_slice(&probes[0].body).expect("probe body");
    assert_eq!(body["model"], "only");
    assert!(
        body["messages"]
            .as_array()
            .is_some_and(|messages| !messages.is_empty())
    );
    running.shutdown().await;
    server.verify().await;
}

#[tokio::test]
async fn provider_test_named_models_keep_order_and_per_model_errors() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"object": "list", "data": [{"id": "good"}, {"id": "bad"}]})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(ModelBody("good"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"choices": []})))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(ModelBody("bad"))
        .respond_with(
            ResponseTemplate::new(500).set_body_json(json!({"error": {"message": "broken"}})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let dir = Scratch::new("test-named");
    let running = start(&dir.0, &config(&server.uri(), "")).await;
    let control = Control::new(&dir.0, &running);
    let (status, bytes) = control
        .send(
            reqwest::Method::POST,
            "chat/test",
            Some(json!({"version": API_VERSION, "models": ["good", "bad", "good"]})),
        )
        .await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    let tested: ProviderTestResponse = decode_response(API_VERSION, &bytes).expect("test");
    let models = tested
        .results
        .iter()
        .map(|result| (result.model.as_str(), result.ok))
        .collect::<Vec<_>>();
    assert_eq!(models, [("good", true), ("bad", false), ("good", true)]);
    let failed = &tested.results[1];
    assert_eq!(failed.status, Some(500));
    assert!(failed.latency_ms.is_some());
    let error = failed.error.as_ref().expect("per-model error");
    assert_eq!(error.kind, ControlErrorKind::Upstream);
    assert!(!error.message.contains("test-key"));
    running.shutdown().await;
    server.verify().await;
}

#[tokio::test]
async fn provider_test_selection_error_sends_no_request() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"object": "list", "data": []})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"choices": []})))
        .expect(0)
        .mount(&server)
        .await;
    let dir = Scratch::new("test-selection");
    let running = start(&dir.0, &config(&server.uri(), "")).await;
    let control = Control::new(&dir.0, &running);
    let (status, bytes) = control
        .send(
            reqwest::Method::POST,
            "chat/test",
            Some(json!({"version": API_VERSION, "models": []})),
        )
        .await;
    assert_eq!(
        status,
        reqwest::StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    assert_eq!(error_kind(&bytes).0, ControlErrorKind::Unprocessable);
    running.shutdown().await;
    server.verify().await;
}
