//! Daemon control-state reporting over the opt-in TCP listener.

use hofund_control_proto::{
    API_VERSION, ListenSource, ShutdownState, StatusResponse, decode_response,
};
use hofund_daemon::{ControlRuntime, StartOptions, start_with_options};

#[tokio::test]
async fn status_reports_disk_runtime_revision_pending_restart_and_not_applied() {
    #[cfg(unix)]
    let dir = tempfile::Builder::new()
        .prefix("hofund-daemon-control-")
        .tempdir_in("/tmp")
        .expect("scratch directory");
    #[cfg(not(unix))]
    let dir = tempfile::tempdir().expect("scratch directory");
    let path = dir.path().join("hofund.toml");
    std::fs::write(&path, "listen = \"127.0.0.1:0\"\n").expect("config");
    let running = start_with_options(
        &path,
        StartOptions {
            control_listen: Some("127.0.0.1:0".parse().expect("control address")),
            control_runtime: Some(ControlRuntime::new(None, dir.path().to_path_buf())),
            ..StartOptions::default()
        },
    )
    .await
    .expect("start");
    let control = running.control_local_addr().expect("control listener");
    let token = std::fs::read_to_string(dir.path().join("hofund.token")).expect("token");
    let client = reqwest::Client::new();
    let url = format!("http://{control}/v1/hofund/status");

    let response = client
        .get(&url)
        .bearer_auth(token.trim())
        .send()
        .await
        .expect("status response");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let initial: StatusResponse =
        decode_response(API_VERSION, &response.bytes().await.expect("status body"))
            .expect("status DTO");
    assert_eq!(initial.pid, std::process::id());
    assert!(initial.started_at_ms > 0);
    assert_eq!(initial.generation, 1);
    assert_eq!(initial.disk_revision, initial.runtime_revision);
    assert_eq!(initial.disk_revision.len(), 64);
    assert!(initial.pending_restart.is_empty());
    assert!(initial.not_applied.is_empty());
    assert_eq!(initial.in_flight, 0);
    assert_eq!(initial.shutdown, ShutdownState::Running);
    assert_eq!(initial.model_listener.configured, "127.0.0.1:0");
    assert_eq!(initial.model_listener.effective, "127.0.0.1:0");
    assert_eq!(
        initial.model_listener.running.as_deref(),
        Some(running.local_addr.to_string().as_str())
    );
    assert_eq!(
        initial.model_listener.effective_source,
        ListenSource::Config
    );

    std::fs::write(&path, "listen = \"127.0.0.1:0\"\n# edited on disk\n").expect("edit config");
    let response = client
        .get(&url)
        .bearer_auth(token.trim())
        .send()
        .await
        .expect("changed status response");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let changed: StatusResponse = decode_response(
        API_VERSION,
        &response.bytes().await.expect("changed status body"),
    )
    .expect("changed status DTO");
    assert_ne!(changed.disk_revision, initial.disk_revision);
    assert_eq!(changed.runtime_revision, initial.runtime_revision);
    assert!(changed.pending_restart.is_empty());
    assert!(changed.not_applied.is_empty());

    running.shutdown().await;
}

#[cfg(windows)]
#[tokio::test]
async fn windows_token_uses_profile_acl_until_pipe() {
    let profile = tempfile::tempdir().expect("profile directory");
    let config = profile.path().join("hofund.toml");
    std::fs::write(&config, "listen = \"127.0.0.1:0\"\n").expect("config");
    let running = start_with_options(
        &config,
        StartOptions {
            control_listen: Some("127.0.0.1:0".parse().expect("control address")),
            control_runtime: Some(ControlRuntime::new(None, profile.path().to_path_buf())),
            ..StartOptions::default()
        },
    )
    .await
    .expect("TCP fallback");
    let token_path = profile.path().join("hofund.token");
    assert!(token_path.starts_with(profile.path()));
    let token = std::fs::read_to_string(token_path).expect("profile token");
    let response = reqwest::get(format!(
        "http://{}/v1/hofund/health",
        running.control_local_addr().expect("control TCP")
    ))
    .await
    .expect("control health");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert!(!token.trim().is_empty());
    running.shutdown().await;
}
