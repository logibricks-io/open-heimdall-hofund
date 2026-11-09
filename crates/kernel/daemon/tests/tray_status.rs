//! Tray status over a real daemon with no traffic or subscriptions.

use hofund_daemon::{TrayStatus, start_for_test, tray_status};

#[tokio::test]
async fn empty_daemon_has_empty_tray_status() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("hofund.toml");
    std::fs::write(&path, "listen = \"127.0.0.1:0\"\n").expect("config");
    let running = start_for_test(&path).await.expect("daemon");
    assert!(running.quota_source().is_some());
    assert!(running.route_trace_book().is_some());

    let status = tray_status(&running, &path).await;
    assert_eq!(
        status,
        TrayStatus {
            route: "Route: no requests yet".to_owned(),
            today: "Today: no requests yet".to_owned(),
            quotas: Vec::new(),
        }
    );
    running.shutdown().await;
}
