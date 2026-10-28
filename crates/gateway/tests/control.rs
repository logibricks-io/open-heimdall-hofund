//! Loopback control routing and browser-request defenses.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use hofund_control_proto::{
    API_VERSION, ControlErrorKind, HealthResponse, StatusResponse, decode_error_envelope,
};
use hofund_gateway::control::{Control, ControlState};
use hofund_gateway::{Gateway, GatewayError};
use hofund_router::{FailoverPolicy, Router, Snapshot};
use hofund_usage::Ledger;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;

const LISTEN: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 3426);
const HOST: &str = "127.0.0.1:3426";
const TOKEN: &str = "control-test-token";

struct State;

impl ControlState for State {
    fn health(&self) -> HealthResponse {
        let mut health = HealthResponse::default();
        health.daemon_version = "test".to_owned();
        health
    }

    fn status(&self) -> StatusResponse {
        let mut status = StatusResponse::default();
        status.pid = 42;
        status
    }
}

fn control() -> Control {
    Control::new(Arc::new(State), TOKEN.to_owned())
}

fn app() -> axum::Router {
    control().into_router(LISTEN)
}

fn scratch(label: &str) -> std::path::PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "hofund-control-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("scratch directory");
    path
}

fn model_app(token: &str) -> axum::Router {
    let dir = scratch("model");
    let usage = Arc::new(Ledger::open(&dir.join("usage.sqlite")).expect("usage"));
    Gateway::new(
        Snapshot::new(Router::new(BTreeMap::new(), FailoverPolicy::default())),
        token.to_owned(),
        HashMap::new(),
        usage,
    )
    .into_router()
}

