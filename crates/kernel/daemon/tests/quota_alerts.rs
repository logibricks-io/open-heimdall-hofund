//! Quota alert control routes and persisted settings.

mod support;

use std::path::{Path, PathBuf};

use hofund_control_proto::{
    API_VERSION, ControlErrorKind, QuotaAlertsResponse, QuotaAlertsSetRequest,
    decode_error_envelope, decode_response,
};
use hofund_daemon::{ControlRuntime, Running, StartOptions, start_with_options};
use hofund_router::quota_alerts::QuotaAlertStore;

struct Scratch(PathBuf, #[allow(dead_code)] tempfile::TempDir);

impl Scratch {
    fn new(label: &str) -> Self {
        let guard = tempfile::Builder::new()
            .prefix(&format!("hofund-daemon-quota-alerts-{label}-"))
            .tempdir()
            .expect("scratch directory");
        Self(guard.path().to_path_buf(), guard)
    }
}

const CONFIG: &str = "listen = \"127.0.0.1:0\"\n";

async fn start(dir: &Path) -> Running {
    let config = dir.join("hofund.toml");
    std::fs::write(&config, CONFIG).expect("config");
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

async fn read_alerts(dir: &Path, running: &Running) -> QuotaAlertsResponse {
    let response = support::client()
        .get(url(running, "/v1/hofund/quotas/alerts"))
        .bearer_auth(token(dir))
        .send()
        .await
        .expect("GET alerts");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    decode_response(API_VERSION, &response.bytes().await.expect("body")).expect("alerts response")
}

#[tokio::test]
async fn al1_11_control_rejects_invalid_alert_settings() {
    let scratch = Scratch::new("reject");
    let dir = &scratch.0;
    let running = start(dir).await;
    let client = support::client();
    let auth = token(dir);

    let over = client
        .post(url(&running, "/v1/hofund/quotas/alerts/settings"))
        .bearer_auth(&auth)
        .json(&QuotaAlertsSetRequest {
            usage_alert: Some(101),
            ..QuotaAlertsSetRequest::new()
        })
        .send()
        .await
        .expect("POST over 100");
    assert_eq!(over.status(), reqwest::StatusCode::BAD_REQUEST);
    let error = decode_error_envelope(API_VERSION, &over.bytes().await.expect("body"))
        .expect("error envelope");
    assert_eq!(error.error.kind, ControlErrorKind::BadRequest);

    let negative = client
        .post(url(&running, "/v1/hofund/quotas/alerts/settings"))
        .bearer_auth(&auth)
        .json(&QuotaAlertsSetRequest {
            balance_alert: Some(-2.0),
            ..QuotaAlertsSetRequest::new()
        })
        .send()
        .await
        .expect("POST negative balance");
    assert_eq!(negative.status(), reqwest::StatusCode::BAD_REQUEST);

    let defaults = read_alerts(dir, &running).await;
    assert_eq!(defaults.usage_alert, 0);
    assert_eq!(defaults.balance_alert, 0.0);

    support::shutdown(running).await;
}

#[tokio::test]
async fn al1_12_alert_settings_round_trip_and_survive_restart() {
    let scratch = Scratch::new("persist");
    let dir = &scratch.0;
    let config = dir.join("hofund.toml");
    let running = start(dir).await;
    let client = support::client();
    let auth = token(dir);

    let mut request = QuotaAlertsSetRequest::new();
    request.usage_alert = Some(75);
    request.balance_alert = Some(12.5);
    let posted = client
        .post(url(&running, "/v1/hofund/quotas/alerts/settings"))
        .bearer_auth(&auth)
        .json(&request)
        .send()
        .await
        .expect("POST settings");
    assert_eq!(posted.status(), reqwest::StatusCode::OK);
    let body: QuotaAlertsResponse =
        decode_response(API_VERSION, &posted.bytes().await.expect("POST body"))
            .expect("set response");
    assert_eq!(body.usage_alert, 75);
    assert!((body.balance_alert - 12.5).abs() < f64::EPSILON);

    let listed = read_alerts(dir, &running).await;
    assert_eq!(listed.usage_alert, 75);
    assert!((listed.balance_alert - 12.5).abs() < f64::EPSILON);

    let settings_path = dir.join("quota-alert-settings.json");
    assert!(settings_path.is_file(), "settings file on disk");

    support::shutdown(running).await;

    let reopened = QuotaAlertStore::new(dir);
    assert_eq!(reopened.usage_alert(), 75);
    assert!((reopened.balance_alert() - 12.5).abs() < f64::EPSILON);

    let restarted = start_with_options(
        &config,
        StartOptions {
            control_listen: Some("127.0.0.1:0".parse().expect("control address")),
            control_runtime: Some(ControlRuntime::new(None, dir.to_path_buf())),
            ..StartOptions::default()
        },
    )
    .await
    .expect("restarted daemon");
    let after = read_alerts(dir, &restarted).await;
    assert_eq!(after.usage_alert, 75);
    assert!((after.balance_alert - 12.5).abs() < f64::EPSILON);
    support::shutdown(restarted).await;
}
