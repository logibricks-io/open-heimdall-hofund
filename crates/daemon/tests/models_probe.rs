//! Host probe selection and batching against local mock upstreams.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use hofund_daemon::ModelsHost;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Match, Mock, MockServer, Request, ResponseTemplate};

fn load_config(source: &str) -> hofund_config::Config {
    hofund_config::load(source).expect("config")
}

fn config_path(test: &str) -> PathBuf {
    std::env::temp_dir()
        .join(format!("hofund-probes-{test}-{}", std::process::id()))
        .join("hofund.toml")
}

fn make_host() -> ModelsHost {
    ModelsHost::new(Duration::from_secs(2), Duration::from_secs(2))
}

#[derive(Clone, Copy)]
enum AuthMode {
    Both,
    XApiOnly,
    BearerOnly,
}

impl Match for AuthMode {
    fn matches(&self, request: &Request) -> bool {
        let x_api = request.headers.contains_key("x-api-key");
        let bearer = request
            .headers
            .get("authorization")
            .is_some_and(|value| value == "Bearer test-key");
        match self {
            Self::Both => x_api && bearer,
            Self::XApiOnly => x_api && !request.headers.contains_key("authorization"),
            Self::BearerOnly => bearer && !x_api,
        }
    }
}

struct ModelBody(&'static str);

impl Match for ModelBody {
    fn matches(&self, request: &Request) -> bool {
        serde_json::from_slice::<serde_json::Value>(&request.body)
            .ok()
            .and_then(|body| {
                body.get("model")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
            .as_deref()
            == Some(self.0)
    }
}

#[tokio::test]
async fn selector_prefers_exposed_upstream_name_then_endpoint_and_second_pass() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                {"id": "gpt-image-1"},
                {"id": "gemini-2.5-flash-image"},
                {"id": "response-only", "supported_endpoints": ["responses"]},
                {"id": "chat-good", "supported_endpoints": ["chat"]}
            ]
        })))
        .expect(3)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(ModelBody("chat-good"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"choices": []})))
        .expect(3)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(ModelBody("gemini-2.5-flash-image"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"choices": []})))
        .expect(0)
        .mount(&server)
        .await;
    let config = load_config(&format!(
        "[[providers]]\nid = \"chat\"\nprotocol = \"openai-chat\"\nbase_url = \"{}/v1\"\napi_key = \"test-key\"\n[[routes]]\nmodel = \"alias\"\nupstreams = [{{ provider = \"chat\", model = \"chat-good\" }}]\n",
        server.uri()
    ));
    let host = make_host();
    let exposed = host
        .probe(&config, &config_path("selection"), "chat", None)
        .await
        .expect("exposed model");
    assert_eq!(exposed.model, "alias");
    let without_routes = load_config(&format!(
        "[[providers]]\nid = \"chat\"\nprotocol = \"openai-chat\"\nbase_url = \"{}/v1\"\napi_key = \"test-key\"\n",
        server.uri()
    ));
    let endpoint = host
        .probe(&config, &config_path("selection"), "chat", None)
        .await
        .expect("endpoint preference");
    assert_eq!(endpoint.model, "alias");
    let listed = host
        .probe(&without_routes, &config_path("selection"), "chat", None)
        .await
        .expect("listed endpoint preference");
    assert_eq!(listed.model, "chat-good");
    server.verify().await;
}

#[tokio::test]
async fn gemini_flash_image_suffix_is_not_an_images_api_id() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "gemini-2.5-flash-image"}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(ModelBody("gemini-2.5-flash-image"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"choices": []})))
        .expect(1)
        .mount(&server)
        .await;
    let config = load_config(&format!(
        "[[providers]]\nid = \"chat\"\nprotocol = \"openai-chat\"\nbase_url = \"{}/v1\"\napi_key = \"test-key\"\n",
        server.uri()
    ));
    let result = make_host()
        .probe(&config, &config_path("gemini-image"), "chat", None)
        .await
        .expect("chat image suffix");
    assert_eq!(result.model, "gemini-2.5-flash-image");
}

#[tokio::test]
async fn second_pass_accepts_an_image_only_list() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "gpt-image-1"}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(ModelBody("gpt-image-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"choices": []})))
        .expect(1)
        .mount(&server)
        .await;
    let config = load_config(&format!(
        "[[providers]]\nid = \"chat\"\nprotocol = \"openai-chat\"\nbase_url = \"{}/v1\"\napi_key = \"test-key\"\n",
        server.uri()
    ));
    let result = make_host()
        .probe(&config, &config_path("second-pass"), "chat", None)
        .await
        .expect("second pass");
    assert_eq!(result.model, "gpt-image-1");
}

#[tokio::test]
async fn named_wrong_protocol_and_decide_only_selection_send_nothing() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "response-only", "supported_endpoints": ["responses"]}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let config = load_config(&format!(
        "[[providers]]\nid = \"chat\"\nprotocol = \"openai-chat\"\nbase_url = \"{}/v1\"\napi_key = \"test-key\"\n",
        server.uri()
    ));
    let host = make_host();
    let error = host
        .probe(
            &config,
            &config_path("no-send"),
            "chat",
            Some("response-only"),
        )
        .await
        .expect_err("wrong protocol");
    assert!(
        error.to_string().contains("not served on this protocol"),
        "{error}"
    );
    server.verify().await;

    let decide = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "decider", "kind": "decision"}]
        })))
        .expect(1)
        .mount(&decide)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&decide)
        .await;
    let config = load_config(&format!(
        "[[providers]]\nid = \"chat\"\nprotocol = \"openai-chat\"\nbase_url = \"{}/v1\"\napi_key = \"test-key\"\n",
        decide.uri()
    ));
    let error = host
        .probe(&config, &config_path("no-send"), "chat", None)
        .await
        .expect_err("decide only");
    assert_eq!(
        error.to_string(),
        "no model to try: expose one, or refresh the model list"
    );
    decide.verify().await;
}

