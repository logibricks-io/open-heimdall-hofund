//! Daemon control-state reporting over the opt-in TCP listener.

use std::sync::atomic::{AtomicU64, Ordering};

use hofund_control_proto::{
    API_VERSION, ListenSource, ShutdownState, StatusResponse, decode_response,
};
use hofund_daemon::{StartOptions, start_with_options};

#[tokio::test]
async fn status_reports_disk_runtime_revision_pending_restart_and_not_applied() {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "hofund-daemon-control-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch directory");
    let path = dir.join("hofund.toml");
    std::fs::write(&path, "listen = \"127.0.0.1:0\"\n").expect("config");
    let running = start_with_options(
        &path,
        StartOptions {
            control_listen: Some("127.0.0.1:0".parse().expect("control address")),
        },
    )
    .await
    .expect("start");
    let control = running.control_local_addr().expect("control listener");
    let token = std::fs::read_to_string(dir.join("hofund.token")).expect("token");
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
    let _ = std::fs::remove_dir_all(dir);
}
