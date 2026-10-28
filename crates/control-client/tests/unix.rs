#![cfg(unix)]

//! Unix transport and ordered discovery tests.

use std::convert::Infallible;
use std::path::{Path, PathBuf};

use hofund_control_client::{Client, ClientError, DiscoveryOptions};
use http_body_util::Full;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use sha2::{Digest, Sha256};
use tokio::net::UnixListener;

const TOKEN: &str = "unix-round-trip-control-token";
const HEALTH: &[u8] = br#"{"version":1,"api_version":1,"daemon_version":"test"}"#;
const HEALTH_MISMATCH: &[u8] = br#"{"version":1,"api_version":2,"daemon_version":"test"}"#;
const STATUS: &[u8] = br#"{"version":1,"pid":7,"started_at_ms":123,"model_listener":{"configured":"127.0.0.1:3425","effective":"127.0.0.1:3425","running":"127.0.0.1:3425","effective_source":"config"},"generation":1,"disk_revision":"disk","runtime_revision":"runtime","pending_restart":[],"not_applied":[],"in_flight":0,"shutdown":"running"}"#;
const UNAUTHORIZED: &[u8] = br#"{"version":1,"error":{"kind":"unauthorized","message":"unauthorized","request_id":"test","retry_after_ms":null,"details":[]}}"#;

struct Scratch(tempfile::TempDir);

impl Scratch {
    fn new(label: &str) -> Self {
        // A short `/tmp` root keeps Unix socket paths under the macOS limit.
        let dir = tempfile::Builder::new()
            .prefix(&format!("hofund-client-{label}-"))
            .tempdir_in("/tmp")
            .expect("scratch directory");
        let socket = dir.path().join("control.sock");
        assert!(
            socket.as_os_str().as_encoded_bytes().len() < 100,
            "socket path must be shorter than 100 bytes: {}",
            socket.display()
        );
        Self(dir)
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn join(&self, path: &str) -> PathBuf {
        self.path().join(path)
    }
}

fn spawn_server(socket: &Path, requests: usize) -> tokio::task::JoinHandle<()> {
    spawn_server_with_health(socket, requests, HEALTH)
}

fn spawn_server_with_health(
    socket: &Path,
    requests: usize,
    health: &'static [u8],
) -> tokio::task::JoinHandle<()> {
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
                        let authorized = request
                            .headers()
                            .get(http::header::AUTHORIZATION)
                            .and_then(|value| value.to_str().ok())
                            == Some(format!("Bearer {TOKEN}").as_str());
                        let path = request.uri().path();
                        let (status, body) = if path == "/v1/hofund/health" {
                            (http::StatusCode::OK, health)
                        } else if path == "/v1/hofund/status" && authorized {
                            (http::StatusCode::OK, STATUS)
                        } else if path == "/v1/hofund/status" {
                            (http::StatusCode::UNAUTHORIZED, UNAUTHORIZED)
                        } else {
                            panic!("unexpected control path")
                        };
                        Ok::<_, Infallible>(
                            http::Response::builder()
                                .status(status)
                                .header(http::header::CONTENT_TYPE, "application/json")
                                .body(Full::new(body))
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
    let text = resolved.to_str().expect("UTF-8 path");
    // Discovery lowercases the canonical path on macOS; random scratch names
    // contain uppercase letters, so mirror that normalization here.
    #[cfg(target_os = "macos")]
    let text = text.to_lowercase();
    #[cfg(not(target_os = "macos"))]
    let text = text.to_owned();
    let digest = Sha256::digest(text.as_bytes());
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn write_config(dir: &Scratch) -> (PathBuf, String) {
    let config = dir.join("hofund.toml");
    std::fs::write(&config, "listen = \"127.0.0.1:0\"\n").expect("config");
    let id = config_id(&config);
    let state = dir.join(".hofund");
    std::fs::create_dir(&state).expect("state directory");
    (config, id)
}

fn write_socket_discovery(dir: &Scratch, id: &str, socket: &Path) {
    std::fs::write(
        dir.join(".hofund").join(format!("control-{id}.json")),
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
    .expect("discovery");
}

#[tokio::test]
async fn client_health_round_trip_over_unix_socket() {
    let dir = Scratch::new("round-trip");
    let socket = dir.join("control.sock");
    let server = spawn_server(&socket, 1);
    let client = Client::unix(&socket, TOKEN);
    assert!(!format!("{client:?}").contains(TOKEN));
    let health = client.health().await.expect("health");
    assert_eq!(health.daemon_version, "test");
    server.await.expect("server task");
}

#[tokio::test]
async fn client_refuses_regular_file_socket_before_request() {
    let dir = Scratch::new("regular-file");
    let socket = dir.join("control.sock");
    std::fs::write(&socket, "not a socket").expect("regular file");
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        Client::unix(&socket, TOKEN).health(),
    )
    .await
    .expect("client must refuse before waiting on the server")
    .expect_err("regular file");
    assert!(matches!(error, ClientError::Discovery(_)), "{error}");
    assert_eq!(
        std::fs::read_to_string(socket).expect("file"),
        "not a socket"
    );
}

#[tokio::test]
async fn client_refuses_symlinked_socket_without_sending_request() {
    let dir = Scratch::new("socket-symlink");
    let socket = dir.join("control.sock");
    let listener = UnixListener::bind(&socket).expect("Unix listener");
    let link = dir.join("linked.sock");
    std::os::unix::fs::symlink(&socket, &link).expect("socket symlink");
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        Client::unix(&link, TOKEN).health(),
    )
    .await
    .expect("client must refuse before waiting on the server")
    .expect_err("socket symlink");
    assert!(matches!(error, ClientError::Discovery(_)), "{error}");
    let accept =
        tokio::time::timeout(std::time::Duration::from_millis(50), listener.accept()).await;
    assert!(accept.is_err(), "the symlinked client reached the server");
}

#[tokio::test]
async fn discovery_falls_back_from_missing_socket_to_runtime_discovery() {
    use std::os::unix::fs::PermissionsExt;
    let dir = Scratch::new("discovery");
    let (config, id) = write_config(&dir);
    let missing = dir.join("missing.sock");
    write_socket_discovery(&dir, &id, &missing);

    let xdg = dir.join("runtime");
    let runtime = xdg.join("hofund");
    std::fs::create_dir_all(&runtime).expect("runtime directory");
    std::fs::set_permissions(&xdg, std::fs::Permissions::from_mode(0o700))
        .expect("runtime base permissions");
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))
        .expect("runtime permissions");
    let socket = runtime.join("live.sock");
    let server = spawn_server(&socket, 2);
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
        DiscoveryOptions::isolated(Some(xdg), dir.path().to_path_buf()),
    )
    .await
    .expect("discovered client");
    assert!(!format!("{client:?}").contains(TOKEN));
    server.await.expect("server task");
}