#[tokio::test]
async fn learned_anthropic_auth_is_reused_and_not_shared_with_chat() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(AuthMode::Both)
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string("x-api-key and authorization cannot both be present"),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(AuthMode::XApiOnly)
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "vendor/claude-sonnet"}]
        })))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(AuthMode::XApiOnly)
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"content": []})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/chat/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "gpt"}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/chat/completions"))
        .and(AuthMode::BearerOnly)
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"choices": []})))
        .expect(1)
        .mount(&server)
        .await;
    let config = load_config(&format!(
        "[[providers]]\nid = \"messages\"\nprotocol = \"anthropic-messages\"\nbase_url = \"{}\"\napi_key = \"test-key\"\n[[providers]]\nid = \"chat\"\nprotocol = \"openai-chat\"\nbase_url = \"{}/chat\"\napi_key = \"test-key\"\n",
        server.uri(),
        server.uri()
    ));
    let host = make_host();
    host.list_models(&config, &config_path("learned"), "messages")
        .await
        .expect("learn auth");
    let probed = host
        .probe(&config, &config_path("learned"), "messages", None)
        .await
        .expect("Anthropic probe");
    assert_eq!(probed.model, "vendor/claude-sonnet");
    host.probe(&config, &config_path("learned"), "chat", None)
        .await
        .expect("chat stays bearer");
    server.verify().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn batch_probe_runs_at_most_four_concurrently_and_preserves_order() {
    let names = ["m0", "m1", "m2", "m3", "m4", "m5", "m6", "m7"];
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let posts = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind batch mock");
    let address = listener.local_addr().expect("batch mock address");
    let server = tokio::spawn({
        let active = Arc::clone(&active);
        let maximum = Arc::clone(&maximum);
        let posts = Arc::clone(&posts);
        async move {
            loop {
                let (socket, _) = listener.accept().await.expect("accept");
                tokio::spawn(batch_connection(
                    socket,
                    Arc::clone(&active),
                    Arc::clone(&maximum),
                    Arc::clone(&posts),
                ));
            }
        }
    });
    let config = load_config(&format!(
        "[[providers]]\nid = \"chat\"\nprotocol = \"openai-chat\"\nbase_url = \"http://{address}/v1\"\napi_key = \"test-key\"\n"
    ));
    let results = make_host()
        .probe_many(&config, &config_path("batch"), "chat", names.to_vec())
        .await
        .expect("batch selection");
    let completed = results
        .into_iter()
        .map(|result| result.expect("probe").model)
        .collect::<Vec<_>>();
    assert_eq!(completed, names);
    assert_eq!(maximum.load(Ordering::SeqCst), 4);
    assert_eq!(active.load(Ordering::SeqCst), 0);
    assert_eq!(posts.load(Ordering::SeqCst), 8);
    server.abort();
}

async fn batch_connection(
    mut socket: tokio::net::TcpStream,
    active: Arc<AtomicUsize>,
    maximum: Arc<AtomicUsize>,
    posts: Arc<AtomicUsize>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut buffered = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let Some(total) = complete_request(&buffered) else {
            match socket.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(read) => buffered.extend_from_slice(&chunk[..read]),
            }
            continue;
        };
        let request = buffered.drain(..total).collect::<Vec<_>>();
        let post = request.starts_with(b"POST ");
        if post {
            let in_flight = active.fetch_add(1, Ordering::SeqCst) + 1;
            maximum.fetch_max(in_flight, Ordering::SeqCst);
            posts.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(60)).await;
            active.fetch_sub(1, Ordering::SeqCst);
        }
        let body = if post {
            br#"{"choices":[]}"#.as_slice()
        } else {
            br#"{"data":[{"id":"m0"},{"id":"m1"},{"id":"m2"},{"id":"m3"},{"id":"m4"},{"id":"m5"},{"id":"m6"},{"id":"m7"}]}"#.as_slice()
        };
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        if socket.write_all(head.as_bytes()).await.is_err() || socket.write_all(body).await.is_err()
        {
            return;
        }
    }
}

fn complete_request(request: &[u8]) -> Option<usize> {
    let header_end = request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")?;
    let headers = String::from_utf8_lossy(&request[..header_end]);
    let length = headers
        .split("\r\n")
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    let total = header_end + 4 + length;
    (request.len() >= total).then_some(total)
}
