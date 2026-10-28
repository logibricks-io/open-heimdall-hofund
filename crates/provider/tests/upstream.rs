//! Upstream adapters against a local mock. No real network.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures_util::StreamExt;
use hofund_codec::{AnthropicMessages, Codec, GenerativeLanguage, OpenAiChat, OpenAiResponses};
use hofund_ir::{
    ContentPart, Extensions, Message, Request, Response, Role, StreamEvent, ToolArguments,
    ToolCallPart,
};
use hofund_provider::{
    Anthropic, Completion, CountOutcome, Credential, Endpoint, Generative, OpenAiCompat,
    OpenAiProtocol, Provider, ProviderErrorKind, Vertex,
};
use hofund_quirks::named;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Match, Mock, MockServer, Request as MockRequest, ResponseTemplate};

fn user_request(stream: bool) -> Request {
    let mut request = Request::new("demo");
    let mut message = Message::new(Role::User);
    message.content.push(ContentPart::text("hi"));
    request.messages.push(message);
    request.stream = stream;
    request.params.temperature = Some(0.2);
    request.params.top_p = Some(0.9);
    request
}

struct OmitsSampling;

impl Match for OmitsSampling {
    fn matches(&self, request: &MockRequest) -> bool {
        let Ok(value) = serde_json::from_slice::<Value>(&request.body) else {
            return false;
        };
        value.get("temperature").is_none() && value.get("top_p").is_none()
    }
}

fn text_in(response: &Response) -> String {
    response
        .message
        .content
        .iter()
        .find_map(|part| match part {
            ContentPart::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

fn text_in_events(events: &[StreamEvent]) -> String {
    let mut text = String::new();
    for event in events {
        if let StreamEvent::TextDelta(delta) = event {
            text.push_str(&delta.text);
        }
    }
    text
}

/// Same-protocol completions are raw bytes. Try each codec until one yields text.
fn text_in_raw(bytes: &[u8]) -> String {
    let codecs: [&dyn Codec; 4] = [
        &OpenAiChat,
        &OpenAiResponses,
        &AnthropicMessages,
        &GenerativeLanguage,
    ];
    if bytes.windows(5).any(|window| window == b"data:") {
        for codec in codecs {
            if let Ok(events) = codec.decode_stream(bytes) {
                let text = text_in_events(&events);
                if !text.is_empty() {
                    return text;
                }
            }
        }
    }
    for codec in codecs {
        if let Ok(response) = codec.decode_response(bytes) {
            let text = text_in(&response);
            if !text.is_empty() {
                return text;
            }
        }
    }
    String::new()
}

async fn collect_raw<S, E>(mut stream: S) -> Vec<u8>
where
    S: futures_util::Stream<Item = Result<bytes::Bytes, E>> + Unpin,
    E: std::fmt::Debug,
{
    let mut bytes = Vec::new();
    while let Some(item) = stream.next().await {
        bytes.extend_from_slice(&item.expect("chunk"));
    }
    bytes
}

fn truncated_server(body: &'static [u8]) -> (String, Arc<AtomicUsize>) {
    truncated_server_with_status(200, body)
}

fn truncated_server_with_status(status: u16, body: &'static [u8]) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let hits = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&hits);
    std::thread::spawn(move || {
        let Ok((mut socket, _)) = listener.accept() else {
            return;
        };
        count.fetch_add(1, Ordering::SeqCst);
        let mut request = [0_u8; 8192];
        let _ = socket.read(&mut request);
        let status_line = match status {
            200 => "200 OK",
            429 => "429 Too Many Requests",
            _ => panic!("unsupported test status"),
        };
        let headers = format!(
            "HTTP/1.1 {status_line}\r\nContent-Length: 4096\r\nConnection: close\r\nContent-Type: text/event-stream\r\nRetry-After: 120\r\n\r\n"
        );
        let _ = socket.write_all(headers.as_bytes());
        let _ = socket.write_all(body);
    });
    (format!("http://{address}"), hits)
}

async fn text_of(completion: Completion) -> String {
    match completion {
        Completion::Unary(response) => text_in(&response),
        Completion::Stream(mut stream) => {
            let mut text = String::new();
            while let Some(item) = stream.next().await {
                if let Ok(StreamEvent::TextDelta(delta)) = item {
                    text.push_str(&delta.text);
                }
            }
            text
        }
        Completion::Raw(stream) => text_in_raw(&collect_raw(stream).await),
    }
}

#[tokio::test]
async fn chat_unary_sends_bearer_and_decodes_text() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-1",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "hello"},
                "finish_reason": "stop"
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let mut endpoint = Endpoint::new("primary", server.uri());
    endpoint.api_key = "secret".to_owned();
    let provider = OpenAiCompat::new(OpenAiProtocol::Chat, endpoint).expect("provider");
    let completion = provider
        .execute(user_request(false), "openai-chat")
        .await
        .expect("call");
    assert_eq!(text_of(completion).await, "hello");

    let requests = server.received_requests().await.expect("requests");
    let authorization = requests[0]
        .headers
        .get("authorization")
        .expect("authorization")
        .to_str()
        .expect("text");
    assert_eq!(authorization, "Bearer secret");
}

#[tokio::test]
async fn chat_sends_an_azure_api_key_without_a_bearer_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-1",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "hello"},
                "finish_reason": "stop"
            }]
        })))
        .mount(&server)
        .await;
    let mut endpoint = Endpoint::new("azure", server.uri());
    endpoint.api_key = "secret".to_owned();
    endpoint.credential = Credential::ApiKey;
    let provider = OpenAiCompat::new(OpenAiProtocol::Chat, endpoint).expect("provider");
    provider
        .execute(user_request(false), "openai-chat")
        .await
        .expect("call");
    let requests = server.received_requests().await.expect("requests");
    assert_eq!(
        requests[0]
            .headers
            .get("api-key")
            .expect("api-key")
            .to_str()
            .expect("text"),
        "secret"
    );
    assert!(requests[0].headers.get("authorization").is_none());
}

