//! C-1b: CLI `models disable` / `enable` over an explicit control endpoint.

#![cfg(unix)]

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hofund_control_proto::{CatalogDisabledListResponse, HealthResponse, StatusResponse};

const TOKEN: &str = "cli-c1b-control-token";
const DEADLINE: Duration = Duration::from_secs(30);

fn hofund() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_hofund"))
}

#[derive(Clone, Debug)]
struct SeenRequest {
    method: String,
    path: String,
    authorization: Option<String>,
    body: String,
}

struct FakeControl {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<SeenRequest>>>,
}

impl FakeControl {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind control mock");
        listener.set_nonblocking(true).expect("set_nonblocking");
        let addr = listener.local_addr().expect("addr");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                if let Some(record) = read_request(&mut stream) {
                    let response = respond(&record);
                    log.lock().expect("log").push(record);
                    let _ = stream.write_all(response.as_bytes());
                }
            }
        });
        Self { addr, seen }
    }

    fn seen(&self) -> Vec<SeenRequest> {
        self.seen.lock().expect("seen").clone()
    }
}

fn read_request(stream: &mut std::net::TcpStream) -> Option<SeenRequest> {
    let mut request = Vec::new();
    let mut buf = [0_u8; 4096];
    let head_end = loop {
        if let Some(at) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break at + 4;
        }
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return None,
            Ok(read) => request.extend_from_slice(&buf[..read]),
        }
    };
    let head = String::from_utf8_lossy(&request[..head_end]);
    let mut method = String::new();
    let mut path = String::new();
    let mut authorization = None;
    for line in head.lines() {
        if method.is_empty() {
            let mut parts = line.split_whitespace();
            method = parts.next()?.to_owned();
            path = parts.next()?.to_owned();
            continue;
        }
        if let Some(value) = line
            .strip_prefix("Authorization:")
            .or_else(|| line.strip_prefix("authorization:"))
        {
            authorization = Some(value.trim().to_owned());
        }
    }
    let content_length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    let body_start = head_end;
    while request.len().saturating_sub(body_start) < content_length {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(read) => request.extend_from_slice(&buf[..read]),
        }
    }
    let body =
        String::from_utf8_lossy(&request[body_start..body_start + content_length]).into_owned();
    Some(SeenRequest {
        method,
        path,
        authorization,
        body,
    })
}

fn respond(request: &SeenRequest) -> String {
    let body = match request.path.as_str() {
        "/v1/hofund/health" => {
            serde_json::to_string(&HealthResponse::default()).expect("health JSON")
        }
        "/v1/hofund/status" => {
            serde_json::to_string(&StatusResponse::default()).expect("status JSON")
        }
        "/v1/hofund/models/disable" | "/v1/hofund/models/enable" => {
            let mut response = CatalogDisabledListResponse::default();
            if request.path.ends_with("/disable") {
                response.disabled = vec!["routed".to_owned()];
            }
            serde_json::to_string(&response).expect("disabled JSON")
        }
        other => {
            return format!(
                "HTTP/1.1 404 Not Found\r\ncontent-length: {}\r\nconnection: close\r\n\r\nnot found: {other}",
                other.len()
            );
        }
    };
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
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

fn run_hofund(config: &Path, control: SocketAddr, args: &[&str]) -> Output {
    let mut command = Command::new(hofund());
    command
        .arg("--config")
        .arg(config)
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", config.parent().expect("parent"))
        .env("HOFUND_LOG", "off")
        .env("HOFUND_CONTROL", control.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(command.output());
    });
    match rx.recv_timeout(DEADLINE) {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => panic!("hofund spawn failed: {error}"),
        Err(_) => panic!("hofund exceeded {DEADLINE:?}: args={args:?}"),
    }
}

#[test]
fn c1b_16_cli_models_disable_and_enable_hit_control_routes() {
    let control = FakeControl::start();
    let config_body = "listen = \"127.0.0.1:0\"\ntoken_file = \"token\"\n";
    let (_dir, config) = scratch(config_body);

    let disable = run_hofund(&config, control.addr, &["models", "disable", "routed"]);
    assert!(
        disable.status.success(),
        "disable failed: stderr={}",
        String::from_utf8_lossy(&disable.stderr)
    );

    let enable = run_hofund(&config, control.addr, &["models", "enable", "routed"]);
    assert!(
        enable.status.success(),
        "enable failed: stderr={}",
        String::from_utf8_lossy(&enable.stderr)
    );

    let seen = control.seen();
    let disable_req = seen
        .iter()
        .find(|row| row.method == "POST" && row.path == "/v1/hofund/models/disable")
        .expect("disable request");
    assert_eq!(
        disable_req.authorization.as_deref(),
        Some(format!("Bearer {TOKEN}").as_str())
    );
    assert!(
        disable_req.body.contains("\"model\":\"routed\""),
        "{:?}",
        disable_req.body
    );

    let enable_req = seen
        .iter()
        .find(|row| row.method == "POST" && row.path == "/v1/hofund/models/enable")
        .expect("enable request");
    assert_eq!(
        enable_req.authorization.as_deref(),
        Some(format!("Bearer {TOKEN}").as_str())
    );
    assert!(
        enable_req.body.contains("\"model\":\"routed\""),
        "{:?}",
        enable_req.body
    );
}
