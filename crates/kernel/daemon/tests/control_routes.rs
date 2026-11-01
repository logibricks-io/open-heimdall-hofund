//! Read-only daemon control operations over TCP and local IPC.

mod support;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use hofund_control_proto::{
    API_VERSION, ControlErrorKind, ModelListResponse, ProviderListResponse, RouteModelListResponse,
    decode_error_envelope, decode_response,
};
use hofund_daemon::{ControlRuntime, Running, StartOptions, start_with_options};
use serde_json::json;
use sha2::{Digest, Sha256};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = PathBuf::from("/tmp").join(format!(
            "hofund-daemon-routes-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("scratch directory");
        Self(path)
    }

    fn join(&self, path: &str) -> PathBuf {
        self.0.join(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn provider_config(base_url: &str) -> String {
    format!(
        "listen = \"127.0.0.1:0\"\n[[providers]]\nid = \"chat\"\nprotocol = \"openai-chat\"\nbase_url = \"{base_url}/v1\"\napi_key = \"never-return-this-key\"\n"
    )
}

async fn start_tcp(dir: &Path, source: &str) -> Running {
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

#[cfg(unix)]
async fn start_unix(dir: &Path, source: &str) -> Running {
    use std::os::unix::fs::PermissionsExt;
    let config = dir.join("hofund.toml");
    std::fs::write(&config, source).expect("config");
    let runtime = dir.join("runtime");
    std::fs::create_dir_all(&runtime).expect("runtime directory");
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))
        .expect("runtime permissions");
    start_with_options(
        &config,
        StartOptions {
            control_enabled: true,
            control_runtime: Some(ControlRuntime::new(Some(runtime), dir.to_path_buf())),
            ..StartOptions::default()
        },
    )
    .await
    .expect("daemon")
}

fn token(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("hofund.token"))
        .expect("token")
        .trim()
        .to_owned()
}

fn seed_models(dir: &Path, provider: &str, models: serde_json::Value) {
    let root = dir.join(".hofund/host/models");
    std::fs::create_dir_all(&root).expect("model state directory");
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
    let path = root.join(format!("{slug}-{}.json", &hash[..16]));
    let state = json!({
        "version": 1,
        "id": provider,
        "fetched_at": 1234,
        "list": models,
        "delisted": ["former"]
    });
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&state).expect("state JSON"),
    )
    .expect("model state");
}

#[tokio::test]
async fn provider_list_is_secret_free() {
    let server = MockServer::start().await;
    let dir = Scratch::new("providers");
    seed_models(
        &dir.0,
        "chat",
        json!([{"id": "model-a"}, {"id": "model-b"}]),
    );
    let running = start_tcp(&dir.0, &provider_config(&server.uri())).await;
    let response = support::client()
        .get(format!(
            "http://{}/v1/hofund/providers",
            running.control_local_addr().expect("control")
        ))
        .bearer_auth(token(&dir.0))
        .send()
        .await
        .expect("providers");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let bytes = response.bytes().await.expect("provider body");
    let text = String::from_utf8_lossy(&bytes);
    assert!(!text.contains("never-return-this-key"));
    assert!(!text.contains("base_url"));
    assert!(!text.contains(&server.uri()));
    let list: ProviderListResponse = decode_response(API_VERSION, &bytes).expect("provider list");
    assert_eq!(list.providers.len(), 1);
    let provider = &list.providers[0];
    assert_eq!(provider.id, "chat");
    assert_eq!(provider.kind, "openai-compat");
    assert_eq!(provider.protocol, "openai-chat");
    assert!(provider.ready);
    assert_eq!(provider.model_count, 2);
    support::shutdown(running).await;
}

#[cfg(unix)]
#[tokio::test]
async fn cached_models_route_makes_no_upstream_call() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let dir = Scratch::new("cached-models");
    seed_models(
        &dir.0,
        "chat",
        json!([
            {"id": "zero", "display_name": "Zero", "context": 0, "max_output": 0},
            {"id": "bounded", "context": 4096, "max_output": 512}
        ]),
    );
    let running = start_unix(&dir.0, &provider_config(&server.uri())).await;
    let socket = running.control_socket_path().expect("Unix socket");
    let client = support::builder()
        .unix_socket(socket)
        .build()
        .expect("Unix client");
    let response = client
        .get("http://hofund-control/v1/hofund/providers/chat/models")
        .bearer_auth(token(&dir.0))
        .send()
        .await
        .expect("cached models");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let list: ModelListResponse = decode_response(
        API_VERSION,
        &response.bytes().await.expect("cached model body"),
    )
    .expect("cached model list");
    assert_eq!(list.provider, "chat");
    assert_eq!(list.fetched_at_ms, Some(1234));
    assert_eq!(list.delisted, ["former"]);
    assert_eq!(list.models.len(), 2);
    assert_eq!(list.models[0].context, None);
    assert_eq!(list.models[0].max_output, None);
    assert_eq!(list.models[1].context, Some(4096));
    assert_eq!(list.models[1].max_output, Some(512));
    drop(client);
    support::shutdown(running).await;
    server.verify().await;
}