#[tokio::test]
async fn bedrock_chat_always_sends_max_completion_tokens() {
    for (preset, base_path) in [("bedrock", "/openai/v1"), ("bedrock-mantle", "/v1")] {
        for inbound in ["openai-chat", "anthropic-messages"] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path(format!("{base_path}/chat/completions")))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "id": "chatcmpl-1",
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": "hello"},
                        "finish_reason": "stop"
                    }]
                })))
                .expect(1)
                .mount(&server)
                .await;
            let mut endpoint = Endpoint::new("bedrock", format!("{}{base_path}", server.uri()));
            endpoint.preset = Some(preset.to_owned());
            let provider = OpenAiCompat::new(OpenAiProtocol::Chat, endpoint).expect("provider");
            let mut request = user_request(false);
            request.params.max_tokens = Some(64);
            provider.execute(request, inbound).await.expect("call");

            let requests = server.received_requests().await.expect("requests");
            let body: Value = serde_json::from_slice(&requests[0].body).expect("body");
            assert_eq!(
                body["max_completion_tokens"],
                json!(64),
                "{preset} {inbound}"
            );
            assert!(body.get("max_tokens").is_none(), "{preset} {inbound}");
        }
    }
}

#[tokio::test]
async fn chat_stream_yields_text_deltas() {
    let server = MockServer::start().await;
    let body =
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"}}]}\n\ndata: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let endpoint = Endpoint::new("primary", server.uri());
    let provider = OpenAiCompat::new(OpenAiProtocol::Chat, endpoint).expect("provider");
    let completion = provider
        .execute(user_request(true), "openai-chat")
        .await
        .expect("call");
    assert_eq!(text_of(completion).await, "hello");
}

#[tokio::test]
async fn responses_unary_sets_store_false() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "resp_1",
            "status": "completed",
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "rain"}]
            }]
        })))
        .mount(&server)
        .await;

    let endpoint = Endpoint::new("primary", format!("{}/v1", server.uri()));
    let provider = OpenAiCompat::new(OpenAiProtocol::Responses, endpoint).expect("provider");
    let completion = provider
        .execute(user_request(false), "openai-responses")
        .await
        .expect("call");
    assert_eq!(text_of(completion).await, "rain");

    let requests = server.received_requests().await.expect("requests");
    assert_eq!(requests[0].url.path(), "/v1/responses");
    let body: Value = serde_json::from_slice(&requests[0].body).expect("json");
    assert_eq!(body["store"], json!(false));
    assert!(requests[0].headers.get("authorization").is_none());
}

#[tokio::test]
async fn responses_stream_decodes_output_text() {
    let server = MockServer::start().await;
    let body = concat!(
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"rain\"}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[]}}\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let endpoint = Endpoint::new("primary", server.uri());
    let provider = OpenAiCompat::new(OpenAiProtocol::Responses, endpoint).expect("provider");
    let completion = provider
        .execute(user_request(true), "openai-responses")
        .await
        .expect("call");
    assert_eq!(text_of(completion).await, "rain");
}

#[tokio::test]
async fn leading_failed_responses_stream_is_a_status_error() {
    let server = MockServer::start().await;
    let body = concat!(
        "event: response.failed\n",
        "data: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\",\"error\":{\"message\":\"overloaded\"},\"output\":[]}}\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let endpoint = Endpoint::new("primary", server.uri());
    let provider = OpenAiCompat::new(OpenAiProtocol::Responses, endpoint).expect("provider");
    let same = match provider
        .execute(user_request(true), "openai-responses")
        .await
    {
        Ok(_) => panic!("leading failure fails over"),
        Err(error) => error,
    };
    assert_eq!(same.kind, ProviderErrorKind::Status);
    assert_eq!(same.status, Some(502));
    assert_eq!(same.message, "overloaded");

    let foreign = provider
        .execute(user_request(true), "openai-chat")
        .await
        .expect("cross");
    let Completion::Stream(mut stream) = foreign else {
        panic!("expected a decoded stream");
    };
    let error = stream.next().await.expect("item").expect_err("status");
    assert_eq!(error.kind, ProviderErrorKind::Status);
    assert_eq!(error.status, Some(502));
    assert_eq!(error.message, "overloaded");
}

#[tokio::test]
async fn messages_unary_sends_version_and_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "hello"}],
            "stop_reason": "end_turn"
        })))
        .mount(&server)
        .await;

    let mut endpoint = Endpoint::new("primary", server.uri());
    endpoint.api_key = "secret".to_owned();
    let provider = Anthropic::new(endpoint).expect("provider");
    let mut request = user_request(false);
    request.params.max_tokens = Some(16);
    let completion = provider
        .execute(request, "anthropic-messages")
        .await
        .expect("call");
    assert_eq!(text_of(completion).await, "hello");

    let requests = server.received_requests().await.expect("requests");
    assert_eq!(
        requests[0]
            .headers
            .get("x-api-key")
            .expect("key")
            .to_str()
            .expect("text"),
        "secret"
    );
    assert_eq!(
        requests[0]
            .headers
            .get("anthropic-version")
            .expect("version")
            .to_str()
            .expect("text"),
        "2023-06-01"
    );
    assert!(requests[0].headers.get("authorization").is_none());
}

async fn bedrock_body_with_betas(betas: Value) -> Value {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "hello"}],
            "stop_reason": "end_turn"
        })))
        .expect(1)
        .mount(&server)
        .await;
    let mut endpoint = Endpoint::new("bedrock", server.uri());
    endpoint.preset = Some("bedrock".to_owned());
    let provider = Anthropic::new(endpoint).expect("provider");
    let mut request = user_request(false);
    request.params.max_tokens = Some(16);
    request.extensions.insert("anthropic_beta", betas);
    provider
        .execute(request, "anthropic-messages")
        .await
        .expect("call");
    let requests = server.received_requests().await.expect("requests");
    serde_json::from_slice(&requests[0].body).expect("body")
}

