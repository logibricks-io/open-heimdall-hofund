#![cfg(unix)]

//! Unix transport and ordered discovery tests.

use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use hofund_control_client::{Client, ClientError, DiscoveryOptions};
use http_body_util::Full;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use sha2::{Digest, Sha256};
use tokio::net::UnixListener;

const TOKEN: &str = "unix-round-trip-control-token";
const HEALTH: &[u8] = br#"{"version":1,"api_version":1,"daemon_version":"test"}"#;

fn scratch(label: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "hofund-client-unix-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("scratch directory");
    path
}

fn spawn_server(socket: &Path, requests: usize) -> tokio::task::JoinHandle<()> {
    use std::os::unix::fs::PermissionsExt;
    let listener = UnixListener::bind(socket).expect("bind Unix server");
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))
        .expect("socket permissions");
    tokio::spawn(async move {
        for _ in 0..requests {
            let (stream, _) = listener.accept().await.expect("accept");
            tokio::spawn(async move {
                let service = service_fn(
                    move |request: http::Request<hyper::body::Incoming>| async move {
                        assert_eq!(
                            request
                                .headers()
                                .get(http::header::HOST)
                                .and_then(|value| value.to_str().ok()),
                            Some("hofund-control")
                        );
                        assert_eq!(
                            request
                                .headers()
                                .get(http::header::AUTHORIZATION)
                                .and_then(|value| value.to_str().ok()),
                            Some(format!("Bearer {TOKEN}").as_str())
                        );
                        assert_eq!(request.uri().path(), "/v1/hofund/health");
                        Ok::<_, Infallible>(
                            http::Response::builder()
                                .header(http::header::CONTENT_TYPE, "application/json")
                                .body(Full::new(HEALTH))
                                .expect("response"),
                        )
                    },
                );
                hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await
                    .expect("serve connection");
            });
        }
    })
}

fn config_id(path: &Path) -> String {
    let resolved = std::fs::canonicalize(path).expect("canonical config");
    let digest = Sha256::digest(resolved.to_str().expect("UTF-8 path").as_bytes());
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[tokio::test]
async fn client_health_round_trip_over_unix_socket() {
    let dir = scratch("round-trip");
    let socket = dir.join("control.sock");
    let server = spawn_server(&socket, 1);
    let client = Client::unix(&socket, TOKEN);
    assert!(!format!("{client:?}").contains(TOKEN));
    let health = client.health().await.expect("health");
    assert_eq!(health.daemon_version, "test");
    server.await.expect("server task");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn discovery_falls_back_from_missing_socket_to_runtime_discovery() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("discovery");
    let config = dir.join("hofund.toml");
    std::fs::write(&config, "listen = \"127.0.0.1:0\"\n").expect("config");
    let id = config_id(&config);
    let state = dir.join(".hofund");
    std::fs::create_dir(&state).expect("state directory");
    let missing = dir.join("missing.sock");
    std::fs::write(
        state.join(format!("control-{id}.json")),
        serde_json::to_vec(&serde_json::json!({
            "api_version": 1,
            "pid": 7,
            "boot_id": "boot",
            "config_id": id,
            "tcp_endpoints": [],
            "socket_path": missing,
        }))
        .expect("discovery JSON"),
    )
    .expect("primary discovery");

    let xdg = dir.join("runtime");
    let runtime = xdg.join("hofund");
    std::fs::create_dir_all(&runtime).expect("runtime directory");
    std::fs::set_permissions(&xdg, std::fs::Permissions::from_mode(0o700))
        .expect("runtime base permissions");
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))
        .expect("runtime permissions");
    let socket = runtime.join("live.sock");
    let server = spawn_server(&socket, 1);
    std::fs::write(
        runtime.join(format!("control-{id}.json")),
        serde_json::to_vec(&serde_json::json!({
            "api_version": 1,
            "pid": 7,
            "boot_id": "boot",
            "config_id": id,
            "tcp_endpoints": [],
            "socket_path": socket,
        }))
        .expect("discovery JSON"),
    )
    .expect("runtime discovery");

    let client = Client::discover(
        &config,
        TOKEN,
        DiscoveryOptions::isolated(Some(xdg), dir.clone()),
    )
    .await
    .expect("discovered client");
    assert!(!format!("{client:?}").contains(TOKEN));
    server.await.expect("server task");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn discovery_wrong_version_does_not_fall_back_to_tcp() {
    let dir = scratch("wrong-version");
    let config = dir.join("hofund.toml");
    std::fs::write(&config, "listen = \"127.0.0.1:0\"\n").expect("config");
    let id = config_id(&config);
    let state = dir.join(".hofund");
    std::fs::create_dir(&state).expect("state directory");
    std::fs::write(
        state.join(format!("control-{id}.json")),
        serde_json::to_vec(&serde_json::json!({
            "api_version": 2,
            "pid": 7,
            "boot_id": "boot",
            "config_id": id,
            "tcp_endpoints": ["127.0.0.1:9"],
            "socket_path": null,
        }))
        .expect("discovery JSON"),
    )
    .expect("discovery");
    let mut options = DiscoveryOptions::isolated(None, dir.clone());
    options.control_listen = Some("127.0.0.1:9".parse().expect("TCP address"));
    let error = Client::discover(&config, TOKEN, options)
        .await
        .expect_err("wrong discovery version");
    assert!(matches!(error, ClientError::Discovery(_)), "{error}");
    let _ = std::fs::remove_dir_all(dir);
}
