//! A temporary Claude settings file drives the linked daemon's subagent pick.

use std::time::Duration;

use hofund_daemon::{StartOptions, start_with_options};
use serde_json::json;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn d267_temp_settings_file_drives_the_daemon_hook() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_string_contains(r#""model":"sub""#))
        .and(body_string_contains(r#""effort":"high""#))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn"}"#,
            "application/json",
        ))
        .expect(1)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().expect("temp dir");
    let config = dir.path().join("hofund.toml");
    std::fs::write(
        &config,
        format!(
            r#"listen = "127.0.0.1:0"
token_file = "token"
usage_file = "usage.sqlite"
[[providers]]
id = "prov"
protocol = "anthropic-messages"
base_url = "{}"
api_key = "test-key"
[[routes]]
model = "lead"
upstreams = ["prov"]
[[routes]]
model = "sub"
upstreams = ["prov"]
"#,
            server.uri()
        ),
    )
    .expect("config");
    let home = dir.path().join("home");
    let settings = home.join(".claude");
    std::fs::create_dir_all(&settings).expect("settings dir");
    std::fs::write(
        settings.join("settings.json"),
        r#"{"env":{"ANTHROPIC_BASE_URL":"http://127.0.0.1:0","CLAUDE_CODE_SUBAGENT_MODEL":"sub:high"}}"#,
    )
    .expect("settings");
    let running = start_with_options(
        &config,
        StartOptions {
            agent_session: Some(hofund_agent::Session::new(home)),
            ..StartOptions::default()
        },
    )
    .await
    .expect("daemon");
    let token = std::fs::read_to_string(dir.path().join("token")).expect("token");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("client");
    let body = json!({
        "model": "lead",
        "max_tokens": 8,
        "system": [{
            "type": "text",
            "text": "You are a file search specialist for Claude Code, built for this test."
        }],
        "tools": [{"name": "read", "input_schema": {"type": "object"}}],
        "messages": [{"role": "user", "content": "find the answer"}]
    });
    let response = client
        .post(format!("http://{}/v1/messages", running.local_addr))
        .header("authorization", format!("Bearer {}", token.trim()))
        .header("user-agent", "claude-cli/2.1.327 (external, cli)")
        .header("x-claude-code-agent-id", "worker")
        .json(&body)
        .send()
        .await
        .expect("response");
    let status = response.status();
    let text = response.text().await.expect("response body");
    assert_eq!(status, 200, "{text}");
    tokio::time::timeout(Duration::from_secs(30), running.shutdown())
        .await
        .expect("shutdown timeout");
}