#[tokio::test]
async fn bedrock_messages_split_comma_separated_body_betas() {
    let body = bedrock_body_with_betas(json!([
        "a, prompt-caching-scope-2026-01-05",
        "advanced-tool-use-2025-11-20"
    ]))
    .await;
    assert_eq!(
        body["anthropic_beta"],
        json!(["a", "tool-search-tool-2025-10-19"])
    );
}

#[tokio::test]
async fn bedrock_messages_stringify_non_string_body_betas() {
    for (asked, expected) in [
        (json!(["a", 1]), Some(json!(["a", 1]))),
        (json!([1, "redact-thinking-2026-02-12"]), Some(json!(["1"]))),
        (json!([null, "redact-thinking-2026-02-12"]), None),
    ] {
        let body = bedrock_body_with_betas(asked).await;
        assert_eq!(body.get("anthropic_beta"), expected.as_ref());
    }
}

#[tokio::test]
async fn bedrock_messages_send_key_alone_fit_betas_and_drop_metadata() {
    for (host, preset) in [
        (Some("bedrock-runtime.us-east-1.amazonaws.com"), None),
        (None, Some("bedrock")),
        (None, Some("bedrock-mantle")),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/anthropic/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "msg_1",
                "type": "message",
                "role": "assistant",
                "content": [{"type": "text", "text": "hello"}],
                "stop_reason": "end_turn"
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/anthropic/v1/messages/count_tokens"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"input_tokens": 17})))
            .expect(1)
            .mount(&server)
            .await;

        let base = format!("{}/anthropic", server.uri());
        let mut endpoint = host.map_or_else(
            || Endpoint::new("bedrock", &base),
            |host| {
                let mut endpoint = Endpoint::new(
                    "bedrock",
                    format!("http://{host}:{}/anthropic", server.address().port()),
                );
                endpoint.client = Some(
                    reqwest::Client::builder()
                        .resolve(host, *server.address())
                        .build()
                        .expect("client"),
                );
                endpoint
            },
        );
        endpoint.preset = preset.map(str::to_owned);
        endpoint.api_key = "secret".to_owned();
        endpoint.credential = Credential::MessagesBoth;
        endpoint.headers = vec![(
            "anthropic-beta".to_owned(),
            concat!(
                "advanced-tool-use-2025-11-20, prompt-caching-scope-2026-01-05,",
                "redact-thinking-2026-02-12,context-1m-2025-08-07,",
                "advanced-tool-use-2025-11-20"
            )
            .to_owned(),
        )];
        let provider = Anthropic::new(endpoint).expect("provider");
        let mut request = user_request(false);
        request.params.max_tokens = Some(16);
        request
            .extensions
            .insert("metadata", json!({"user_id": "{\"device_id\":\"local\"}"}));
        request.extensions.insert(
            "anthropic_beta",
            json!([
                "redact-thinking-2026-02-12",
                "advanced-tool-use-2025-11-20",
                "advanced-tool-use-2025-11-20"
            ]),
        );
        provider
            .execute(request.clone(), "anthropic-messages")
            .await
            .expect("call");
        let counted = provider
            .count_input(request, "anthropic-messages")
            .await
            .expect("count");
        assert!(matches!(counted, CountOutcome::Counted(17)));

        let requests = server.received_requests().await.expect("requests");
        assert_eq!(requests.len(), 2);
        for recorded in &requests {
            assert_eq!(
                recorded
                    .headers
                    .get("x-api-key")
                    .and_then(|value| value.to_str().ok()),
                Some("secret")
            );
            assert!(recorded.headers.get("authorization").is_none());
            assert_eq!(
                recorded
                    .headers
                    .get("anthropic-beta")
                    .and_then(|value| value.to_str().ok()),
                Some("tool-search-tool-2025-10-19,context-1m-2025-08-07")
            );
            let body: Value = serde_json::from_slice(&recorded.body).expect("body");
            assert!(body.get("metadata").is_none());
            assert_eq!(
                body["anthropic_beta"],
                json!(["tool-search-tool-2025-10-19"])
            );
        }
    }
}

#[tokio::test]
async fn bedrock_messages_remove_an_empty_body_beta_list() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "hello"}],
            "stop_reason": "end_turn"
        })))
        .expect(1)
        .mount(&server)
        .await;
    let mut endpoint = Endpoint::new("bedrock", server.uri());
    endpoint.preset = Some("bedrock".to_owned());
    let provider = Anthropic::new(endpoint).expect("provider");
    let mut request = user_request(false);
    request.params.max_tokens = Some(16);
    request.extensions.insert(
        "anthropic_beta",
        json!([
            "redact-thinking-2026-02-12",
            "prompt-caching-scope-2026-01-05"
        ]),
    );
    provider
        .execute(request, "anthropic-messages")
        .await
        .expect("call");
    let requests = server.received_requests().await.expect("requests");
    let body: Value = serde_json::from_slice(&requests[0].body).expect("body");
    assert!(body.get("anthropic_beta").is_none());
}

#[tokio::test]
async fn plain_anthropic_messages_keep_body_betas() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "hello"}],
            "stop_reason": "end_turn"
        })))
        .expect(1)
        .mount(&server)
        .await;
    let provider = Anthropic::new(Endpoint::new("plain", server.uri())).expect("provider");
    let mut request = user_request(false);
    request.params.max_tokens = Some(16);
    let betas = json!([
        "redact-thinking-2026-02-12",
        "advanced-tool-use-2025-11-20",
        "advanced-tool-use-2025-11-20"
    ]);
    request.extensions.insert("anthropic_beta", betas.clone());
    provider
        .execute(request, "anthropic-messages")
        .await
        .expect("call");
    let requests = server.received_requests().await.expect("requests");
    let body: Value = serde_json::from_slice(&requests[0].body).expect("body");
    assert_eq!(body["anthropic_beta"], betas);
}

