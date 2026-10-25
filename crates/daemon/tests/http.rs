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
    protocol: &'static str,
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
        protocol: "openai-chat",
        quirks: None,
        wire_path: "/chat/completions",
        body: r#"{"choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#,
        stream: "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n",
    },
    Upstream {
        protocol: "openai-responses",
        quirks: None,
        wire_path: "/responses",
        body: r#"{"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}]}"#,
        stream: "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"ok\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[]}}\n\n",
    },
    Upstream {
        protocol: "anthropic-messages",
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
protocol = "{protocol}"
base_url = "{base_url}"
api_key = "test-key"
{quirks}[[routes]]
model = "demo"
upstreams = ["primary"]
"#,
        protocol = upstream.protocol,
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
protocol = "openai-chat"
base_url = "{}"
[[providers]]
id = "backup"
protocol = "openai-chat"
base_url = "{}"
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
            "openai-chat",
            None,
            "/chat/completions",
            r#"{"choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"Paris\"}"}}]},"finish_reason":"tool_calls"}]}"#,
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"\"}}]}}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"city\\\":\\\"Paris\\\"}\"}}]}}]}\n\ndata: {\"choices\":[{\"index\":0,\"finish_reason\":\"tool_calls\",\"delta\":{}}]}\n\ndata: [DONE]\n\n",
        ),
        (
            "anthropic-messages",
            Some("messages-default"),
            "/v1/messages",
            r#"{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"get_weather","input":{"city":"Paris"}}],"stop_reason":"tool_use"}"#,
            "",
        ),
    ];
    for (protocol, quirks, wire_path, body, stream) in cases {
        let upstream = Upstream {
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
                "{protocol} stream={stream_call}: {text}"
            );
            assert!(
                text.contains("Paris"),
                "{protocol} stream={stream_call}: {text}"
            );
            running.shutdown().await;
            fs::remove_dir_all(dir).ok();
        }
    }
}

