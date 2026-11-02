//! C-1b: CLI `models disable` / `enable` over an explicit control endpoint.

#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use hofund_control_proto::{CatalogDisabledListResponse, HealthResponse, StatusResponse};
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "cli-c1b-control-token";
const DEADLINE: Duration = Duration::from_secs(30);

fn hofund() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_hofund"))
}

fn scratch(config_body: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::Builder::new()
        .prefix("hf-c1b-cli-")
        .tempdir_in("/tmp")
        .expect("tempdir");
    let root = fs::canonicalize(dir.path()).expect("canonical");
    let config = root.join("hofund.toml");
    fs::write(&config, config_body).expect("config");
    fs::write(
        config.parent().expect("parent").join("token"),
        format!("{TOKEN}\n"),
    )
    .expect("token");
    (dir, config)
}

fn run_hofund(config: &Path, control: &str, args: &[&str]) -> Output {
    let mut command = Command::new(hofund());
    command
        .arg("--config")
        .arg(config)
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", config.parent().expect("parent"))
        .env("HOFUND_LOG", "off")
        .env("HOFUND_CONTROL", control)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(command.output());
    });
    match rx.recv_timeout(DEADLINE) {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => panic!("hofund spawn failed: {error}"),
        Err(_) => panic!("hofund exceeded {DEADLINE:?}: args={args:?}"),
    }
}

#[tokio::test]
async fn c1b_16_cli_models_disable_and_enable_hit_control_routes() {
    let server = MockServer::start().await;
    let control = server.address().to_string();
    let bearer = format!("Bearer {TOKEN}");

    Mock::given(method("GET"))
        .and(path("/v1/hofund/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(HealthResponse::default()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/hofund/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(StatusResponse::default()))
        .mount(&server)
        .await;

    let mut disabled_body = CatalogDisabledListResponse::default();
    disabled_body.disabled = vec!["routed".to_owned()];
    Mock::given(method("POST"))
        .and(path("/v1/hofund/models/disable"))
        .and(header("authorization", bearer.as_str()))
        .and(body_string_contains(r#""model":"routed""#))
        .respond_with(ResponseTemplate::new(200).set_body_json(disabled_body))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/hofund/models/enable"))
        .and(header("authorization", bearer.as_str()))
        .and(body_string_contains(r#""model":"routed""#))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(CatalogDisabledListResponse::default()),
        )
        .expect(1)
        .mount(&server)
        .await;

    let config_body = "listen = \"127.0.0.1:0\"\ntoken_file = \"token\"\n";
    let (_dir, config) = scratch(config_body);

    let disable = tokio::task::spawn_blocking({
        let config = config.clone();
        let control = control.clone();
        move || run_hofund(&config, &control, &["models", "disable", "routed"])
    })
    .await
    .expect("join disable");
    assert!(
        disable.status.success(),
        "disable failed: stderr={}",
        String::from_utf8_lossy(&disable.stderr)
    );

    let enable = tokio::task::spawn_blocking({
        let control = control.clone();
        move || run_hofund(&config, &control, &["models", "enable", "routed"])
    })
    .await
    .expect("join enable");
    assert!(
        enable.status.success(),
        "enable failed: stderr={}",
        String::from_utf8_lossy(&enable.stderr)
    );

    server.verify().await;
}