#[tokio::test]
async fn messages_can_send_both_key_headers() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "hello"}],
            "stop_reason": "end_turn"
        })))
        .mount(&server)
        .await;
    let mut endpoint = Endpoint::new("relay", server.uri());
    endpoint.api_key = "secret".to_owned();
    endpoint.credential = Credential::MessagesBoth;
    let provider = Anthropic::new(endpoint).expect("provider");
    let mut request = user_request(false);
    request.params.max_tokens = Some(16);
    provider
        .execute(request, "anthropic-messages")
        .await
        .expect("call");
    let requests = server.received_requests().await.expect("requests");
    assert_eq!(
        requests[0]
            .headers
            .get("x-api-key")
            .expect("key")
            .to_str()
            .expect("text"),
        "secret"
    );
    assert_eq!(
        requests[0]
            .headers
            .get("authorization")
            .expect("bearer")
            .to_str()
            .expect("text"),
        "Bearer secret"
    );
}

#[tokio::test]
async fn messages_stream_yields_text_deltas() {
    let server = MockServer::start().await;
    let body = concat!(
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let endpoint = Endpoint::new("primary", server.uri());
    let provider = Anthropic::new(endpoint).expect("provider");
    let mut request = user_request(true);
    request.params.max_tokens = Some(16);
    let completion = provider
        .execute(request, "anthropic-messages")
        .await
        .expect("call");
    assert_eq!(text_of(completion).await, "hello");
}

#[tokio::test]
async fn non_streaming_body_breaks_are_interrupted_and_not_retried() {
    for inbound in ["openai-chat", "anthropic-messages"] {
        let (base, hits) = truncated_server(br#"{"id":"cut"}"#);
        let provider = OpenAiCompat::new(OpenAiProtocol::Chat, Endpoint::new("primary", base))
            .expect("provider");
        let error = match provider.execute(user_request(false), inbound).await {
            Ok(_) => panic!("expected an interrupted body"),
            Err(error) => error,
        };
        assert_eq!(error.kind, ProviderErrorKind::Interrupted, "{inbound}");
        assert_eq!(hits.load(Ordering::SeqCst), 1, "{inbound}");
    }
}

#[tokio::test]
async fn truncated_429_body_preserves_status_and_retry_after() {
    let (base, hits) = truncated_server_with_status(429, br#"{"error":"#);
    let provider =
        OpenAiCompat::new(OpenAiProtocol::Chat, Endpoint::new("primary", base)).expect("provider");
    let error = match provider.execute(user_request(false), "openai-chat").await {
        Ok(_) => panic!("expected a 429 status error"),
        Err(error) => error,
    };
    assert_eq!(error.kind, ProviderErrorKind::Status);
    assert_eq!(error.status, Some(429));
    assert_eq!(
        error.retry_after(),
        Some(std::time::Duration::from_secs(120))
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn streaming_body_breaks_are_interrupted_for_raw_and_decoded_streams() {
    let body = b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"}}]}\n\n";
    for inbound in ["openai-chat", "anthropic-messages"] {
        let (base, _) = truncated_server(body);
        let provider = OpenAiCompat::new(OpenAiProtocol::Chat, Endpoint::new("primary", base))
            .expect("provider");
        let completion = provider
            .execute(user_request(true), inbound)
            .await
            .expect("headers");
        match completion {
            Completion::Raw(mut stream) => {
                let error = loop {
                    match stream.next().await {
                        Some(Ok(_)) => {}
                        Some(Err(error)) => break error,
                        None => panic!("stream ended before the declared body length"),
                    }
                };
                assert_eq!(error.kind, ProviderErrorKind::Interrupted, "{inbound}");
            }
            Completion::Stream(mut stream) => {
                let error = loop {
                    match stream.next().await {
                        Some(Ok(_)) => {}
                        Some(Err(error)) => break error,
                        None => panic!("stream ended before the declared body length"),
                    }
                };
                assert_eq!(error.kind, ProviderErrorKind::Interrupted, "{inbound}");
            }
            Completion::Unary(_) => panic!("expected a stream"),
        }
    }
}

#[tokio::test]
async fn status_503_is_a_status_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({
            "error": {"message": "busy"}
        })))
        .mount(&server)
        .await;

    let endpoint = Endpoint::new("primary", server.uri());
    let provider = OpenAiCompat::new(OpenAiProtocol::Chat, endpoint).expect("provider");
    let error = match provider.execute(user_request(false), "openai-chat").await {
        Ok(_) => panic!("expected a status error"),
        Err(error) => error,
    };
    assert_eq!(error.kind, ProviderErrorKind::Status);
    assert_eq!(error.status, Some(503));
    assert_eq!(error.message, "busy");
}

#[tokio::test]
async fn execute_status_errors_carry_unbounded_retry_hints() {
    let date = httpdate::fmt_http_date(
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(4_000_000_000),
    );
    for (header, expected) in [
        (Some("120"), Some(120)),
        (Some(date.as_str()), None),
        (Some("7200"), Some(7200)),
        (None, None),
    ] {
        let server = MockServer::start().await;
        let mut response = ResponseTemplate::new(429).set_body_string("slow");
        if let Some(header) = header {
            response = response.insert_header("retry-after", header);
        }
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(response)
            .expect(1)
            .mount(&server)
            .await;
        let provider =
            OpenAiCompat::new(OpenAiProtocol::Chat, Endpoint::new("primary", server.uri()))
                .expect("provider");
        let error = match provider.execute(user_request(false), "openai-chat").await {
            Ok(_) => panic!("status"),
            Err(error) => error,
        };
        if header == Some(date.as_str()) {
            assert!(
                error.retry_after() > Some(std::time::Duration::from_secs(7200)),
                "header: {header:?}"
            );
        } else {
            assert_eq!(
                error.retry_after(),
                expected.map(std::time::Duration::from_secs),
                "header: {header:?}"
            );
        }
    }
}

#[tokio::test]
async fn count_status_errors_carry_retry_hints() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "120")
                .set_body_string("slow"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let provider = Anthropic::new(Endpoint::new("primary", server.uri())).expect("provider");
    let error = match provider
        .count_input(user_request(false), "anthropic-messages")
        .await
    {
        Ok(_) => panic!("status"),
        Err(error) => error,
    };
    assert_eq!(
        error.retry_after(),
        Some(std::time::Duration::from_secs(120))
    );
}

#[tokio::test]
async fn no_sampling_profile_omits_temperature_and_top_p() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(OmitsSampling)
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop"
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let mut endpoint = Endpoint::new("primary", server.uri());
    endpoint.quirks = named("no-sampling");
    let provider = OpenAiCompat::new(OpenAiProtocol::Chat, endpoint).expect("provider");
    provider
        .execute(user_request(false), "openai-chat")
        .await
        .expect("call");
}

