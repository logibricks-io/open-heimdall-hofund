//! Each inbound protocol against each upstream kind, plus auth and failover.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use hofund_daemon::{check, start};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Upstream {
    kind: &'static str,
    protocol: Option<&'static str>,
    quirks: Option<&'static str>,
    wire_path: &'static str,
    body: &'static str,
    stream: &'static str,
}

struct Inbound {
    route: &'static str,
    stream_route: &'static str,
    body: &'static str,
    stream_body: &'static str,
}

const UPSTREAMS: &[Upstream] = &[
    Upstream {
        kind: "openai_compat",
        protocol: Some("chat"),
        quirks: None,
        wire_path: "/chat/completions",
        body: r#"{"choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#,
        stream: "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n",
    },
    Upstream {
        kind: "openai_compat",
        protocol: Some("responses"),
        quirks: None,
        wire_path: "/responses",
        body: r#"{"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}]}"#,
        stream: "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"ok\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[]}}\n\n",
    },
    Upstream {
        kind: "anthropic",
        protocol: None,
        quirks: Some("messages-default"),
        wire_path: "/v1/messages",
        body: r#"{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn"}"#,
        stream: "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    },
];

const GENERATE_BODY: &str = r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#;

const INBOUNDS: &[Inbound] = &[
    Inbound {
        route: "/v1/messages",
        stream_route: "/v1/messages",
        body: r#"{"model":"demo","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}"#,
        stream_body: r#"{"model":"demo","max_tokens":16,"stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
    },
    Inbound {
        route: "/v1/chat/completions",
        stream_route: "/v1/chat/completions",
        body: r#"{"model":"demo","messages":[{"role":"user","content":"hi"}]}"#,
        stream_body: r#"{"model":"demo","stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
    },
    Inbound {
        route: "/v1/responses",
        stream_route: "/v1/responses",
        body: r#"{"model":"demo","input":"hi"}"#,
        stream_body: r#"{"model":"demo","input":"hi","stream":true}"#,
    },
    Inbound {
        route: "/v1beta/models/demo:generateContent",
        stream_route: "/v1beta/models/demo:streamGenerateContent?alt=sse",
        body: GENERATE_BODY,
        stream_body: GENERATE_BODY,
    },
];

fn scratch() -> PathBuf {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("hofund-http-{}-{id}", std::process::id()));
    fs::create_dir_all(&path).expect("dir");
    path
}

fn config_text(base_url: &str, upstream: &Upstream, failover_statuses: &str) -> String {
    let protocol = upstream
        .protocol
        .map(|protocol| format!("protocol = \"{protocol}\"\n"))
        .unwrap_or_default();
    let quirks = upstream
        .quirks
        .map(|quirks| format!("quirks = \"{quirks}\"\n"))
        .unwrap_or_default();
    format!(
        r#"
listen = "127.0.0.1:0"
token_file = "token"
usage_file = "usage.sqlite"
[failover]
statuses = {failover_statuses}
[[providers]]
id = "primary"
kind = "{kind}"
base_url = "{base_url}"
api_key = "test-key"
{protocol}{quirks}[[routes]]
model = "demo"
upstreams = ["primary"]
"#,
        kind = upstream.kind,
    )
}

async fn listen(dir: &Path, text: &str) -> (hofund_daemon::Running, String) {
    let config = dir.join("hofund.toml");
    fs::write(&config, text).expect("config");
    let running = start(&config).await.expect("start");
    let token = fs::read_to_string(dir.join("token")).expect("token");
    (running, token.trim().to_owned())
}

#[tokio::test]
async fn each_inbound_talks_to_each_upstream() {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("client");
    for upstream in UPSTREAMS {
        for inbound in INBOUNDS {
            for stream in [false, true] {
                let server = MockServer::start().await;
                let template = if stream {
                    ResponseTemplate::new(200)
                        .insert_header("content-type", "text/event-stream")
                        .set_body_string(upstream.stream)
                } else {
                    ResponseTemplate::new(200).set_body_raw(upstream.body, "application/json")
                };
                Mock::given(method("POST"))
                    .and(path(upstream.wire_path))
                    .respond_with(template)
                    .expect(1)
                    .mount(&server)
                    .await;
                let dir = scratch();
                let (running, token) = listen(
                    &dir,
                    &config_text(&server.uri(), upstream, "[429, 500, 502, 503, 504]"),
                )
                .await;
                let body = if stream {
                    inbound.stream_body
                } else {
                    inbound.body
                };
                let route = if stream {
                    inbound.stream_route
                } else {
                    inbound.route
                };
                let response = client
                    .post(format!("http://{}{route}", running.local_addr))
                    .header("authorization", format!("Bearer {token}"))
                    .body(body)
                    .send()
                    .await
                    .expect("send");
                let status = response.status();
                let text = response.text().await.expect("text");
                assert_eq!(status, 200, "{text}");
                assert!(
                    text.contains("ok"),
                    "{} -> {} stream={stream}: {text}",
                    inbound.route,
                    upstream.wire_path
                );
                running.shutdown().await;
                fs::remove_dir_all(dir).ok();
            }
        }
    }
}

