//! Usage reports and graceful shutdown over the daemon control listener.

mod support;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hofund_control_proto::{
    API_VERSION, ControlErrorKind, ShutdownResponse, ShutdownState, StatusResponse, UsagePeriod,
    UsageResponse, decode_error_envelope, decode_response,
};
use hofund_daemon::{ControlRuntime, Running, StartOptions, start_with_options};
use hofund_usage::{Ledger, Row};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = PathBuf::from("/tmp").join(format!(
            "hofund-daemon-usage-{label}-{}-{}",
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
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const CONFIG: &str = "listen = \"127.0.0.1:0\"\nusage_file = \"usage.sqlite\"\n";

fn model_config(base_url: &str) -> String {
    format!(
        "listen = \"127.0.0.1:0\"\nusage_file = \"usage.sqlite\"\n[[providers]]\nid = \"primary\"\nprotocol = \"openai-chat\"\nbase_url = \"{base_url}\"\n[[routes]]\nmodel = \"demo\"\nupstreams = [\"primary\"]\n"
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

fn token(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("hofund.token"))
        .expect("token")
        .trim()
        .to_owned()
}

fn now_unix() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_secs(),
    )
    .expect("seconds")
}

fn row(created_unix: i64, model: &str, input: Option<i64>, output: Option<i64>) -> Row {
    Row {
        created_unix,
        model: model.to_owned(),
        upstream: "primary".to_owned(),
        input_tokens: input,
        output_tokens: output,
        latency_ms: 7,
        status: "200".to_owned(),
        caller_key_id: String::new(),
        caller_key_name: String::new(),
    }
}

fn control_url(running: &Running, route: &str) -> String {
    format!(
        "http://{}{route}",
        running.control_local_addr().expect("control listener")
    )
}

async fn usage(running: &Running, dir: &Path, query: &str) -> reqwest::Response {
    support::client()
        .get(control_url(running, &format!("/v1/hofund/usage{query}")))
        .bearer_auth(token(dir))
        .send()
        .await
        .expect("usage")
}

async fn post_json(running: &Running, dir: &Path, route: &str, body: &str) -> reqwest::Response {
    support::client()
        .post(control_url(running, route))
        .bearer_auth(token(dir))
        .header("content-type", "application/json")
        .body(body.to_owned())
        .send()
        .await
        .expect("control post")
}

async fn status(running: &Running, dir: &Path) -> StatusResponse {
    let response = support::client()
        .get(control_url(running, "/v1/hofund/status"))
        .bearer_auth(token(dir))
        .send()
        .await
        .expect("status");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    decode_response(API_VERSION, &response.bytes().await.expect("status body")).expect("status")
}

async fn decoded_usage(response: reqwest::Response) -> UsageResponse {
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    decode_response(API_VERSION, &response.bytes().await.expect("usage body")).expect("usage DTO")
}

#[tokio::test]
async fn usage_route_filters_period_and_totals() {
    let dir = Scratch::new("periods");
    let now = now_unix();
    {
        let ledger = Ledger::open(&dir.0.join("usage.sqlite")).expect("ledger");
        ledger
            .record(&row(now - 40 * 86_400, "ancient", Some(1_000), Some(1_000)))
            .expect("ancient");
        ledger
            .record(&row(now - 10 * 86_400, "older", Some(100), Some(100)))
            .expect("older");
        ledger
            .record(&row(now, "fresh", Some(-5), Some(3)))
            .expect("fresh");
        ledger
            .record(&row(now, "unreported", None, None))
            .expect("unreported");
    }
    let running = start_tcp(&dir.0, CONFIG).await;

    let all = decoded_usage(usage(&running, &dir.0, "").await).await;
    assert_eq!(all.period, UsagePeriod::All);
    assert_eq!(all.rows.len(), 4);
    assert!(!all.rows_truncated);
    assert_eq!(all.rows[0].model, "ancient");
    assert_eq!(
        all.rows[0].created_at_ms,
        u64::try_from(now - 40 * 86_400).expect("positive") * 1000
    );
    assert_eq!(all.rows[0].duration_ms, 7);
    assert_eq!(all.rows[0].provider, "primary");
    assert_eq!(all.rows[0].status, "200");
    assert_eq!(all.rows[2].input_tokens, Some(0), "negative counts clamp");
    assert_eq!(all.rows[3].input_tokens, None);
    assert_eq!(all.totals.requests, 4);
    assert_eq!(all.totals.input_tokens, 1_100);
    assert_eq!(all.totals.output_tokens, 1_103);

    let month = decoded_usage(usage(&running, &dir.0, "?period=month").await).await;
    assert_eq!(month.period, UsagePeriod::Month);
    assert_eq!(
        month
            .rows
            .iter()
            .map(|row| row.model.as_str())
            .collect::<Vec<_>>(),
        ["older", "fresh", "unreported"]
    );
    assert_eq!(month.totals.requests, 3);
    assert_eq!(month.totals.input_tokens, 100);
    assert_eq!(month.totals.output_tokens, 103);

    let week = decoded_usage(usage(&running, &dir.0, "?period=week").await).await;
    assert_eq!(week.period, UsagePeriod::Week);
    assert_eq!(week.totals.requests, 2);
    assert_eq!(week.totals.output_tokens, 3);

    let day = decoded_usage(usage(&running, &dir.0, "?period=day").await).await;
    assert_eq!(day.period, UsagePeriod::Day);
    assert_eq!(day.totals.requests, 2);
    assert_eq!(day.rows.len(), 2);
    support::shutdown(running).await;
}

#[tokio::test]
async fn usage_route_rejects_unknown_period_and_key() {
    let dir = Scratch::new("reject");
    let running = start_tcp(&dir.0, CONFIG).await;
    for query in ["?period=year", "?window=all", "?period=all&extra=1"] {
        let response = usage(&running, &dir.0, query).await;
        assert_eq!(
            response.status(),
            reqwest::StatusCode::BAD_REQUEST,
            "{query}"
        );
        let envelope =
            decode_error_envelope(API_VERSION, &response.bytes().await.expect("error body"))
                .expect("envelope");
        assert_eq!(envelope.error.kind, ControlErrorKind::BadRequest, "{query}");
    }
    support::shutdown(running).await;
}

#[tokio::test]
async fn usage_row_cap_keeps_totals_complete() {
    let dir = Scratch::new("cap");
    let now = now_unix();
    {
        let ledger = Ledger::open(&dir.0.join("usage.sqlite")).expect("ledger");
        for index in 0..10_003_i64 {
            ledger
                .record(&row(
                    now - 10_003 + index,
                    &format!("m{index}"),
                    Some(1),
                    Some(2),
                ))
                .expect("row");
        }
    }
    let running = start_tcp(&dir.0, CONFIG).await;
    let all = decoded_usage(usage(&running, &dir.0, "?period=all").await).await;
    assert_eq!(all.rows.len(), 10_000);
    assert!(all.rows_truncated);
    assert_eq!(all.rows[0].model, "m3", "the oldest rows are dropped");
    assert_eq!(all.rows[9_999].model, "m10002");
    assert_eq!(all.totals.requests, 10_003);
    assert_eq!(all.totals.input_tokens, 10_003);
    assert_eq!(all.totals.output_tokens, 20_006);
    support::shutdown(running).await;
}

#[cfg(unix)]
#[tokio::test]
async fn shutdown_route_drains_inflight_model_request() {
    use std::os::unix::fs::PermissionsExt;

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(
                    r#"{"choices":[{"index":0,"message":{"role":"assistant","content":"drained"},"finish_reason":"stop"}]}"#,
                    "application/json",
                )
                .set_delay(Duration::from_millis(1_500)),
        )
        .mount(&server)
        .await;
    let dir = Scratch::new("drain");
    let config = dir.0.join("hofund.toml");
    std::fs::write(&config, model_config(&server.uri())).expect("config");
    let runtime = dir.0.join("runtime");
    std::fs::create_dir_all(&runtime).expect("runtime directory");
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))
        .expect("runtime permissions");
    let options = StartOptions {
        control_enabled: true,
        control_runtime: Some(ControlRuntime::new(Some(runtime), dir.0.clone())),
        ..StartOptions::default()
    };
    let running = start_with_options(&config, options.clone())
        .await
        .expect("daemon");
    let model_addr = running.local_addr;
    let socket = running
        .control_socket_path()
        .expect("Unix socket")
        .to_path_buf();
    let token = token(&dir.0);
    let served = tokio::spawn(hofund_daemon::serve_until_shutdown(running));

    let model_token = token.clone();
    let inflight = tokio::spawn(async move {
        let response = support::client()
            .post(format!("http://{model_addr}/v1/chat/completions"))
            .bearer_auth(model_token)
            .body(r#"{"model":"demo","messages":[{"role":"user","content":"hi"}]}"#)
            .send()
            .await
            .expect("model request");
        (
            response.status(),
            response.text().await.expect("model body"),
        )
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while server
        .received_requests()
        .await
        .unwrap_or_default()
        .is_empty()
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "upstream never saw the request"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let control = support::builder()
        .unix_socket(socket.clone())
        .build()
        .expect("Unix client");
    let response = control
        .post("http://hofund-control/v1/hofund/shutdown")
        .bearer_auth(&token)
        .header("content-type", "application/json")
        .body("{\"version\":1}")
        .send()
        .await
        .expect("shutdown");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let ack: ShutdownResponse =
        decode_response(API_VERSION, &response.bytes().await.expect("ack body")).expect("ack");
    assert!(ack.draining);

    let (status, body) = tokio::time::timeout(Duration::from_secs(10), inflight)
        .await
        .expect("in-flight request finished")
        .expect("in-flight task");
    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(body.contains("drained"), "{body}");
    tokio::time::timeout(Duration::from_secs(10), served)
        .await
        .expect("serve returned")
        .expect("serve task")
        .expect("a requested shutdown is not fatal");

    assert!(
        tokio::net::TcpStream::connect(model_addr).await.is_err(),
        "the model port still accepts connections"
    );
    assert!(!socket.exists(), "the control socket was not removed");
    let again = start_with_options(&config, options)
        .await
        .expect("the daemon lock was released");
    support::shutdown(again).await;
}

#[tokio::test]
async fn shutdown_is_idempotent_and_status_reports_draining() {
    let dir = Scratch::new("idempotent");
    let running = start_tcp(&dir.0, CONFIG).await;
    assert_eq!(
        status(&running, &dir.0).await.shutdown,
        ShutdownState::Running
    );
    for _ in 0..2 {
        let response = post_json(&running, &dir.0, "/v1/hofund/shutdown", "{\"version\":1}").await;
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let ack: ShutdownResponse =
            decode_response(API_VERSION, &response.bytes().await.expect("ack body")).expect("ack");
        assert!(ack.draining);
    }
    assert_eq!(
        status(&running, &dir.0).await.shutdown,
        ShutdownState::Draining
    );
    support::shutdown(running).await;
}

#[tokio::test]
async fn mutations_after_shutdown_return_503() {
    let dir = Scratch::new("mutations");
    let running = start_tcp(&dir.0, CONFIG).await;
    let response = post_json(&running, &dir.0, "/v1/hofund/shutdown", "{\"version\":1}").await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    for route in [
        "/v1/hofund/token/rotate",
        "/v1/hofund/agents/example/connect",
        "/v1/hofund/providers/primary/models/refresh",
    ] {
        let response = post_json(&running, &dir.0, route, "{\"version\":1}").await;
        assert_eq!(
            response.status(),
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            "{route}"
        );
        let envelope =
            decode_error_envelope(API_VERSION, &response.bytes().await.expect("error body"))
                .expect("envelope");
        assert_eq!(
            envelope.error.kind,
            ControlErrorKind::Unavailable,
            "{route}"
        );
        assert!(envelope.error.message.contains("draining"), "{route}");
    }
    let usage = usage(&running, &dir.0, "").await;
    assert_eq!(usage.status(), reqwest::StatusCode::OK, "reads still work");
    support::shutdown(running).await;
}