#[tokio::test]
async fn extra_header_is_sent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop"
            }]
        })))
        .mount(&server)
        .await;

    let mut endpoint = Endpoint::new("primary", server.uri());
    endpoint
        .headers
        .push(("X-Title".to_owned(), "hofund".to_owned()));
    let provider = OpenAiCompat::new(OpenAiProtocol::Chat, endpoint).expect("provider");
    provider
        .execute(user_request(false), "openai-chat")
        .await
        .expect("call");
    let requests = server.received_requests().await.expect("requests");
    assert_eq!(
        requests[0]
            .headers
            .get("x-title")
            .expect("title")
            .to_str()
            .expect("text"),
        "hofund"
    );
}

#[tokio::test]
async fn chat_stream_sets_include_usage_and_drops_foreign_fields() {
    let server = MockServer::start().await;
    let body = concat!(
        "data: {\"id\":\"chatcmpl-1\",\"created\":1710000000,\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .expect(2)
        .mount(&server)
        .await;

    let endpoint = Endpoint::new("primary", server.uri());
    let provider = OpenAiCompat::new(OpenAiProtocol::Chat, endpoint).expect("provider");
    let mut request = user_request(true);
    request
        .extensions
        .insert("metadata", json!({"user_id": "device-should-not-leak"}));
    let foreign = provider
        .execute(request.clone(), "anthropic-messages")
        .await
        .expect("cross");
    let same = provider
        .execute(request, "openai-chat")
        .await
        .expect("same");
    assert!(!event_has_created(foreign).await);
    assert!(event_has_created(same).await);

    let requests = server.received_requests().await.expect("requests");
    let foreign_body: Value = serde_json::from_slice(&requests[0].body).expect("json");
    let same_body: Value = serde_json::from_slice(&requests[1].body).expect("json");
    assert_eq!(foreign_body["stream_options"]["include_usage"], json!(true));
    assert!(foreign_body.get("metadata").is_none());
    assert!(!foreign_body.to_string().contains("device-should-not-leak"));
    assert_eq!(same_body["stream_options"]["include_usage"], json!(true));
    assert_eq!(
        same_body["metadata"]["user_id"],
        json!("device-should-not-leak")
    );
}

#[tokio::test]
async fn messages_count_is_forwarded_and_a_missing_endpoint_is_unsupported() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"input_tokens": 17})))
        .expect(1)
        .mount(&server)
        .await;
    let mut endpoint = Endpoint::new("primary", server.uri());
    endpoint.api_key = "secret".to_owned();
    let provider = Anthropic::new(endpoint).expect("provider");
    let mut request = user_request(false);
    request.params.max_tokens = Some(16);
    let counted = provider
        .count_input(request.clone(), "anthropic-messages")
        .await
        .expect("count");
    assert!(matches!(counted, CountOutcome::Counted(17)));
    let seen = server.received_requests().await.expect("requests");
    let body: Value = serde_json::from_slice(&seen[0].body).expect("json");
    assert!(body.get("max_tokens").is_none(), "{body}");
    assert_eq!(body["model"], json!("demo"));

    let omitted = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"input_tokens": 4})))
        .expect(1)
        .mount(&omitted)
        .await;
    let provider = Anthropic::new(Endpoint::new("plain", omitted.uri())).expect("provider");
    let mut bare = user_request(false);
    bare.params.max_tokens = None;
    provider
        .count_input(bare, "openai-responses")
        .await
        .expect("count");
    let seen = omitted.received_requests().await.expect("requests");
    let body: Value = serde_json::from_slice(&seen[0].body).expect("json");
    assert!(body.get("max_tokens").is_none(), "{body}");

    let missing = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
        .mount(&missing)
        .await;
    let provider = Anthropic::new(Endpoint::new("backup", missing.uri())).expect("provider");
    let outcome = provider
        .count_input(request, "anthropic-messages")
        .await
        .expect("missing");
    assert!(matches!(outcome, CountOutcome::Unsupported));
}

#[tokio::test]
async fn billing_header_is_stripped_only_when_protocols_differ() {
    let chat = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop"
            }]
        })))
        .mount(&chat)
        .await;
    let chat_provider =
        OpenAiCompat::new(OpenAiProtocol::Chat, Endpoint::new("primary", chat.uri()))
            .expect("chat");
    chat_provider
        .execute(billing_request(true), "anthropic-messages")
        .await
        .expect("cross");
    chat_provider
        .execute(billing_request(false), "anthropic-messages")
        .await
        .expect("line only");
    let seen = chat.received_requests().await.expect("requests");
    let crossed: Value = serde_json::from_slice(&seen[0].body).expect("json");
    assert_eq!(crossed["messages"][0]["content"], json!("You are helpful."));
    assert!(
        crossed["messages"][1]["content"]
            .as_str()
            .expect("user")
            .contains("x-anthropic-billing-header")
    );
    let only: Value = serde_json::from_slice(&seen[1].body).expect("json");
    assert!(!only.to_string().contains("x-anthropic-billing-header"));
    assert_eq!(only["messages"][0]["content"], json!("hi"));

    let messages = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn"
        })))
        .mount(&messages)
        .await;
    let anthropic = Anthropic::new(Endpoint::new("primary", messages.uri())).expect("messages");
    anthropic
        .execute(billing_request(true), "anthropic-messages")
        .await
        .expect("same");
    let seen = messages.received_requests().await.expect("requests");
    let same = String::from_utf8(seen[0].body.clone()).expect("utf8");
    assert!(same.contains("x-anthropic-billing-header"), "{same}");
    assert!(same.contains("You are helpful."), "{same}");
}