#[tokio::test]
async fn cross_protocol_fields_do_not_reach_the_client_or_the_upstream() {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("client");
    let server = MockServer::start().await;
    let stream = concat!(
        "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1710000000,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1710000000,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":1,\"total_tokens\":3}}\n\n",
        "data: [DONE]\n\n",
    );
    let unary = r#"{"id":"chatcmpl-1","created":1710000000,"choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(move |request: &wiremock::Request| {
            let body = String::from_utf8_lossy(&request.body);
            if body.contains("\"stream\":true") {
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(stream)
            } else {
                ResponseTemplate::new(200).set_body_raw(unary, "application/json")
            }
        })
        .mount(&server)
        .await;

    let upstream = Upstream {
        protocol: "openai-chat",
        quirks: None,
        wire_path: "/chat/completions",
        body: "",
        stream: "",
    };
    let dir = scratch();
    let (running, token) = listen(
        &dir,
        &config_text(&server.uri(), &upstream, "[429, 500, 502, 503, 504]"),
    )
    .await;
    let anthropic = r#"{"model":"demo","max_tokens":16,"stream":true,"thinking":{"type":"enabled","budget_tokens":64},"context_management":{"edits":[]},"output_config":{"effort":"low"},"metadata":{"user_id":"device-should-not-leak"},"messages":[{"role":"user","content":[{"type":"text","text":"hi","cache_control":{"type":"ephemeral"}}]}]}"#;
    let anthropic_text = post_text(&client, &running, "/v1/messages", &token, anthropic).await;
    assert!(!anthropic_text.contains("1710000000"), "{anthropic_text}");
    assert!(!anthropic_text.contains("device-should-not-leak"));
    let names = event_names(&anthropic_text);
    let stop = names.iter().position(|name| *name == "content_block_stop");
    let delta = names.iter().position(|name| *name == "message_delta");
    assert!(stop.is_some_and(|stop| delta.is_some_and(|delta| stop < delta)));
    assert_eq!(
        names
            .iter()
            .filter(|name| **name == "message_delta")
            .count(),
        1
    );
    assert!(anthropic_text.contains("\"id\":\"msg_chatcmpl-1\""));
    assert!(anthropic_text.contains("\"usage\":{\"input_tokens\":0,\"output_tokens\":0}"));

    let responses_text = post_text(
        &client,
        &running,
        "/v1/responses",
        &token,
        r#"{"model":"demo","input":"hi"}"#,
    )
    .await;
    assert!(!responses_text.contains("1710000000"), "{responses_text}");

    let gemini_text = post_text(
        &client,
        &running,
        "/v1beta/models/demo:streamGenerateContent?alt=sse",
        &token,
        GENERATE_BODY,
    )
    .await;
    assert!(!gemini_text.contains("1710000000"), "{gemini_text}");
    assert!(gemini_text.contains("\"parts\""));

    let chat_text = post_text(
        &client,
        &running,
        "/v1/chat/completions",
        &token,
        r#"{"model":"demo","messages":[{"role":"user","content":"hi"}],"metadata":{"user_id":"device-should-not-leak"}}"#,
    )
    .await;
    assert!(chat_text.contains("1710000000"), "{chat_text}");

    let requests = server.received_requests().await.expect("requests");
    let first = String::from_utf8(requests[0].body.clone()).expect("utf8");
    for needle in [
        "thinking",
        "context_management",
        "output_config",
        "device-should-not-leak",
        "cache_control",
    ] {
        assert!(!first.contains(needle), "{needle} leaked: {first}");
    }
    assert!(first.contains("\"include_usage\":true"), "{first}");
    let same = requests
        .iter()
        .map(|request| String::from_utf8(request.body.clone()).expect("utf8"))
        .find(|body| body.contains("device-should-not-leak"))
        .expect("same-protocol metadata");
    assert!(same.contains("device-should-not-leak"));
    running.shutdown().await;
    fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn codex_responses_tools_are_accepted() {
    // Same reconstructed Codex CLI 0.164.0 tool set as the codec fixture.
    let fixture = include_str!("../../codec/tests/fixtures/codex_0_164_responses.json");
    let original: serde_json::Value = serde_json::from_str(fixture).expect("fixture");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("client");

    let responses = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}]}"#,
            "application/json",
        ))
        .expect(1)
        .mount(&responses)
        .await;
    let responses_upstream = Upstream {
        protocol: "openai-responses",
        quirks: None,
        wire_path: "/responses",
        body: "",
        stream: "",
    };
    let dir = scratch();
    let (running, token) = listen(
        &dir,
        &config_text(
            &responses.uri(),
            &responses_upstream,
            "[429, 500, 502, 503, 504]",
        ),
    )
    .await;
    let text = post_text(&client, &running, "/v1/responses", &token, fixture).await;
    assert!(text.contains("ok"), "{text}");
    let seen = responses.received_requests().await.expect("requests");
    let forwarded: serde_json::Value = serde_json::from_slice(&seen[0].body).expect("json");
    assert_eq!(forwarded["tools"], original["tools"]);
    running.shutdown().await;
    fs::remove_dir_all(&dir).ok();

    let chat = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#,
            "application/json",
        ))
        .expect(1)
        .mount(&chat)
        .await;
    let chat_upstream = Upstream {
        protocol: "openai-chat",
        quirks: None,
        wire_path: "/chat/completions",
        body: "",
        stream: "",
    };
    let dir = scratch();
    let (running, token) = listen(
        &dir,
        &config_text(&chat.uri(), &chat_upstream, "[429, 500, 502, 503, 504]"),
    )
    .await;
    let text = post_text(&client, &running, "/v1/responses", &token, fixture).await;
    assert!(text.contains("ok"), "{text}");
    let seen = chat.received_requests().await.expect("requests");
    let forwarded: serde_json::Value = serde_json::from_slice(&seen[0].body).expect("json");
    let tools = forwarded["tools"].as_array().expect("tools");
    assert!(tools.iter().all(|tool| tool["type"] == "function"));
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str())
        .collect();
    assert_eq!(names, vec!["multi_agent_v1__spawn_agent", "apply_patch"]);
    assert!(!forwarded.to_string().contains("web_search"), "{forwarded}");
    running.shutdown().await;
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn bedrock_converse_from_options_is_signed() {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("client");
    let converse = r#"{"output":{"message":{"role":"assistant","content":[{"text":"ok"}]}}}"#;
    let chat = r#"{"model":"demo","messages":[{"role":"user","content":"hi"}]}"#;
    for legacy in [false, true] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/model/demo/converse"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(converse, "application/json"))
            .expect(1)
            .mount(&server)
            .await;
        let keys = if legacy {
            "\
region = \"us-east-1\"
access_key_id = \"AKIDEXAMPLE\"
secret_access_key = \"secret\"
session_token = \"SESSTOKEN\"
"
        } else {
            "\
[providers.options]
region = \"us-east-1\"
access_key_id = \"AKIDEXAMPLE\"
secret_access_key = \"secret\"
session_token = \"SESSTOKEN\"
"
        };
        let text = format!(
            "\
listen = \"127.0.0.1:0\"
token_file = \"token\"
usage_file = \"usage.sqlite\"
[[providers]]
id = \"primary\"
protocol = \"bedrock-converse\"
base_url = \"{base}\"
{keys}[[routes]]
model = \"demo\"
upstreams = [\"primary\"]
",
            base = server.uri(),
        );
        let dir = scratch();
        let (running, token) = listen(&dir, &text).await;
        let body = post_text(&client, &running, "/v1/chat/completions", &token, chat).await;
        assert!(body.contains("ok"), "legacy={legacy}: {body}");
        let seen = server.received_requests().await.expect("requests");
        let authorization = seen[0]
            .headers
            .get("authorization")
            .expect("authorization")
            .to_str()
            .expect("authorization text");
        assert!(
            authorization.starts_with("AWS4-HMAC-SHA256 "),
            "{authorization}"
        );
        assert!(
            authorization.contains("Credential=AKIDEXAMPLE/"),
            "{authorization}"
        );
        assert!(
            authorization.contains("/us-east-1/bedrock/aws4_request"),
            "{authorization}"
        );
        let session = seen[0]
            .headers
            .get("x-amz-security-token")
            .expect("session token")
            .to_str()
            .expect("session text");
        assert_eq!(session, "SESSTOKEN");
        running.shutdown().await;
        fs::remove_dir_all(&dir).ok();
    }
}

