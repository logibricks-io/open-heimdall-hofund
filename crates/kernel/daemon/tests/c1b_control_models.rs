//! C-1b control-plane catalog disable/enable and route-model visibility.

mod support;

use std::path::Path;
use std::time::Duration;

use hofund_control_proto::{
    API_VERSION, CatalogDisabledListResponse, CatalogModelRequest, RouteModelListResponse,
    decode_response,
};
use hofund_daemon::{ControlRuntime, Running, StartOptions, start_with_options};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CATALOG: &str = r#"{"acme":{"models":{"live":{"id":"live","status":"active","limit":{"context":8000}},"gone":{"id":"gone","status":"retired","limit":{"context":8000}}}}}"#;
const STEP: Duration = Duration::from_secs(30);

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL.lock().await
}

async fn with_step<T>(label: &str, future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(STEP, future)
        .await
        .unwrap_or_else(|_| panic!("{label} timed out after {STEP:?}"))
}

fn seed_catalog(config_dir: &Path) {
    let root = config_dir.join(".hofund/catalog");
    std::fs::create_dir_all(&root).expect("catalog dir");
    std::fs::write(
        root.join("models.dev.json"),
        format!(r#"{{"catalog":{CATALOG}}}"#),
    )
    .expect("catalog cache");
}

fn base_config(server: &MockServer) -> String {
    format!(
        "listen = \"127.0.0.1:0\"\ntoken_file = \"hofund.token\"\n[[providers]]\nid = \"chat\"\nprotocol = \"openai-chat\"\nbase_url = \"{}/v1\"\napi_key = \"test-key\"\n[providers.models]\nrefresh = \"off\"\n[[routes]]\nmodel = \"live\"\nupstreams = [\"chat\"]\n[[routes]]\nmodel = \"gone\"\nupstreams = [\"chat\"]\n[[routes]]\nmodel = \"routed\"\nupstreams = [\"chat\"]\n",
        server.uri()
    )
}

fn token(dir: &Path) -> String {
    for candidate in ["hofund.token", "token"] {
        if let Ok(text) = std::fs::read_to_string(dir.join(candidate)) {
            return text.trim().to_owned();
        }
    }
    panic!("token file missing under {}", dir.display());
}

async fn start(dir: &Path, source: &str) -> Running {
    let config = dir.join("hofund.toml");
    std::fs::write(&config, source).expect("config");
    seed_catalog(dir);
    with_step("daemon start", async {
        start_with_options(
            &config,
            StartOptions {
                control_listen: Some("127.0.0.1:0".parse().expect("control")),
                control_runtime: Some(ControlRuntime::new(None, dir.to_path_buf())),
                skip_config_watch: true,
                ..StartOptions::default()
            },
        )
        .await
        .expect("daemon")
    })
    .await
}

struct ControlClient {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl ControlClient {
    fn new(running: &Running, dir: &Path) -> Self {
        Self {
            http: support::client(),
            base: format!(
                "http://{}",
                running.control_local_addr().expect("control addr")
            ),
            token: token(dir),
        }
    }

    async fn post<T: serde::Serialize>(
        &self,
        path: &str,
        body: &T,
    ) -> (reqwest::StatusCode, Vec<u8>) {
        with_step(&format!("POST {path}"), async {
            let response = self
                .http
                .post(format!("{}{path}", self.base))
                .bearer_auth(&self.token)
                .json(body)
                .send()
                .await
                .expect("post");
            let status = response.status();
            (status, response.bytes().await.expect("body").to_vec())
        })
        .await
    }

    async fn get(&self, path: &str) -> (reqwest::StatusCode, Vec<u8>) {
        with_step(&format!("GET {path}"), async {
            let response = self
                .http
                .get(format!("{}{path}", self.base))
                .bearer_auth(&self.token)
                .send()
                .await
                .expect("get");
            let status = response.status();
            (status, response.bytes().await.expect("body").to_vec())
        })
        .await
    }

    async fn route_models(&self) -> RouteModelListResponse {
        let (status, body) = self.get("/v1/hofund/models").await;
        assert_eq!(status, reqwest::StatusCode::OK);
        decode_response(API_VERSION, &body).expect("route models")
    }
}

#[tokio::test]
async fn c1b_14_control_disable_persists_and_hides_model() {
    let _lock = serial().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "ok"}}]
        })))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().expect("tempdir");
    let running = start(dir.path(), &base_config(&server)).await;
    let control = ControlClient::new(&running, dir.path());
    let (status, body) = control
        .post(
            "/v1/hofund/models/disable",
            &CatalogModelRequest::new("routed"),
        )
        .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let disabled: CatalogDisabledListResponse =
        decode_response(API_VERSION, &body).expect("disabled list");
    assert!(disabled.disabled.iter().any(|row| row == "routed"));
    let routes = control.route_models().await;
    assert!(!routes.models.iter().any(|row| row.id == "routed"));
    assert!(!routes.models.iter().any(|row| row.id == "gone"));
    assert!(routes.models.iter().any(|row| row.id == "live"));
    let config_text = std::fs::read_to_string(dir.path().join("hofund.toml")).expect("config");
    assert!(config_text.contains("\"routed\""));
    let model_addr = running.local_addr;
    let refused = with_step("disabled model refusal", async {
        support::client()
            .post(format!("http://{model_addr}/v1/chat/completions"))
            .bearer_auth(token(dir.path()))
            .json(
                &serde_json::json!({"model":"routed","messages":[{"role":"user","content":"hi"}]}),
            )
            .send()
            .await
            .expect("chat")
    })
    .await;
    assert_eq!(refused.status(), reqwest::StatusCode::FORBIDDEN);
    with_step("daemon shutdown", running.shutdown()).await;
}