#[tokio::test]
async fn discovery_wrong_token_does_not_fall_back_to_tcp() {
    let dir = Scratch::new("wrong-token");
    let (config, id) = write_config(&dir);
    let socket = dir.join("live.sock");
    let server = spawn_server(&socket, 2);
    write_socket_discovery(&dir, &id, &socket);
    let mut options = DiscoveryOptions::isolated(None, dir.path().to_path_buf());
    options.control_listen = Some("127.0.0.1:9".parse().expect("TCP address"));
    let error = Client::discover(&config, "wrong-token", options)
        .await
        .expect_err("wrong token");
    let ClientError::Server {
        status,
        error: envelope,
    } = error
    else {
        panic!("wrong token did not return an authorization error");
    };
    assert_eq!(status, http::StatusCode::UNAUTHORIZED);
    assert_eq!(
        envelope.error.kind,
        hofund_control_client::hofund_control_proto::ControlErrorKind::Unauthorized
    );
    server.await.expect("server task");
}

#[tokio::test]
async fn discovery_health_version_mismatch_does_not_fall_back_to_tcp() {
    let dir = Scratch::new("health-version");
    let (config, id) = write_config(&dir);
    let socket = dir.join("live.sock");
    let server = spawn_server_with_health(&socket, 1, HEALTH_MISMATCH);
    write_socket_discovery(&dir, &id, &socket);
    let mut options = DiscoveryOptions::isolated(None, dir.path().to_path_buf());
    options.control_listen = Some("127.0.0.1:9".parse().expect("TCP address"));
    let error = Client::discover(&config, TOKEN, options)
        .await
        .expect_err("health version mismatch");
    assert!(matches!(error, ClientError::Discovery(_)), "{error}");
    server.await.expect("server task");
}

#[tokio::test]
async fn discovery_wrong_version_does_not_fall_back_to_tcp() {
    let dir = Scratch::new("wrong-version");
    let (config, id) = write_config(&dir);
    std::fs::write(
        dir.join(".hofund").join(format!("control-{id}.json")),
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
    let mut options = DiscoveryOptions::isolated(None, dir.path().to_path_buf());
    options.control_listen = Some("127.0.0.1:9".parse().expect("TCP address"));
    let error = Client::discover(&config, TOKEN, options)
        .await
        .expect_err("wrong discovery version");
    assert!(matches!(error, ClientError::Discovery(_)), "{error}");
}

#[tokio::test]
async fn discovery_overlong_socket_path_falls_back_to_tcp() {
    let dir = Scratch::new("long-socket");
    let (config, id) = write_config(&dir);
    let socket = dir.path().join("s".repeat(120));
    std::fs::write(
        dir.join(".hofund").join(format!("control-{id}.json")),
        serde_json::to_vec(&serde_json::json!({
            "api_version": 1,
            "pid": 7,
            "boot_id": "boot",
            "config_id": id,
            "tcp_endpoints": ["127.0.0.1:9"],
            "socket_path": socket,
        }))
        .expect("discovery JSON"),
    )
    .expect("discovery");
    let options = DiscoveryOptions::isolated(None, dir.path().to_path_buf());
    let error = Client::discover(&config, TOKEN, options)
        .await
        .expect_err("unavailable TCP fallback");
    assert!(
        matches!(error, ClientError::ConnectionUnavailable(_)),
        "{error}"
    );
    assert!(error.to_string().contains("TCP"), "{error}");
}
