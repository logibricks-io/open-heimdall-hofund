//! Control and runtime tests for one-card quota refresh.

mod support;

use std::path::Path;

use hofund_control_proto::{
    API_VERSION, QuotaRefreshRequest, QuotaRefreshResponse, decode_response,
};
use hofund_daemon::{ControlRuntime, Running, StartOptions, start_with_options};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct Scratch(tempfile::TempDir);

impl Scratch {
    fn new(label: &str) -> Self {
        Self(
            tempfile::Builder::new()
                .prefix(&format!("hofund-daemon-quota-refresh-{label}-"))
                .tempdir()
                .expect("scratch directory"),
        )
    }

    fn path(&self) -> &Path {
        self.0.path()
    }
}

async fn start(dir: &Path, text: &str) -> Running {
    let config = dir.join("hofund.toml");
    std::fs::write(&config, text).expect("config");
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

fn token(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("hofund.token"))
        .expect("token")
        .trim()
        .to_owned()
}

fn url(running: &Running, path: &str) -> String {
    format!(
        "http://{}{path}",
        running.control_local_addr().expect("control")
    )
}

async fn refresh(
    dir: &Path,
    running: &Running,
    provider: &str,
    user: &str,
) -> QuotaRefreshResponse {
    let response = support::client()
        .post(url(running, "/v1/hofund/quotas/refresh"))
        .bearer_auth(token(dir))
        .json(&QuotaRefreshRequest::new(provider, user))
        .send()
        .await
        .expect("POST refresh");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    decode_response(API_VERSION, &response.bytes().await.expect("body")).expect("refresh response")
}

#[tokio::test]
async fn d38_1_refresh_reads_one_balance_card_and_honours_gap() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/panel/a"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"usd":7}"#))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/panel/b"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"usd":9}"#))
        .mount(&server)
        .await;
    let scratch = Scratch::new("one-card");
    let dir = scratch.path();
    let text = format!(
        "listen = \"127.0.0.1:0\"\n[[providers]]\nid = \"a\"\npreset = \"relay\"\nbase_url = \"http://127.0.0.1:9\"\napi_key = \"alpha\"\nkey_name = \"one\"\n[providers.balance]\nurl = \"{}/panel/a\"\npath = \"$usd\"\n[[providers]]\nid = \"b\"\npreset = \"relay\"\nbase_url = \"http://127.0.0.1:9\"\napi_key = \"beta\"\nkey_name = \"two\"\n[providers.balance]\nurl = \"{}/panel/b\"\npath = \"$usd\"\n[[routes]]\nmodel = \"demo\"\nupstreams = [\"a\"]\n",
        server.uri(),
        server.uri()
    );
    let running = start(dir, &text).await;
    let first = refresh(dir, &running, "a", "").await;
    assert_eq!(first.data.len(), 1);
    assert_eq!(first.data[0].provider, "a");
    assert_eq!(first.data[0].balance, "$7.00");
    assert!(!first.throttled);
    assert_eq!(
        server.received_requests().await.unwrap_or_default().len(),
        1
    );

    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/panel/a"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"usd":6}"#))
        .mount(&server)
        .await;

    let second = refresh(dir, &running, "a", "").await;
    assert!(second.throttled);
    assert_eq!(second.data[0].balance, "$7.00");
    assert!(
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty()
    );
}

#[tokio::test]
async fn d38_2_refresh_one_named_key_on_a_provider() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/shared"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"amount":10}"#))
        .mount(&server)
        .await;
    let scratch = Scratch::new("named-key");
    let dir = scratch.path();
    let url = format!("{}/shared", server.uri());
    let text = format!(
        "listen = \"127.0.0.1:0\"\n[[providers]]\nid = \"relay\"\npreset = \"relay\"\nbase_url = \"http://127.0.0.1:9\"\napi_key = \"alpha\"\nkey_name = \"one\"\n[providers.balance]\nurl = \"{url}\"\npath = \"$amount\"\n[[providers.keys]]\nkey = \"beta\"\nname = \"two\"\n[[routes]]\nmodel = \"demo\"\nupstreams = [\"relay\"]\n"
    );
    let running = start(dir, &text).await;
    let _ = refresh(dir, &running, "relay", "").await;
    assert_eq!(
        server.received_requests().await.unwrap_or_default().len(),
        2
    );
    let one = refresh(dir, &running, "relay", "two").await;
    assert_eq!(one.data.len(), 1);
    assert_eq!(one.data[0].alias, "two");
    assert_eq!(
        server.received_requests().await.unwrap_or_default().len(),
        3
    );
}

#[tokio::test]
async fn d38_3_unknown_provider_returns_no_cards() {
    let scratch = Scratch::new("missing");
    let dir = scratch.path();
    let running = start(dir, "listen = \"127.0.0.1:0\"\n").await;
    let response = refresh(dir, &running, "missing", "").await;
    assert!(response.data.is_empty());
    assert!(!response.throttled);
}
