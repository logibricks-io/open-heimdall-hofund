//! Typed control calls against an in-process TCP server.

use std::convert::Infallible;

use hofund_control_client::{Client, ClientError};
use http_body_util::Full;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

const TOKEN: &str = "round-trip-control-token";
const HEALTH: &[u8] = br#"{"version":1,"api_version":1,"daemon_version":"test"}"#;
const STATUS: &[u8] = br#"{"version":1,"pid":7,"started_at_ms":123,"model_listener":{"configured":"127.0.0.1:3425","effective":"127.0.0.1:3425","running":"127.0.0.1:3425","effective_source":"config"},"generation":1,"disk_revision":"disk","runtime_revision":"runtime","pending_restart":[],"not_applied":[],"in_flight":0,"shutdown":"running"}"#;
const UNAUTHORIZED: &[u8] = br#"{"version":1,"error":{"kind":"unauthorized","message":"unauthorized","request_id":"test","retry_after_ms":null,"details":[]}}"#;

#[tokio::test]
async fn client_health_and_status_round_trip_over_tcp() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind server");
    let addr = listener.local_addr().expect("server address");
    let server = tokio::spawn(async move {
        for _ in 0..3 {
            let (stream, _) = listener.accept().await.expect("accept");
            tokio::spawn(async move {
                let service = service_fn(
                    move |request: http::Request<hyper::body::Incoming>| async move {
                        assert_eq!(
                            request
                                .headers()
                                .get(http::header::HOST)
                                .and_then(|value| value.to_str().ok()),
                            Some(addr.to_string().as_str())
                        );
                        let authorized = request
                            .headers()
                            .get(http::header::AUTHORIZATION)
                            .and_then(|value| value.to_str().ok())
                            == Some(format!("Bearer {TOKEN}").as_str());
                        let (status, body) = if !authorized {
                            (http::StatusCode::UNAUTHORIZED, UNAUTHORIZED)
                        } else if request.uri().path() == "/v1/hofund/health" {
                            (http::StatusCode::OK, HEALTH)
                        } else if request.uri().path() == "/v1/hofund/status" {
                            (http::StatusCode::OK, STATUS)
                        } else {
                            panic!("unexpected control path")
                        };
                        let response = http::Response::builder()
                            .status(status)
                            .header(http::header::CONTENT_TYPE, "application/json")
                            .body(Full::new(body))
                            .expect("response");
                        Ok::<_, Infallible>(response)
                    },
                );
                hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await
                    .expect("serve connection");
            });
        }
    });

    let client = Client::tcp(addr, TOKEN);
    assert!(!format!("{client:?}").contains(TOKEN));
    let health = client.health().await.expect("health");
    assert_eq!(health.daemon_version, "test");
    let status = client.status().await.expect("status");
    assert_eq!(status.pid, 7);
    assert_eq!(status.runtime_revision, "runtime");

    let denied = Client::tcp(addr, "wrong-token").health().await;
    let Err(ClientError::Server { status, error }) = denied else {
        panic!("wrong token did not return a typed server error");
    };
    assert_eq!(status, http::StatusCode::UNAUTHORIZED);
    assert_eq!(
        error.error.kind,
        hofund_control_client::hofund_control_proto::ControlErrorKind::Unauthorized
    );
    server.await.expect("server task");
}