#[tokio::test]
async fn unknown_provider_returns_404_without_state_file() {
    let server = MockServer::start().await;
    let dir = Scratch::new("unknown-provider");
    let running = start_tcp(&dir.0, &provider_config(&server.uri())).await;
    let response = support::client()
        .get(format!(
            "http://{}/v1/hofund/providers/missing/models",
            running.control_local_addr().expect("control")
        ))
        .bearer_auth(token(&dir.0))
        .send()
        .await
        .expect("unknown provider");
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
    let envelope = decode_error_envelope(
        API_VERSION,
        &response.bytes().await.expect("unknown provider body"),
    )
    .expect("error envelope");
    assert_eq!(envelope.error.kind, ControlErrorKind::NotFound);
    let state = dir.join(".hofund/host/models");
    let files = std::fs::read_dir(state)
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(files, 0);
    support::shutdown(running).await;
}

#[tokio::test]
async fn route_models_in_config_order() {
    let server = MockServer::start().await;
    let dir = Scratch::new("route-models");
    let source = format!(
        "{}[[routes]]\nmodel = \"z-last\"\nrouting = \"rotate\"\nupstreams = [\"chat\"]\n[[routes]]\nmodel = \"a-first\"\nupstreams = [\"chat\"]\n[[routes]]\nmodel = \"middle\"\nupstreams = [\"chat\"]\n",
        provider_config(&server.uri())
    );
    let running = start_tcp(&dir.0, &source).await;
    let response = support::client()
        .get(format!(
            "http://{}/v1/hofund/models",
            running.control_local_addr().expect("control")
        ))
        .bearer_auth(token(&dir.0))
        .send()
        .await
        .expect("route models");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let list: RouteModelListResponse = decode_response(
        API_VERSION,
        &response.bytes().await.expect("route model body"),
    )
    .expect("route model list");
    assert_eq!(list.models[0].routing, "rotate");
    assert_eq!(list.models[1].routing, "smart");
    assert_eq!(list.models[2].routing, "smart");
    let ids = list
        .models
        .into_iter()
        .map(|model| model.id)
        .collect::<Vec<_>>();
    assert_eq!(ids, ["z-last", "a-first", "middle"]);
    support::shutdown(running).await;
}

#[tokio::test]
async fn provider_ready_reflects_shared_cooldowns() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({"error": "down"})))
        .expect(1)
        .mount(&server)
        .await;
    let dir = Scratch::new("provider-cooldown");
    let source = format!(
        "{}[[routes]]\nmodel = \"demo\"\npatience = \"off\"\nupstreams = [\"chat\"]\n",
        provider_config(&server.uri())
    );
    let running = start_tcp(&dir.0, &source).await;
    let client = support::client();
    let token = token(&dir.0);
    let control = running.control_local_addr().expect("control");
    let response = client
        .post(format!("http://{}/v1/chat/completions", running.local_addr))
        .bearer_auth(&token)
        .json(&json!({"model": "demo", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .expect("failed model request");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::INTERNAL_SERVER_ERROR
    );
    let response = client
        .get(format!("http://{control}/v1/hofund/providers"))
        .bearer_auth(token)
        .send()
        .await
        .expect("providers");
    let list: ProviderListResponse =
        decode_response(API_VERSION, &response.bytes().await.expect("provider body"))
            .expect("provider list");
    assert!(!list.providers[0].ready);
    support::shutdown(running).await;
    server.verify().await;
}

#[cfg(unix)]
#[tokio::test]
async fn unix_shutdown_closes_a_control_request_that_outlasts_the_drain_bound() {
    use tokio::io::AsyncWriteExt;

    let dir = Scratch::new("unix-stuck-drain");
    let running = start_unix(&dir.0, "listen = \"127.0.0.1:0\"\n").await;
    let socket = running
        .control_socket_path()
        .expect("control socket")
        .to_path_buf();
    let mut stuck = tokio::net::UnixStream::connect(&socket)
        .await
        .expect("connect control socket");
    // A request whose body never finishes keeps the connection in flight,
    // so only the drain bound can end it.
    let head = format!(
        "POST /v1/hofund/config/validate HTTP/1.1\r\nHost: hofund-control\r\n\
         Authorization: Bearer {}\r\nContent-Type: application/json\r\n\
         Content-Length: 1000\r\n\r\n{{",
        token(&dir.0)
    );
    stuck.write_all(head.as_bytes()).await.expect("send head");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let started = std::time::Instant::now();
    tokio::time::timeout(std::time::Duration::from_secs(10), running.shutdown())
        .await
        .expect("shutdown is bounded with a stuck control request");
    assert!(
        started.elapsed() >= std::time::Duration::from_secs(3),
        "the stuck request is given the drain budget, not cut at once: {:?}",
        started.elapsed()
    );
    drop(stuck);
}