#[tokio::test]
async fn c1b_15_control_enable_restores_listing() {
    let _lock = serial().await;
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let running = start(dir.path(), &base_config(&server)).await;
    let control = ControlClient::new(&running, dir.path());
    control
        .post(
            "/v1/hofund/models/disable",
            &CatalogModelRequest::new("routed"),
        )
        .await;
    let (status, body) = control
        .post(
            "/v1/hofund/models/enable",
            &CatalogModelRequest::new("routed"),
        )
        .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let enabled: CatalogDisabledListResponse =
        decode_response(API_VERSION, &body).expect("enabled list");
    assert!(!enabled.disabled.iter().any(|row| row == "routed"));
    let routes = control.route_models().await;
    assert!(routes.models.iter().any(|row| row.id == "routed"));
    with_step("daemon shutdown", running.shutdown()).await;
}

#[tokio::test]
async fn c1b_17_control_route_models_hide_retired_and_disabled() {
    let _lock = serial().await;
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let mut source = base_config(&server);
    source.push_str("[catalog]\ndisabled = [\"live\"]\n");
    let running = start(dir.path(), &source).await;
    let control = ControlClient::new(&running, dir.path());
    let routes = control.route_models().await;
    let ids: Vec<_> = routes.models.iter().map(|row| row.id.as_str()).collect();
    assert_eq!(ids, ["routed"]);
    with_step("daemon shutdown", running.shutdown()).await;
}

#[cfg(unix)]
#[tokio::test]
async fn c1b_14_unix_ipc_disable_catalog_model() {
    let _lock = serial().await;
    use std::os::unix::fs::PermissionsExt;

    let server = MockServer::start().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime = dir.path().join("runtime");
    std::fs::create_dir_all(&runtime).expect("runtime");
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).expect("mode");
    let config = dir.path().join("hofund.toml");
    std::fs::write(&config, base_config(&server)).expect("config");
    seed_catalog(dir.path());
    let running = with_step("unix daemon start", async {
        start_with_options(
            &config,
            StartOptions {
                control_enabled: true,
                control_runtime: Some(ControlRuntime::new(Some(runtime), dir.path().to_path_buf())),
                skip_config_watch: true,
                ..StartOptions::default()
            },
        )
        .await
        .expect("daemon")
    })
    .await;
    let socket = running.control_socket_path().expect("unix socket");
    let client = hofund_control_client::Client::unix(socket, token(dir.path()));
    let response = with_step("unix disable", client.disable_catalog_model("routed"))
        .await
        .expect("disable over unix");
    assert!(response.disabled.iter().any(|row| row == "routed"));
    with_step("daemon shutdown", running.shutdown()).await;
}