#[tokio::test]
async fn vertex_generate_content_runs_through_the_factory() {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("client");
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"access_token":"ya29.test","expires_in":3600}"#,
            "application/json",
        ))
        .expect(1)
        .mount(&server)
        .await;
    let generate = "/v1/projects/demo-project/locations/us-central1/publishers/google/models/demo:generateContent";
    Mock::given(method("POST"))
        .and(path(generate))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"ok"}]}}]}"#,
            "application/json",
        ))
        .expect(1)
        .mount(&server)
        .await;
    let dir = scratch();
    let credentials = dir.join("adc.json");
    fs::write(
        &credentials,
        r#"{"type":"authorized_user","client_id":"client","client_secret":"secret","refresh_token":"refresh-token"}"#,
    )
    .expect("credentials");
    let text = format!(
        "\
listen = \"127.0.0.1:0\"
token_file = \"token\"
usage_file = \"usage.sqlite\"
[[providers]]
id = \"vertex\"
preset = \"google-vertex\"
base_url = \"{base}\"
[providers.options]
project = \"demo-project\"
location = \"us-central1\"
credentials = \"{creds}\"
token_url = \"{base}/token\"
[[routes]]
model = \"demo\"
upstreams = [\"vertex\"]
",
        base = server.uri(),
        creds = credentials.display(),
    );
    let (running, token) = listen(&dir, &text).await;
    let chat = r#"{"model":"demo","messages":[{"role":"user","content":"hi"}]}"#;
    let body = post_text(&client, &running, "/v1/chat/completions", &token, chat).await;
    assert!(body.contains("ok"), "{body}");
    let seen = server.received_requests().await.expect("requests");
    let generate_request = seen
        .iter()
        .find(|request| request.url.path() == generate)
        .expect("generate");
    let authorization = generate_request
        .headers
        .get("authorization")
        .expect("authorization")
        .to_str()
        .expect("authorization text");
    assert_eq!(authorization, "Bearer ya29.test");
    running.shutdown().await;
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn builtin_read_timeout_follows_http_limits() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let port = listener.local_addr().expect("addr").port();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = std::sync::Arc::clone(&stop);
    std::thread::spawn(move || {
        use std::io::Read;
        use std::sync::atomic::Ordering;
        while !flag.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
                    let mut buf = [0_u8; 2048];
                    let _ = stream.read(&mut buf);
                    while !flag.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(_) => break,
            }
        }
    });
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .expect("client");
    let dir = scratch();
    let text = format!(
        "\
listen = \"127.0.0.1:0\"
token_file = \"token\"
usage_file = \"usage.sqlite\"
connect_timeout_ms = 1000
read_timeout_ms = 400
[failover]
connect_errors = false
statuses = []
[[providers]]
id = \"primary\"
protocol = \"openai-chat\"
base_url = \"http://127.0.0.1:{port}\"
api_key = \"test\"
[[routes]]
model = \"demo\"
upstreams = [\"primary\"]
"
    );
    let (running, token) = listen(&dir, &text).await;
    let started = std::time::Instant::now();
    let response = client
        .post(format!("http://{}/v1/chat/completions", running.local_addr))
        .header("authorization", format!("Bearer {token}"))
        .body(r#"{"model":"demo","messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await
        .expect("daemon answered before the test client gave up");
    let elapsed = started.elapsed();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(
        elapsed < Duration::from_secs(2),
        "read budget was not the pooled limit: {elapsed:?}"
    );
    assert_ne!(response.status().as_u16(), 200);
    running.shutdown().await;
    fs::remove_dir_all(&dir).ok();
}

async fn post_text(
    client: &reqwest::Client,
    running: &hofund_daemon::Running,
    route: &str,
    token: &str,
    body: &str,
) -> String {
    let response = client
        .post(format!("http://{}{route}", running.local_addr))
        .header("authorization", format!("Bearer {token}"))
        .body(body.to_owned())
        .send()
        .await
        .expect("send");
    let status = response.status();
    let text = response.text().await.expect("text");
    assert_eq!(status.as_u16(), 200, "{route} {text}");
    text
}

fn event_names(body: &str) -> Vec<&str> {
    body.split("\n\n")
        .filter_map(|frame| frame.lines().find_map(|line| line.strip_prefix("event: ")))
        .collect()
}