fn billing_request(with_rest: bool) -> Request {
    let mut request = Request::new("demo");
    request.params.max_tokens = Some(16);
    let mut system = Message::new(Role::System);
    let line = if with_rest {
        "x-anthropic-billing-header: cc_version=2.1.197; cc_entrypoint=cli; cch=abc;\nYou are helpful."
    } else {
        "x-anthropic-billing-header: cc_version=2.1.197; cc_entrypoint=cli; cch=abc;"
    };
    system.content.push(ContentPart::text(line));
    request.messages.push(system);
    let mut user = Message::new(Role::User);
    user.content.push(ContentPart::text(if with_rest {
        "see x-anthropic-billing-header: cc_version=1; in the middle"
    } else {
        "hi"
    }));
    request.messages.push(user);
    request
}

fn event_created(event: &StreamEvent) -> bool {
    event
        .extensions()
        .is_some_and(|extensions| extensions.get("created").is_some())
}

async fn event_has_created(completion: Completion) -> bool {
    match completion {
        Completion::Stream(mut stream) => {
            let mut found = false;
            while let Some(item) = stream.next().await {
                if let Ok(event) = item
                    && event_created(&event)
                {
                    found = true;
                }
            }
            found
        }
        Completion::Raw(stream) => OpenAiChat
            .decode_stream(&collect_raw(stream).await)
            .map(|events| events.iter().any(event_created))
            .unwrap_or(false),
        Completion::Unary(_) => false,
    }
}

#[tokio::test]
async fn translating_a_bad_tool_call_is_a_client_error() {
    let provider =
        Anthropic::new(Endpoint::new("primary", "http://127.0.0.1:9")).expect("provider");
    let mut request = Request::new("demo");
    let mut message = Message::new(Role::Assistant);
    message.content.push(ContentPart::ToolCall(ToolCallPart {
        id: "c".to_owned(),
        name: "n".to_owned(),
        arguments: ToolArguments::Raw {
            text: "not-json".to_owned(),
        },
        extensions: Extensions::new(),
    }));
    request.messages.push(message);
    let Err(error) = provider.execute(request, "openai-responses").await else {
        panic!("a tool call that is not JSON must not be sent upstream");
    };
    assert_eq!(error.kind, ProviderErrorKind::Status);
    assert_eq!(error.status, Some(400));
}

#[tokio::test]
async fn generative_unary_uses_the_api_key_header_and_omits_model() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1beta/models/demo:generateContent"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": "hello"}]},
                "finishReason": "STOP"
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let mut endpoint = Endpoint::new("studio", server.uri());
    endpoint.api_key = "secret".to_owned();
    let provider = Generative::new(endpoint).expect("provider");
    let completion = provider
        .execute(user_request(false), "generative-language")
        .await
        .expect("call");
    assert_eq!(text_of(completion).await, "hello");

    let requests = server.received_requests().await.expect("requests");
    let request = &requests[0];
    assert_eq!(request.url.path(), "/v1beta/models/demo:generateContent");
    assert!(request.url.query().is_none());
    assert_eq!(
        request
            .headers
            .get("x-goog-api-key")
            .expect("key")
            .to_str()
            .expect("text"),
        "secret"
    );
    assert!(request.headers.get("authorization").is_none());
    let body: Value = serde_json::from_slice(&request.body).expect("json");
    assert!(body.get("model").is_none());
    assert!(body.get("stream").is_none());
    assert!(body.get("contents").is_some());
}

