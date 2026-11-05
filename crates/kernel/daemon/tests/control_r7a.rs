//! R-7a control routes against a live daemon (presets and status serve role).

mod support;

use std::path::PathBuf;

use hofund_control_proto::{API_VERSION, PresetListResponse, decode_response};
use hofund_daemon::{ControlRuntime, Running, StartOptions, start_with_options};

fn config() -> String {
    "listen = \"127.0.0.1:0\"\n\
[[providers]]\nid = \"chat\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\n\
[[routes]]\nmodel = \"m\"\nupstreams = [\"chat\"]\n"
        .to_owned()
}

struct Scratch {
    config_dir: PathBuf,
    runtime_dir: PathBuf,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

impl Scratch {
    fn new() -> Self {
        let make = |kind: &str| {
            tempfile::Builder::new()
                .prefix(&format!("hofund-daemon-r7a-{kind}-"))
                .tempdir()
                .expect("scratch dir")
        };
        let config = make("config");
        let runtime = make("runtime");
        Self {
            config_dir: config.path().to_path_buf(),
            runtime_dir: runtime.path().to_path_buf(),
            _dirs: (config, runtime),
        }
    }

    fn path(&self) -> PathBuf {
        self.config_dir.join("hofund.toml")
    }
}

async fn start(scratch: &Scratch, text: &str) -> Running {
    let path = scratch.path();
    std::fs::write(&path, text).expect("config");
    let options = StartOptions {
        control_listen: Some("127.0.0.1:0".parse().expect("control address")),
        control_runtime: Some(ControlRuntime::new(None, scratch.runtime_dir.clone())),
        skip_config_watch: true,
        hofund_addr: Some(String::new()),
        ..StartOptions::default()
    };
    start_with_options(&path, options).await.expect("daemon")
}

struct Client {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl Client {
    fn new(running: &Running, scratch: &Scratch) -> Self {
        let token = std::fs::read_to_string(scratch.runtime_dir.join("hofund.token"))
            .or_else(|_| std::fs::read_to_string(scratch.config_dir.join("hofund.token")))
            .expect("token")
            .trim()
            .to_owned();
        Self {
            http: support::client(),
            base: format!("http://{}", running.control_local_addr().expect("control")),
            token,
        }
    }

    async fn get(&self, path: &str) -> (reqwest::StatusCode, Vec<u8>) {
        let response = self
            .http
            .get(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .expect("send");
        let status = response.status();
        (status, response.bytes().await.expect("body").to_vec())
    }
}

#[tokio::test]
async fn presets_route_returns_catalog_rows() {
    let scratch = Scratch::new();
    let running =
        support::within("start daemon", support::REQUEST, start(&scratch, &config())).await;
    let client = Client::new(&running, &scratch);
    let (status, body) = support::within(
        "GET presets",
        support::REQUEST,
        client.get("/v1/hofund/presets"),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let response: PresetListResponse = decode_response(API_VERSION, &body).expect("presets body");
    assert_eq!(response.version, API_VERSION);
    assert!(
        !response.presets.is_empty(),
        "preset catalog should not be empty"
    );
    support::shutdown(running).await;
}