async fn call(
    app: axum::Router,
    method: Method,
    uri: &str,
    host: Option<&str>,
    headers: &[(&str, &str)],
    body: &str,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(host) = host {
        builder = builder.header(header::HOST, host);
    }
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let response = app
        .oneshot(builder.body(Body::from(body.to_owned())).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body")
        .to_vec();
    (status, headers, body)
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

#[tokio::test]
async fn model_health_reports_hofund_identity() {
    let (status, _, body) = call(model_app(TOKEN), Method::GET, "/health", None, &[], "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, br#"{"status":"ok","service":"hofund"}"#);
}

#[tokio::test]
async fn control_health_and_model_health_are_open() {
    let (status, headers, body) =
        call(app(), Method::GET, "/v1/hofund/health", Some(HOST), &[], "").await;
    assert_eq!(status, StatusCode::OK);
    let health: HealthResponse = serde_json::from_slice(&body).expect("health");
    assert_eq!(health.version, API_VERSION);
    assert_eq!(health.api_version, API_VERSION);
    assert!(!headers.contains_key("access-control-allow-origin"));

    let (status, _, _) = call(model_app(TOKEN), Method::GET, "/health", None, &[], "").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn control_routes_reject_missing_wrong_malformed_query_and_alternate_tokens() {
    for path in ["/v1/hofund/status", "/v1/hofund/unknown"] {
        let cases: Vec<(&str, Vec<(&str, &str)>)> = vec![
            ("missing", vec![]),
            ("wrong", vec![("authorization", "Bearer wrong")]),
            ("bare", vec![("authorization", "Bearer")]),
            (
                "scheme",
                vec![("authorization", "bearer  control-test-token")],
            ),
            (
                "spacing",
                vec![("authorization", "Bearer  control-test-token")],
            ),
            (
                "trailing",
                vec![("authorization", "Bearer control-test-token ")],
            ),
            ("api-key", vec![("x-api-key", TOKEN)]),
            ("alternate-api-key", vec![("x-goog-api-key", TOKEN)]),
            (
                "duplicate",
                vec![
                    ("authorization", "Bearer control-test-token"),
                    ("authorization", "Bearer control-test-token"),
                ],
            ),
        ];
        for (name, headers) in cases {
            let (status, _, body) = call(app(), Method::GET, path, Some(HOST), &headers, "").await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{path} {name}");
            let envelope = decode_error_envelope(API_VERSION, &body).expect("error envelope");
            assert_eq!(envelope.error.kind, ControlErrorKind::Unauthorized);
        }
        for query in ["key", "token"] {
            let uri = format!("{path}?{query}={TOKEN}");
            let authorization = bearer(TOKEN);
            let (status, _, _) = call(
                app(),
                Method::GET,
                &uri,
                Some(HOST),
                &[("authorization", &authorization)],
                "",
            )
            .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}");
        }

        let authorization = bearer(TOKEN);
        let (status, _, body) = call(
            app(),
            Method::GET,
            path,
            Some(HOST),
            &[("authorization", &authorization)],
            "",
        )
        .await;
        if path.ends_with("status") {
            assert_eq!(status, StatusCode::OK);
            let status: StatusResponse = serde_json::from_slice(&body).expect("status");
            assert_eq!(status.pid, 42);
        } else {
            assert_eq!(status, StatusCode::NOT_FOUND);
            let envelope = decode_error_envelope(API_VERSION, &body).expect("error envelope");
            assert_eq!(envelope.error.kind, ControlErrorKind::NotFound);
        }
    }
}

#[tokio::test]
async fn wrong_host_and_dns_rebinding_host_fail() {
    let authorization = bearer(TOKEN);
    for host in [
        Some("localhost:3426"),
        Some("evil.example:3426"),
        Some("127.0.0.1:9999"),
        None,
    ] {
        let (status, _, body) = call(
            app(),
            Method::GET,
            "/v1/hofund/status",
            host,
            &[("authorization", &authorization)],
            "",
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{host:?}");
        let envelope = decode_error_envelope(API_VERSION, &body).expect("error envelope");
        assert_eq!(envelope.error.kind, ControlErrorKind::Forbidden);
    }

    let (status, _, body) = call(
        app(),
        Method::GET,
        "/v1/hofund/status",
        Some(HOST),
        &[
            ("authorization", &authorization),
            ("host", "evil.example:3426"),
        ],
        "",
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "duplicate Host headers");
    let envelope = decode_error_envelope(API_VERSION, &body).expect("error envelope");
    assert_eq!(envelope.error.kind, ControlErrorKind::Forbidden);
}

#[tokio::test]
async fn ipv6_loopback_host_and_origin_exact_match() {
    let running = match control().bind("[::1]:0".parse().expect("address")).await {
        Ok(running) => running,
        Err(GatewayError::Bind { source, .. })
            if matches!(
                source.kind(),
                std::io::ErrorKind::AddrNotAvailable | std::io::ErrorKind::Unsupported
            ) =>
        {
            eprintln!("skipping IPv6 loopback test: {source}");
            return;
        }
        Err(error) => panic!("IPv6 loopback bind failed: {error}"),
    };
    let address = running.local_addr;
    let mut stream = tokio::net::TcpStream::connect(address)
        .await
        .expect("IPv6 connection");
    let request = format!(
        "GET /v1/hofund/status HTTP/1.1\r\nHost: {address}\r\nOrigin: http://{address}\r\nAuthorization: Bearer {TOKEN}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.expect("read");
    let text = String::from_utf8(response).expect("HTTP response");
    assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "{text}");
    assert!(text.contains("\"pid\":42"), "{text}");
    running.shutdown().await;
}

#[tokio::test]
async fn cross_site_origin_and_form_mutation_fail() {
    let authorization = bearer(TOKEN);
    let (status, headers, _) = call(
        app(),
        Method::POST,
        "/v1/hofund/status",
        Some(HOST),
        &[
            ("authorization", &authorization),
            ("origin", "https://evil.example"),
            ("content-type", "application/json"),
        ],
        "{}",
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(!headers.contains_key("access-control-allow-origin"));

    for content_type in ["application/x-www-form-urlencoded", "text/plain"] {
        let (status, _, body) = call(
            app(),
            Method::POST,
            "/v1/hofund/status",
            Some(HOST),
            &[
                ("authorization", &authorization),
                ("content-type", content_type),
            ],
            "value",
        )
        .await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{content_type}");
        let envelope = decode_error_envelope(API_VERSION, &body).expect("error envelope");
        assert_eq!(envelope.error.kind, ControlErrorKind::BadRequest);
    }

    let (status, _, body) = call(
        app(),
        Method::POST,
        "/v1/hofund/status",
        Some(HOST),
        &[
            ("authorization", &authorization),
            ("content-type", "application/json; charset=utf-8"),
        ],
        "{}",
    )
    .await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    let envelope = decode_error_envelope(API_VERSION, &body).expect("error envelope");
    assert_eq!(envelope.error.kind, ControlErrorKind::MethodNotAllowed);
}

#[tokio::test]
async fn control_bind_rejects_non_loopback_addresses() {
    for address in ["0.0.0.0:0", "[::]:0", "192.168.1.20:0"] {
        let Err(error) = control().bind(address.parse().expect("address")).await else {
            panic!("non-loopback bind succeeded");
        };
        assert!(matches!(error, GatewayError::NonLoopback { .. }));
        assert!(error.to_string().contains("numeric loopback"));
    }
    let result = control()
        .bind("127.0.0.2:0".parse().expect("address"))
        .await;
    assert!(
        !matches!(&result, Err(GatewayError::NonLoopback { .. })),
        "127.0.0.2 was refused by the loopback check"
    );
    #[cfg(not(target_os = "macos"))]
    assert!(result.is_ok(), "the full IPv4 loopback range is accepted");
}

#[tokio::test]
async fn token_rotation_takes_effect_on_next_control_request() {
    let dir = scratch("rotation");
    let token_path = dir.join("control.token");
    std::fs::write(&token_path, "first\n").expect("initial token");
    let app = Control::with_token_file(Arc::new(State), "first".to_owned(), token_path.clone())
        .into_router(LISTEN);
    let first = bearer("first");
    let (status, _, _) = call(
        app.clone(),
        Method::GET,
        "/v1/hofund/status",
        Some(HOST),
        &[("authorization", &first)],
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    std::fs::write(&token_path, "second\n").expect("rotate token");
    let (status, _, _) = call(
        app.clone(),
        Method::GET,
        "/v1/hofund/status",
        Some(HOST),
        &[("authorization", &first)],
        "",
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let second = bearer("second");
    let (status, _, _) = call(
        app.clone(),
        Method::GET,
        "/v1/hofund/status",
        Some(HOST),
        &[("authorization", &second)],
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    std::fs::write(&token_path, "\n").expect("empty token file");
    let (status, _, _) = call(
        app,
        Method::GET,
        "/v1/hofund/status",
        Some(HOST),
        &[("authorization", &second)],
        "",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "an empty file retains the last good token"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn control_token_never_in_debug_or_errors() {
    let secret = "never-print-this-control-token";
    let control = Control::new(Arc::new(State), secret.to_owned());
    assert!(!format!("{control:?}").contains(secret));
    let app = control.into_router(LISTEN);
    let (status, _, body) = call(
        app,
        Method::GET,
        "/v1/hofund/status",
        Some(HOST),
        &[("authorization", "Bearer wrong")],
        "",
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(!String::from_utf8_lossy(&body).contains(secret));
}

#[tokio::test]
async fn control_routes_are_not_on_model_listener() {
    let authorization = bearer(TOKEN);
    let (status, _, body) = call(
        model_app(TOKEN),
        Method::GET,
        "/v1/hofund/status",
        None,
        &[("authorization", &authorization)],
        "",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    assert!(body.get("pid").is_none());
    assert!(body.get("runtime_revision").is_none());
}
