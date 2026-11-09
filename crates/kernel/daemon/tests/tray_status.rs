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

/// The desktop keeps the server in a struct behind a lock and refreshes the
/// tray from a spawned task, so that pattern must give a `Send` future.
struct Holder {
    server: Option<hofund_daemon::Running>,
    path: std::path::PathBuf,
    tray: TrayStatus,
}

impl Holder {
    async fn refresh(&mut self) {
        let pending = self
            .server
            .as_ref()
            .map(|server| tray_status(server, &self.path));
        self.tray = match pending {
            Some(pending) => pending.await,
            None => TrayStatus::default(),
        };
    }
}

#[tokio::test]
async fn refreshing_from_a_spawned_task_is_send() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("hofund.toml");
    std::fs::write(&path, "listen = \"127.0.0.1:0\"\n").expect("config");
    let running = start_for_test(&path).await.expect("daemon");
    let mut holder = Holder {
        server: Some(running),
        path,
        tray: TrayStatus::default(),
    };
    let holder = tokio::spawn(async move {
        holder.refresh().await;
        holder
    })
    .await
    .expect("refresh task");
    assert_eq!(holder.tray.route, "Route: no requests yet");
    if let Some(server) = holder.server {
        server.shutdown().await;
    }
}