#[tokio::test]
async fn generative_stream_uses_sse_and_a_pasted_base_is_cut_back() {
    let server = MockServer::start().await;
    let body = "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"hello\"}]}}]}\n\n";
    Mock::given(method("POST"))
        .and(path(
            "/proxy/v1beta/models/gemini-2.5-pro:streamGenerateContent",
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .expect(1)
        .mount(&server)
        .await;

    let base = format!(
        "{}/proxy/v1beta/models/gemini-2.5-flash:generateContent",
        server.uri()
    );
    let mut endpoint = Endpoint::new("studio", base);
    endpoint.api_key = "secret".to_owned();
    let provider = Generative::new(endpoint).expect("provider");
    let mut request = user_request(true);
    request.model = "models/gemini-2.5-pro".to_owned();
    let completion = provider
        .execute(request, "openai-chat")
        .await
        .expect("call");
    assert_eq!(text_of(completion).await, "hello");

    let requests = server.received_requests().await.expect("requests");
    assert_eq!(
        requests[0].url.path(),
        "/proxy/v1beta/models/gemini-2.5-pro:streamGenerateContent"
    );
    assert_eq!(requests[0].url.query(), Some("alt=sse"));
    let body: Value = serde_json::from_slice(&requests[0].body).expect("json");
    assert!(body.get("model").is_none());
    assert!(body.get("stream").is_none());
}

#[tokio::test]
async fn generative_count_reads_total_tokens_and_a_missing_method_is_unsupported() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1beta/models/demo:countTokens"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"totalTokens": 17})))
        .expect(1)
        .mount(&server)
        .await;

    let endpoint = Endpoint::new("studio", format!("{}/v1beta", server.uri()));
    let provider = Generative::new(endpoint).expect("provider");
    let counted = provider
        .count_input(user_request(false), "generative-language")
        .await
        .expect("count");
    assert!(matches!(counted, CountOutcome::Counted(17)));

    let requests = server.received_requests().await.expect("requests");
    let body: Value = serde_json::from_slice(&requests[0].body).expect("json");
    assert!(body.get("model").is_none());
    assert!(requests[0].headers.get("authorization").is_none());
    assert!(requests[0].headers.get("x-goog-api-key").is_none());

    let missing = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1beta/models/demo:countTokens"))
        .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
        .mount(&missing)
        .await;
    let provider = Generative::new(Endpoint::new("studio", missing.uri())).expect("provider");
    let outcome = provider
        .count_input(user_request(false), "generative-language")
        .await
        .expect("missing");
    assert!(matches!(outcome, CountOutcome::Unsupported));
}

struct Credentials {
    _guard: tempfile::TempDir,
    path: std::path::PathBuf,
}

impl std::ops::Deref for Credentials {
    type Target = std::path::Path;

    fn deref(&self) -> &Self::Target {
        &self.path
    }
}

fn creds_file(_name: &str, body: &Value) -> Credentials {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("adc.json");
    std::fs::write(&path, serde_json::to_vec(body).expect("json")).expect("write");
    Credentials { _guard: dir, path }
}

fn vertex_at(server: &MockServer, creds: &std::path::Path, impersonate: &str) -> Vertex {
    let base = format!("{}/v1/projects/demo-project/locations/global", server.uri());
    Vertex::with_token_endpoints(
        Endpoint::new("vertex", base),
        creds.display().to_string(),
        impersonate,
        format!("{}/token", server.uri()),
        format!("{}/iam/v1", server.uri()),
    )
    .expect("vertex")
}

fn bearer(request: &wiremock::Request) -> &str {
    request
        .headers
        .get("authorization")
        .expect("authorization")
        .to_str()
        .expect("text")
}

#[tokio::test]
async fn vertex_unary_mints_a_refresh_token_and_reuses_it() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "ya29.minted",
            "expires_in": 3600
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(
            "/v1/projects/demo-project/locations/global/publishers/google/models/demo:generateContent",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": "hello"}]},
                "finishReason": "STOP"
            }]
        })))
        .expect(2)
        .mount(&server)
        .await;

    let creds = creds_file(
        "refresh",
        &json!({
            "type": "authorized_user",
            "client_id": "client",
            "client_secret": "secret",
            "refresh_token": "refresh-token"
        }),
    );
    let provider = vertex_at(&server, &creds, "");
    let completion = provider
        .execute(user_request(false), "generative-language")
        .await
        .expect("call");
    assert_eq!(text_of(completion).await, "hello");
    provider
        .execute(user_request(false), "generative-language")
        .await
        .expect("cached");

    let requests = server.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 3);
    let form = String::from_utf8(requests[0].body.clone()).expect("form");
    assert!(form.contains("grant_type=refresh_token"), "{form}");
    assert!(form.contains("refresh_token=refresh-token"), "{form}");
    assert!(requests[0].headers.get("authorization").is_none());
    assert_eq!(bearer(&requests[1]), "Bearer ya29.minted");
    assert!(requests[1].headers.get("x-goog-api-key").is_none());
    let body: Value = serde_json::from_slice(&requests[1].body).expect("json");
    assert!(body.get("model").is_none());
    assert!(body.get("stream").is_none());
    assert_eq!(bearer(&requests[2]), "Bearer ya29.minted");
    let _ = std::fs::remove_dir_all(creds.parent().expect("parent"));
}

#[tokio::test]
async fn vertex_service_account_signs_a_jwt_and_streams() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "ya29.signed",
            "expires_in": 3600
        })))
        .expect(1)
        .mount(&server)
        .await;
    let sse = "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"hello\"}]}}]}\n\n";
    Mock::given(method("POST"))
        .and(path(
            "/v1/projects/demo-project/locations/global/publishers/google/models/gemini-2.5-pro:streamGenerateContent",
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .expect(1)
        .mount(&server)
        .await;

    let pem = include_str!("fixtures/vertex-sa-pkcs8.pem");
    let creds = creds_file(
        "sa",
        &json!({
            "type": "service_account",
            "client_email": "sa@example.com",
            "private_key": pem,
            "private_key_id": "key-1"
        }),
    );
    let provider = vertex_at(&server, &creds, "");
    let mut request = user_request(true);
    request.model = "gemini-2.5-pro".to_owned();
    let completion = provider
        .execute(request, "openai-chat")
        .await
        .expect("stream");
    assert_eq!(text_of(completion).await, "hello");

    let requests = server.received_requests().await.expect("requests");
    let form = String::from_utf8(requests[0].body.clone()).expect("form");
    assert!(
        form.contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer"),
        "{form}"
    );
    assert!(form.contains("assertion=eyJ"), "{form}");
    assert_eq!(requests[1].url.query(), Some("alt=sse"));
    assert_eq!(bearer(&requests[1]), "Bearer ya29.signed");
    let _ = std::fs::remove_dir_all(creds.parent().expect("parent"));
}

#[tokio::test]
async fn vertex_impersonation_sends_the_traded_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "source-token",
            "expires_in": 3600
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(
            "/iam/v1/projects/-/serviceAccounts/worker@example.com:generateAccessToken",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "accessToken": "impersonated-token",
            "expireTime": "2026-10-25T08:00:00Z"
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(
            "/v1/projects/demo-project/locations/global/publishers/google/models/demo:countTokens",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"totalTokens": 9})))
        .expect(1)
        .mount(&server)
        .await;

    let creds = creds_file(
        "impersonate",
        &json!({
            "type": "authorized_user",
            "client_id": "client",
            "client_secret": "secret",
            "refresh_token": "refresh-token"
        }),
    );
    let provider = vertex_at(&server, &creds, "worker@example.com");
    let counted = provider
        .count_input(user_request(false), "generative-language")
        .await
        .expect("count");
    assert!(matches!(counted, CountOutcome::Counted(9)));

    let requests = server.received_requests().await.expect("requests");
    assert_eq!(bearer(&requests[1]), "Bearer source-token");
    let body: Value = serde_json::from_slice(&requests[1].body).expect("iam");
    assert_eq!(body["lifetime"], "3600s");
    assert_eq!(bearer(&requests[2]), "Bearer impersonated-token");
    let _ = std::fs::remove_dir_all(creds.parent().expect("parent"));
}

