//! The bedrock-mantle plugin on a real wire: header values the recording
//! client does not keep, and a route refusal read from a real error body.

use hofund_kit_upstream::{HttpLimits, runtime_host};
use hofund_plugin_api::{
    BuildContext, HostContext, PluginId, ProbeQuery, ProtocolId, RawTable, RawValue, Request,
    UpstreamProvider,
};
use hofund_plugin_registry::factory;
use serde_json::json;
use std::sync::Arc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn build(
    base: String,
    headers: &[(&str, &str)],
    protocol: ProtocolId,
) -> (tempfile::TempDir, HostContext, Arc<dyn UpstreamProvider>) {
    let root = tempfile::tempdir().expect("root");
    let host = runtime_host(
        PluginId::from_static("bedrock-mantle"),
        "ABSK-wire-key",
        root.path(),
        HttpLimits::default(),
    )
    .expect("host");
    let mut raw = RawTable::new();
    raw.insert("base_url", RawValue::String(base));
    let mut table = RawTable::new();
    for (name, value) in headers {
        table.insert(*name, RawValue::String((*value).to_owned()));
    }
    raw.insert("headers", RawValue::Table(table));
    let provider = factory("bedrock-mantle")
        .expect("bedrock-mantle factory")
        .validate_and_build_with(&raw, &host, &BuildContext::new(protocol))
        .expect("build");
    (root, host, provider)
}

/// Probes a Messages provider built with `headers` and returns the headers
/// the upstream received.
async fn probe_headers(headers: &[(&str, &str)]) -> Vec<(String, String)> {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(b"{}".to_vec(), "application/json"))
        .mount(&server)
        .await;
    let (_root, _host, provider) = build(
        format!("{}/anthropic", server.uri()),
        headers,
        ProtocolId::ANTHROPIC_MESSAGES,
    );
    provider
        .probe(&ProbeQuery::new(ProtocolId::ANTHROPIC_MESSAGES, "claude").expect("query"))
        .await
        .expect("probe");
    let received = server.received_requests().await.expect("recording");
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].url.path(), "/anthropic/v1/messages");
    received[0]
        .headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect()
}

fn values<'a>(headers: &'a [(String, String)], name: &str) -> Vec<&'a str> {
    headers
        .iter()
        .filter(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_messages_probe_sends_the_version_and_the_key_alone() {
    let headers = probe_headers(&[]).await;
    assert_eq!(values(&headers, "anthropic-version"), ["2023-06-01"]);
    assert_eq!(values(&headers, "x-api-key"), ["ABSK-wire-key"]);
    assert!(values(&headers, "authorization").is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn configured_betas_are_fitted_and_configured_headers_win() {
    let headers = probe_headers(&[
        (
            "anthropic-beta",
            "advanced-tool-use-2025-11-20, redact-thinking-2026-02-12",
        ),
        ("anthropic-version", "2099-01-01"),
        ("x-api-key", "ABSK-configured"),
    ])
    .await;
    assert_eq!(
        values(&headers, "anthropic-beta"),
        ["tool-search-tool-2025-10-19"]
    );
    assert_eq!(values(&headers, "anthropic-version"), ["2099-01-01"]);
    assert_eq!(values(&headers, "x-api-key"), ["ABSK-configured"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chat_call_refused_on_its_route_is_answered_at_the_other_prefix() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": {"message": "The model `newmaker.model` isn't supported on this route"}
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/openai/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "c1",
            "object": "chat.completion",
            "model": "newmaker.model",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop"
            }]
        })))
        .expect(2)
        .mount(&server)
        .await;
    let (_root, _host, provider) =
        build(format!("{}/v1", server.uri()), &[], ProtocolId::OPENAI_CHAT);
    for _ in 0..2 {
        let request: Request = serde_json::from_value(json!({
            "model": "newmaker.model",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
        }))
        .expect("request");
        provider
            .execute(request, ProtocolId::OPENAI_CHAT)
            .await
            .expect("answered");
    }
    let received = server.received_requests().await.expect("recording");
    let paths: Vec<&str> = received.iter().map(|request| request.url.path()).collect();
    assert_eq!(
        paths,
        [
            "/v1/chat/completions",
            "/openai/v1/chat/completions",
            "/openai/v1/chat/completions"
        ]
    );
    for request in &received {
        assert_eq!(
            request
                .headers
                .get("authorization")
                .and_then(|value| value.to_str().ok()),
            Some("Bearer ABSK-wire-key")
        );
    }
}

/// A server that refuses every `/v1/...` call as the wrong route and closes
/// every other connection after reading the request. Returns its base and the
/// paths it was asked.
fn refusing_then_closing_server() -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let paths = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = Arc::clone(&paths);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let path = line.split(' ').nth(1).unwrap_or_default().to_owned();
            let mut length = 0usize;
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).is_err() || header == "\r\n" || header.is_empty() {
                    break;
                }
                if let Some((name, value)) = header.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; length];
            let _ = reader.read_exact(&mut body);
            seen.lock().expect("paths").push(path.clone());
            if path.starts_with("/v1/") {
                let reply = r#"{"error":{"message":"The model `newmaker.model` isn't supported on this route"}}"#;
                let _ = write!(
                    stream,
                    "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                    reply.len()
                );
            }
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    });
    (base, paths)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reroute_that_breaks_in_transport_returns_the_refusal_and_forgets_the_route() {
    let (base, paths) = refusing_then_closing_server();
    let (_root, _host, provider) = build(format!("{base}/v1"), &[], ProtocolId::OPENAI_CHAT);
    for _ in 0..2 {
        let request: Request = serde_json::from_value(json!({
            "model": "newmaker.model",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
        }))
        .expect("request");
        let error = provider
            .execute(request, ProtocolId::OPENAI_CHAT)
            .await
            .map(drop)
            .expect_err("refused");
        assert_eq!(error.status(), Some(400), "{error}");
        assert!(
            error.message().contains("supported on this route"),
            "{error}"
        );
    }
    let paths = paths.lock().expect("paths").clone();
    assert_eq!(
        paths.first().map(String::as_str),
        Some("/v1/chat/completions")
    );
    assert!(
        paths.contains(&"/openai/v1/chat/completions".to_owned()),
        "{paths:?}"
    );
    // The route was forgotten, so the second call starts at `/v1` again.
    assert_eq!(
        paths
            .iter()
            .filter(|path| path.as_str() == "/v1/chat/completions")
            .count(),
        2,
        "{paths:?}"
    );
}
