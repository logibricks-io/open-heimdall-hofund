//! Daemon event stream over TCP.

mod support;

use std::path::{Path, PathBuf};
use std::time::Duration;

use hofund_agent::Session;
use hofund_control_proto::{
    API_VERSION, EventEnvelope, EventPayload, ShutdownState, TokenRotateRequest, Topic,
    decode_response,
};
use hofund_daemon::{ControlRuntime, Running, StartOptions, start_with_options};

const CONFIG: &str = "listen = \"127.0.0.1:0\"\n\
[[providers]]\nid = \"chat\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key = \"provider-key\"\n\
[[routes]]\nmodel = \"m\"\nupstreams = [\"chat\"]\n";

struct Scratch(PathBuf, #[allow(dead_code)] tempfile::TempDir);

impl Scratch {
    fn new(label: &str) -> Self {
        let dir = tempfile::Builder::new()
            .prefix(&format!("hofund-daemon-events-{label}-"))
            .tempdir()
            .expect("scratch dir");
        let path = dir.path().to_path_buf();
        std::fs::create_dir_all(path.join("home")).expect("scratch home");
        Self(path, dir)
    }
}

async fn start(dir: &Path) -> Running {
    let config = dir.join("hofund.toml");
    std::fs::write(&config, CONFIG).expect("config");
    start_with_options(
        &config,
        StartOptions {
            control_listen: Some("127.0.0.1:0".parse().expect("control address")),
            control_runtime: Some(ControlRuntime::new(None, dir.to_path_buf())),
            agent_session: Some(Session::new(dir.join("home")).with_path("")),
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

/// Reads complete SSE blocks until `count` events or the end of the stream.
async fn read_events(
    response: &mut reqwest::Response,
    count: usize,
) -> (Vec<(String, String, EventEnvelope)>, bool) {
    let mut buffer = String::new();
    let mut events = Vec::new();
    loop {
        while let Some(end) = buffer.find("\n\n") {
            let block: String = buffer.drain(..end + 2).collect();
            let mut id = String::new();
            let mut name = String::new();
            let mut data = String::new();
            for line in block.lines() {
                if let Some(value) = line.strip_prefix("id: ") {
                    id = value.to_owned();
                } else if let Some(value) = line.strip_prefix("event: ") {
                    name = value.to_owned();
                } else if let Some(value) = line.strip_prefix("data: ") {
                    data.push_str(value);
                }
            }
            if !name.is_empty() {
                let envelope = decode_response::<EventEnvelope>(API_VERSION, data.as_bytes())
                    .expect("event envelope");
                events.push((id, name, envelope));
            }
            if events.len() == count {
                return (events, false);
            }
        }
        let chunk = tokio::time::timeout(Duration::from_secs(10), response.chunk())
            .await
            .expect("event stream progresses")
            .expect("chunk");
        match chunk {
            Some(bytes) => buffer.push_str(std::str::from_utf8(&bytes).expect("utf-8")),
            None => return (events, true),
        }
    }
}

#[tokio::test]
async fn daemon_stream_starts_with_a_snapshot_and_closes_on_token_rotation() {
    let dir = Scratch::new("rotate");
    let running = start(&dir.0).await;
    let client = support::client();
    let mut stream = client
        .get(url(&running, "/v1/hofund/events"))
        .bearer_auth(token(&dir.0))
        .send()
        .await
        .expect("events");
    assert_eq!(stream.status(), reqwest::StatusCode::OK);
    let (events, _) = read_events(&mut stream, 1).await;
    let (id, name, envelope) = &events[0];
    assert_eq!(name, "snapshot");
    let boot = envelope.boot_id.clone();
    assert_eq!(id, &format!("{boot}:control=0"));
    match &envelope.payload {
        EventPayload::Snapshot(snapshot) => {
            assert_eq!(snapshot.shutdown, ShutdownState::Running);
            assert_eq!(snapshot.cursors.len(), 1);
            assert_eq!(snapshot.cursors[0].topic, Topic::Control);
            assert!(!snapshot.effective_listen.is_empty());
        }
        other => panic!("expected snapshot, got {other:?}"),
    }

    let rotated = client
        .post(url(&running, "/v1/hofund/token/rotate"))
        .bearer_auth(token(&dir.0))
        .json(&TokenRotateRequest::new())
        .send()
        .await
        .expect("rotate");
    assert_eq!(rotated.status(), reqwest::StatusCode::OK);
    let (events, ended) = read_events(&mut stream, 2).await;
    let names: Vec<&str> = events.iter().map(|(_, name, _)| name.as_str()).collect();
    assert_eq!(names, ["token_rotated"]);
    assert!(ended, "the stream ends after token_rotated");
    assert_eq!(events[0].0, format!("{boot}:control=1"));
    support::shutdown(running).await;
}

#[tokio::test]
async fn embedded_shutdown_publishes_shutdown_started_and_closes_live_stream() {
    let dir = Scratch::new("embedded-shutdown");
    let running = start(&dir.0).await;
    let mut stream = support::client()
        .get(url(&running, "/v1/hofund/events"))
        .bearer_auth(token(&dir.0))
        .send()
        .await
        .expect("events");
    assert_eq!(stream.status(), reqwest::StatusCode::OK);
    let (snapshot, _) = read_events(&mut stream, 1).await;
    assert_eq!(snapshot[0].1, "snapshot");

    tokio::time::timeout(Duration::from_secs(10), running.shutdown())
        .await
        .expect("shutdown finishes with a live event stream");
    let (events, ended) = read_events(&mut stream, usize::MAX).await;
    assert!(ended, "the event stream ends after shutdown_started");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].1, "shutdown_started");
    match &events[0].2.payload {
        EventPayload::ShutdownStarted(event) => assert!(event.draining),
        other => panic!("expected shutdown_started, got {other:?}"),
    }
}

#[tokio::test]
async fn shutdown_closes_a_control_request_that_outlasts_the_drain_bound() {
    use tokio::io::AsyncWriteExt;

    let dir = Scratch::new("stuck-drain");
    let running = start(&dir.0).await;
    let addr = running.control_local_addr().expect("control");
    let mut stuck = tokio::net::TcpStream::connect(addr)
        .await
        .expect("connect control");
    // A request whose body never finishes keeps the connection in flight,
    // so only the drain bound can end it.
    let head = format!(
        "POST /v1/hofund/config/validate HTTP/1.1\r\nHost: {addr}\r\n\
         Authorization: Bearer {}\r\nContent-Type: application/json\r\n\
         Content-Length: 1000\r\n\r\n{{",
        token(&dir.0)
    );
    stuck.write_all(head.as_bytes()).await.expect("send head");
    tokio::time::sleep(Duration::from_millis(200)).await;

    let started = std::time::Instant::now();
    tokio::time::timeout(Duration::from_secs(10), running.shutdown())
        .await
        .expect("shutdown is bounded with a stuck control request");
    assert!(
        started.elapsed() >= Duration::from_secs(3),
        "the stuck request is given the drain budget, not cut at once: {:?}",
        started.elapsed()
    );
    drop(stuck);
}