#[tokio::test]
async fn vertex_impersonation_forbidden_names_the_token_creator_role() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "source-token",
            "expires_in": 3600
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(
            "/iam/v1/projects/-/serviceAccounts/worker@example.com:generateAccessToken",
        ))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "error": {"message": "Permission denied"}
        })))
        .mount(&server)
        .await;
    let creds = creds_file(
        "forbidden",
        &json!({
            "type": "authorized_user",
            "client_id": "client",
            "refresh_token": "refresh-token"
        }),
    );
    let provider = vertex_at(&server, &creds, "worker@example.com");
    let Err(error) = provider
        .execute(user_request(false), "generative-language")
        .await
    else {
        panic!("impersonation should have been forbidden");
    };
    let message = error.to_string();
    assert!(
        message.contains("roles/iam.serviceAccountTokenCreator"),
        "{message}"
    );
    assert!(message.contains("worker@example.com"), "{message}");
    let _ = std::fs::remove_dir_all(creds.parent().expect("parent"));
}

#[tokio::test]
async fn vertex_impersonation_token_errors_redact_tokens_and_response_bodies() {
    const SOURCE_TOKEN: &str = "SENTINEL-SOURCE-TOKEN-7731";
    const BODY_SENTINEL: &str = "SENTINEL-BODY-7731";
    let bodies = [
        (
            json!({"detail": BODY_SENTINEL}).to_string(),
            "no accessToken",
        ),
        (format!("not-json {BODY_SENTINEL}"), "not JSON"),
    ];
    for (index, (body, expected)) in bodies.into_iter().enumerate() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": SOURCE_TOKEN,
                "expires_in": 3600
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(
                "/iam/v1/projects/-/serviceAccounts/worker@example.com:generateAccessToken",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .expect(1)
            .mount(&server)
            .await;
        let creds = creds_file(
            &format!("impersonation-error-{index}"),
            &json!({
                "type": "authorized_user",
                "client_id": "client",
                "refresh_token": "refresh-token"
            }),
        );
        let provider = vertex_at(&server, &creds, "worker@example.com");
        let Err(error) = provider
            .execute(user_request(false), "generative-language")
            .await
        else {
            panic!("an invalid IAM token response must fail");
        };
        let message = error.to_string();
        assert!(
            message.contains("impersonating worker@example.com"),
            "{message}"
        );
        assert!(message.contains(expected), "{message}");
        assert!(!message.contains(BODY_SENTINEL), "{message}");
        assert!(!message.contains(SOURCE_TOKEN), "{message}");
        let _ = std::fs::remove_dir_all(creds.parent().expect("parent"));
    }
}

#[tokio::test]
async fn vertex_uses_a_credentials_file_created_after_construction() {
    let dir = tempfile::tempdir().expect("temp dir");
    let credentials = dir.path().join("adc.json");
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "ya29.late-file",
            "expires_in": 3600
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(
            "/v1/projects/demo-project/locations/global/publishers/google/models/demo:generateContent",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": "hello"}]},
                "finishReason": "STOP"
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let provider = vertex_at(&server, &credentials, "");
    let Err(error) = provider
        .execute(user_request(false), "generative-language")
        .await
    else {
        panic!("the missing credentials file must fail at request time");
    };
    let message = error.to_string();
    assert!(message.contains("no Google credentials at"), "{message}");
    assert!(
        message.contains("clear it to use Application Default Credentials"),
        "{message}"
    );

    let pem = include_str!("fixtures/vertex-sa-pkcs8.pem");
    std::fs::write(
        &credentials,
        serde_json::to_vec(&json!({
            "type": "service_account",
            "client_email": "sa@example.com",
            "private_key": pem,
            "private_key_id": "key-1"
        }))
        .expect("json"),
    )
    .expect("late credentials");
    let completion = provider
        .execute(user_request(false), "generative-language")
        .await
        .expect("late file is read without rebuilding");
    assert_eq!(text_of(completion).await, "hello");
    let requests = server.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 2);
    assert_eq!(bearer(&requests[1]), "Bearer ya29.late-file");
}

#[tokio::test]
async fn vertex_rejects_a_missing_file_workload_identity_and_an_api_key() {
    let directory = tempfile::tempdir().expect("temp dir");
    let missing = directory.path().join("adc.json");
    let provider = Vertex::new(
        Endpoint::new(
            "vertex",
            "https://aiplatform.googleapis.com/v1/projects/demo-project/locations/global",
        ),
        missing.display().to_string(),
        "",
    )
    .expect("build");
    let Err(error) = provider
        .execute(user_request(false), "generative-language")
        .await
    else {
        panic!("missing credentials must fail at request time");
    };
    assert!(
        error.to_string().contains("no Google credentials at"),
        "{error}"
    );
    assert!(
        error
            .to_string()
            .contains("clear it to use Application Default Credentials"),
        "{error}"
    );

    let server = MockServer::start().await;
    let creds = creds_file(
        "wif",
        &json!({
            "type": "external_account",
            "universe_domain": "googleapis.com"
        }),
    );
    let provider = vertex_at(&server, &creds, "");
    let Err(error) = provider
        .execute(user_request(false), "generative-language")
        .await
    else {
        panic!("workload identity federation should be refused");
    };
    assert!(
        error.to_string().contains("workload identity federation"),
        "{error}"
    );
    let mut endpoint = Endpoint::new("vertex", format!("{}/v1", server.uri()));
    endpoint.api_key = "secret".to_owned();
    let error = Vertex::new(endpoint, "", "").expect_err("key");
    assert!(error.to_string().contains("not an API key"));
    let _ = std::fs::remove_dir_all(creds.parent().expect("parent"));
}
