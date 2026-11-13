//! G-7 daemon log-tail route.

mod support;

use hofund_control_proto::{API_VERSION, LogTailResponse, decode_response};
use hofund_daemon::{ControlRuntime, Running, StartOptions, start_with_options};

struct Scratch {
    config: std::path::PathBuf,
    runtime: std::path::PathBuf,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

async fn start() -> (Scratch, Running) {
    let config = tempfile::tempdir().expect("config dir");
    let runtime = tempfile::tempdir().expect("runtime dir");
    std::fs::create_dir_all(runtime.path().join("home")).expect("runtime home");
    let path = config.path().join("hofund.toml");
    std::fs::write(&path, "listen = \"127.0.0.1:0\"\n").expect("config");
    let running = start_with_options(
        &path,
        StartOptions {
            control_listen: Some("127.0.0.1:0".parse().expect("control")),
            control_enabled: true,
            control_runtime: Some(ControlRuntime::new(None, runtime.path().to_path_buf())),
            skip_config_watch: true,
            hofund_addr: Some(String::new()),
            ..StartOptions::default()
        },
    )
    .await
    .expect("daemon");
    (
        Scratch {
            config: path,
            runtime: runtime.path().to_path_buf(),
            _dirs: (config, runtime),
        },
        running,
    )
}

struct Client {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl Client {
    fn new(scratch: &Scratch, running: &Running) -> Self {
        let token = std::fs::read_to_string(
            scratch
                .config
                .parent()
                .expect("parent")
                .join("hofund.token"),
        )
        .or_else(|_| std::fs::read_to_string(scratch.runtime.join("hofund.token")))
        .expect("token")
        .trim()
        .to_owned();
        Self {
            http: support::client(),
            base: format!("http://{}", running.control_local_addr().expect("control")),
            token,
        }
    }

    async fn logs(&self, query: &str) -> (reqwest::StatusCode, Vec<u8>) {
        let response = self
            .http
            .get(format!("{}/v1/hofund/logs{query}", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .expect("logs");
        let status = response.status();
        (status, response.bytes().await.expect("body").to_vec())
    }
}

fn decoded(body: &[u8]) -> LogTailResponse {
    decode_response(API_VERSION, body).expect("log response")
}

#[tokio::test]
async fn log_tail_handles_missing_files_resume_and_the_64_kib_cap() {
    let (scratch, running) = start().await;
    let client = Client::new(&scratch, &running);
    let (status, body) = client.logs("").await;
    assert_eq!(status, reqwest::StatusCode::OK, "missing log was not a 200");
    let empty = decoded(&body);
    assert_eq!(empty.offset, 0);
    assert_eq!(empty.next_offset, 0);
    assert!(empty.lines.is_empty());
    assert!(empty.path.ends_with(".log"));

    let path = std::path::PathBuf::from(&empty.path);
    std::fs::create_dir_all(path.parent().expect("log parent")).expect("log dir");
    std::fs::write(&path, b"one\ntwo\n").expect("log lines");
    let (status, body) = client.logs("?after=0&max_bytes=4").await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let first = decoded(&body);
    assert_eq!(first.lines, ["one"]);
    assert_eq!(first.next_offset, 4);
    let (status, body) = client
        .logs(&format!("?after={}&max_bytes=4", first.next_offset))
        .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let second = decoded(&body);
    assert_eq!(second.offset, 4);
    assert_eq!(second.next_offset, 8);
    assert_eq!(second.lines, ["two"]);

    std::fs::write(&path, vec![b'x'; 70_000]).expect("large log");
    let (status, body) = client.logs("?after=0&max_bytes=65536").await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let truncated = decoded(&body);
    assert_eq!(truncated.next_offset, 65_536);
    assert_eq!(truncated.lines[0].len(), 65_536);
    let (status, body) = client
        .logs(&format!("?after={}&max_bytes=65536", truncated.next_offset))
        .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let rest = decoded(&body);
    assert_eq!(rest.offset, 65_536);
    assert_eq!(rest.next_offset, 70_000);
    assert_eq!(rest.lines[0].len(), 70_000 - 65_536);

    let (status, _) = client.logs("?after=0&max_bytes=65537").await;
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST);
    support::shutdown(running).await;
}