#[tokio::test]
async fn auth_models_failover_and_config_check() {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("client");
    let primary = MockServer::start().await;
    let backup = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(503).set_body_string("{\"error\":{\"message\":\"busy\"}}"),
        )
        .expect(1)
        .mount(&primary)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#,
            "application/json",
        ))
        .expect(1)
        .mount(&backup)
        .await;

    let dir = scratch();
    let config = format!(
        r#"
listen = "127.0.0.1:0"
token_file = "token"
usage_file = "usage.sqlite"
[[providers]]
id = "primary"
kind = "openai_compat"
base_url = "{}"
protocol = "chat"
[[providers]]
id = "backup"
kind = "openai_compat"
base_url = "{}"
protocol = "chat"
[[routes]]
model = "demo"
upstreams = ["primary", "backup"]
"#,
        primary.uri(),
        backup.uri()
    );
    let (running, token) = listen(&dir, &config).await;
    let url = format!("http://{}/v1/chat/completions", running.local_addr);
    let missing = client.post(&url).body("{}").send().await.expect("send");
    assert_eq!(missing.status(), 401);
    let response = client
        .post(&url)
        .header("authorization", format!("Bearer {token}"))
        .body(r#"{"model":"demo","messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await
        .expect("send");
    let text = response.text().await.expect("text");
    assert!(text.contains("ok"), "{text}");

    let models = client
        .get(format!("http://{}/v1/models", running.local_addr))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("models");
    assert!(models.text().await.expect("text").contains("demo"));

    let missing_list = client
        .get(format!("http://{}/v1beta/models", running.local_addr))
        .send()
        .await
        .expect("models");
    assert_eq!(missing_list.status().as_u16(), 401);
    let missing_list_body = missing_list.text().await.expect("text");
    assert!(
        missing_list_body.contains("UNAUTHENTICATED"),
        "{missing_list_body}"
    );
    let listed = client
        .get(format!(
            "http://{}/v1beta/models?alt=json",
            running.local_addr
        ))
        .header("x-goog-api-key", &token)
        .send()
        .await
        .expect("models");
    assert!(listed.status().is_success());
    let listed_body = listed.text().await.expect("text");
    assert!(listed_body.contains("models/demo"), "{listed_body}");
    assert!(listed_body.contains("generateContent"), "{listed_body}");
    running.shutdown().await;

    let bad = dir.join("bad.toml");
    fs::write(&bad, "listen = \"nope\"\n").expect("write");
    let error = check(&bad).expect_err("config");
    assert!(error.to_string().contains("listen"));
    fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn generative_inbound_carries_a_tool_call() {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("client");
    let cases = [
        (
            "openai_compat",
            Some("chat"),
            None,
            "/chat/completions",
            r#"{"choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"Paris\"}"}}]},"finish_reason":"tool_calls"}]}"#,
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"\"}}]}}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"city\\\":\\\"Paris\\\"}\"}}]}}]}\n\ndata: {\"choices\":[{\"index\":0,\"finish_reason\":\"tool_calls\",\"delta\":{}}]}\n\ndata: [DONE]\n\n",
        ),
        (
            "anthropic",
            None,
            Some("messages-default"),
            "/v1/messages",
            r#"{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"get_weather","input":{"city":"Paris"}}],"stop_reason":"tool_use"}"#,
            "",
        ),
    ];
    for (kind, protocol, quirks, wire_path, body, stream) in cases {
        let upstream = Upstream {
            kind,
            protocol,
            quirks,
            wire_path,
            body,
            stream,
        };
        for stream_call in [false, true] {
            if stream_call && stream.is_empty() {
                continue;
            }
            let server = MockServer::start().await;
            let template = if stream_call {
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(stream)
            } else {
                ResponseTemplate::new(200).set_body_raw(body, "application/json")
            };
            Mock::given(method("POST"))
                .and(path(wire_path))
                .respond_with(template)
                .expect(1)
                .mount(&server)
                .await;
            let dir = scratch();
            let (running, token) = listen(
                &dir,
                &config_text(&server.uri(), &upstream, "[429, 500, 502, 503, 504]"),
            )
            .await;
            let route = if stream_call {
                "/v1beta/models/demo:streamGenerateContent?alt=sse"
            } else {
                "/v1beta/models/demo:generateContent"
            };
            let response = client
                .post(format!("http://{}{route}", running.local_addr))
                .header("x-goog-api-key", &token)
                .body(GENERATE_BODY)
                .send()
                .await
                .expect("send");
            let status = response.status();
            let text = response.text().await.expect("text");
            assert_eq!(status.as_u16(), 200, "{text}");
            assert!(
                text.contains("get_weather"),
                "{kind} stream={stream_call}: {text}"
            );
            assert!(
                text.contains("Paris"),
                "{kind} stream={stream_call}: {text}"
            );
            running.shutdown().await;
            fs::remove_dir_all(dir).ok();
        }
    }
}
