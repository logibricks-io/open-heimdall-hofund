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
    Anthropic, AttemptHook, AttemptRoute, Completion, CountOutcome, Credential, Endpoint,
    Generative, OpenAiCompat, OpenAiProtocol, Provider, ProviderErrorKind,
};
use hofund_quirks::named;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Match, Mock, MockServer, Request as MockRequest, Respond, ResponseTemplate};

struct RefuseOnce {
    refusal: &'static str,
    success: Value,
    calls: AtomicUsize,
}

impl Respond for RefuseOnce {
    fn respond(&self, _request: &MockRequest) -> ResponseTemplate {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            ResponseTemplate::new(400).set_body_string(self.refusal)
        } else {
            ResponseTemplate::new(200).set_body_json(self.success.clone())
        }
    }
}

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
    assert_eq!(same.status, Some(529));
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
    assert_eq!(error.status, Some(529));
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
async fn v2_deepseek_chat_preserves_both_caps_and_rewrites_patterns() {
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
        .expect(1)
        .mount(&server)
        .await;
    let mut endpoint = Endpoint::new("deepseek", server.uri());
    endpoint.preset = Some("deepseek".to_owned());
    let provider = OpenAiCompat::new(OpenAiProtocol::Chat, endpoint).expect("provider");
    let request = OpenAiChat
        .decode_request(br#"{"model":"deepseek-chat","messages":[],"max_tokens":10,"max_completion_tokens":20,"tools":[{"type":"function","function":{"name":"Artifact","parameters":{"pattern":"^[^\\0]*$"}}}]}"#)
        .expect("request");
    provider
        .execute(request, "openai-chat")
        .await
        .expect("call");
    let requests = server.received_requests().await.expect("requests");
    let body: Value = serde_json::from_slice(&requests[0].body).expect("body");
    assert_eq!(body["max_tokens"], 10);
    assert_eq!(body["max_completion_tokens"], 20);
    assert_eq!(
        body["tools"][0]["function"]["parameters"]["pattern"],
        r"^[^\u0000]*$"
    );
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

struct DeltaRequest;

impl Match for DeltaRequest {
    fn matches(&self, request: &MockRequest) -> bool {
        serde_json::from_slice::<Value>(&request.body)
            .ok()
            .is_some_and(|body| body["previous_response_id"] == "resp_up1")
    }
}

struct WholeRequest;

impl Match for WholeRequest {
    fn matches(&self, request: &MockRequest) -> bool {
        serde_json::from_slice::<Value>(&request.body)
            .ok()
            .is_some_and(|body| body.get("previous_response_id").is_none())
    }
}

struct Shorten;

impl AttemptHook for Shorten {
    fn prepare(&self, _route: &AttemptRoute, body: &[u8]) -> Option<Vec<u8>> {
        let mut body: Value = serde_json::from_slice(body).ok()?;
        body["input"] = json!([]);
        body["previous_response_id"] = json!("resp_up1");
        serde_json::to_vec(&body).ok()
    }

    fn refused(&self, _route: &AttemptRoute, status: u16, body: &[u8]) -> bool {
        matches!(status, 400 | 404 | 422)
            && String::from_utf8_lossy(body).contains("previous_response")
    }
}

/// Says every failure is a refusal, so only the provider's own status filter decides.
struct AnyRefusal;

impl AttemptHook for AnyRefusal {
    fn prepare(&self, route: &AttemptRoute, body: &[u8]) -> Option<Vec<u8>> {
        Shorten.prepare(route, body)
    }

    fn refused(&self, _route: &AttemptRoute, _status: u16, _body: &[u8]) -> bool {
        true
    }
}

#[tokio::test]
async fn a11_provider_resends_the_whole_body_after_a_refused_delta() {
    for status in [400, 404, 422] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .and(DeltaRequest)
            .respond_with(
                ResponseTemplate::new(status)
                    .set_body_string(r#"{"error":{"code":"previous_response_not_found"}}"#),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .and(WholeRequest)
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "resp_up2",
                "status": "completed",
                "output": [{"type":"message","role":"assistant","content":[{"type":"output_text","text":"whole"}]}]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let endpoint = Endpoint::new("primary", format!("{}/v1", server.uri()));
        let provider = OpenAiCompat::new(OpenAiProtocol::Responses, endpoint).expect("provider");
        let completion = provider
            .execute_with_attempt(user_request(false), "openai-responses", Some(&Shorten))
            .await
            .expect("whole retry");
        assert_eq!(text_of(completion).await, "whole", "{status}");
        let requests = server.received_requests().await.expect("requests");
        assert_eq!(requests.len(), 2, "{status}");
        let whole: Value = serde_json::from_slice(&requests[1].body).expect("whole JSON");
        assert!(whole["input"].to_string().contains("hi"));
    }
}

#[tokio::test]
async fn a11_provider_does_not_resend_after_other_statuses() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(409)
                .set_body_string(r#"{"error":{"code":"previous_response_not_found"}}"#),
        )
        .expect(1)
        .mount(&server)
        .await;
    let endpoint = Endpoint::new("primary", format!("{}/v1", server.uri()));
    let provider = OpenAiCompat::new(OpenAiProtocol::Responses, endpoint).expect("provider");
    let outcome = provider
        .execute_with_attempt(user_request(false), "openai-responses", Some(&AnyRefusal))
        .await;
    assert!(outcome.is_err());
    assert_eq!(server.received_requests().await.expect("requests").len(), 1);
}

#[tokio::test]
async fn a6_nonstreamed_200_error_fails_as_its_mapped_status() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "error": {
                "message": "Rate limit reached on tokens per min (TPM)",
                "code": "rate_limit_exceeded"
            }
        })))
        .expect(2)
        .mount(&server)
        .await;
    let provider = OpenAiCompat::new(OpenAiProtocol::Chat, Endpoint::new("relay", server.uri()))
        .expect("provider");
    for inbound in ["openai-chat", "anthropic-messages"] {
        let error = provider
            .execute(user_request(false), inbound)
            .await
            .err()
            .expect("carried error");
        assert_eq!(error.status, Some(429), "{inbound}");
        assert!(!error.is_credit(), "{inbound}");
    }
}

#[tokio::test]
async fn a7_finish_reason_error_fails_raw_and_translated_streams() {
    let server = MockServer::start().await;
    let body = concat!(
        "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"}}]}\n\n",
        "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"error\"}]}\n\n",
        "data: [DONE]\n\n"
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
    let provider = OpenAiCompat::new(OpenAiProtocol::Chat, Endpoint::new("relay", server.uri()))
        .expect("provider");
    let error = provider
        .execute(user_request(true), "openai-chat")
        .await
        .err()
        .expect("raw finish error");
    assert_eq!(error.status, Some(502));
    assert!(!error.cools());

    let completion = provider
        .execute(user_request(true), "anthropic-messages")
        .await
        .expect("translated stream");
    let Completion::Stream(mut events) = completion else {
        panic!("stream");
    };
    let error = loop {
        match events.next().await {
            Some(Ok(_)) => continue,
            Some(Err(error)) => break error,
            None => panic!("finish error"),
        }
    };
    assert_eq!(error.status, Some(502));
    assert!(!error.cools());
}

#[tokio::test]
async fn v4_root_union_is_retried_folded_and_learned() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(RefuseOnce {
            refusal: r#"{"error":{"message":"Invalid schema for function 'automation_update': schema must have type 'object' and not have 'oneOf'/'anyOf'/'allOf'/'enum'/'const'/'not' at the top level.","type":"invalid_request_error","code":"invalid_function_parameters"}}"#,
            success: json!({
                "id": "r1",
                "object": "response",
                "status": "completed",
                "output": [],
                "usage": {"input_tokens": 1, "output_tokens": 1}
            }),
            calls: AtomicUsize::new(0),
        })
        .expect(3)
        .mount(&server)
        .await;
    let provider = OpenAiCompat::new(
        OpenAiProtocol::Responses,
        Endpoint::new("relay", server.uri()),
    )
    .expect("provider");
    let request = OpenAiResponses
        .decode_request(br#"{"model":"grok-4.7","input":[],"tools":[{"type":"function","name":"automation_update","parameters":{"anyOf":[{"type":"object","properties":{"prompt":{"type":"string"},"mode":{"type":"string"}},"required":["prompt"]},{"type":"object","properties":{"id":{"type":"string"}},"required":["id"]}]}}]}"#)
        .expect("request");
    for _ in 0..2 {
        provider
            .execute(request.clone(), "openai-responses")
            .await
            .expect("folded response");
    }
    let requests = server.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 3);
    for request in &requests[1..] {
        let body: Value = serde_json::from_slice(&request.body).expect("body");
        let parameters = &body["tools"][0]["parameters"];
        assert_eq!(parameters["type"], "object");
        assert!(parameters.get("anyOf").is_none());
        assert_eq!(parameters["properties"]["prompt"]["type"], "string");
        assert_eq!(parameters["properties"]["mode"]["type"], "string");
        assert!(parameters.get("required").is_none());
    }
}

#[tokio::test]
async fn v5_chat_token_field_is_retried_and_learned() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(RefuseOnce {
            refusal: r#"{"error":{"message":"Unsupported parameter: 'max_tokens' is not supported with this model. Use 'max_completion_tokens' instead.","type":"invalid_request_error","param":"max_tokens","code":"unsupported_parameter"}}"#,
            success: json!({
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "ok"},
                    "finish_reason": "stop"
                }]
            }),
            calls: AtomicUsize::new(0),
        })
        .expect(3)
        .mount(&server)
        .await;
    let provider = OpenAiCompat::new(OpenAiProtocol::Chat, Endpoint::new("relay", server.uri()))
        .expect("provider");
    let request = OpenAiChat
        .decode_request(br#"{"model":"gpt-6.1-sol","max_tokens":4096,"messages":[{"role":"user","content":"hi"}]}"#)
        .expect("request");
    for _ in 0..2 {
        provider
            .execute(request.clone(), "openai-chat")
            .await
            .expect("completion");
    }
    let requests = server.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 3);
    for request in &requests[1..] {
        let body: Value = serde_json::from_slice(&request.body).expect("body");
        assert!(body.get("max_tokens").is_none());
        assert_eq!(body["max_completion_tokens"], 4096);
    }
}

const D10_REFUSAL: &str = r#"{"error":{"code":null,"message":"[ObjectParam] [input[2].internal_chat_message_metadata_passthrough.content_token_count] [unknown_parameter] Unknown parameter: 'input[2].internal_chat_message_metadata_passthrough.content_token_count'.","param":null,"type":"invalid_request_error"}}"#;

fn d10_request() -> Request {
    OpenAiResponses
        .decode_request(
            br#"{"model":"gpt-6","stream":false,"input":[
            {"type":"message","role":"developer","content":[{"type":"input_text","text":"be brief"}],"internal_chat_message_metadata_passthrough":{"turn_id":"t1","content_token_count":[3]}},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"again"}],"internal_chat_message_metadata_passthrough":{"turn_id":"t2","content_item_kinds":["text"],"content_token_count":[1]}}
            ]}"#,
        )
        .expect("request")
}

fn d10_success() -> Value {
    json!({
        "id": "r1",
        "object": "response",
        "status": "completed",
        "output": [],
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
}

fn d10_has_token_count(body: &[u8]) -> bool {
    let value: Value = serde_json::from_slice(body).expect("body");
    value["input"].as_array().is_some_and(|items| {
        items.iter().any(|item| {
            item.pointer("/internal_chat_message_metadata_passthrough/content_token_count")
                .is_some()
        })
    })
}

#[tokio::test]
async fn d10f_routed_responses_refused_member_is_dropped_and_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(RefuseOnce {
            refusal: D10_REFUSAL,
            success: d10_success(),
            calls: AtomicUsize::new(0),
        })
        .expect(2)
        .mount(&server)
        .await;
    let provider = OpenAiCompat::new(
        OpenAiProtocol::Responses,
        Endpoint::new("relay", server.uri()),
    )
    .expect("provider");
    provider
        .execute(d10_request(), "openai-responses")
        .await
        .expect("retry");
    let requests = server.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 2);
    assert!(d10_has_token_count(&requests[0].body));
    assert!(!d10_has_token_count(&requests[1].body));
    let retry: Value = serde_json::from_slice(&requests[1].body).expect("retry");
    assert_eq!(retry["input"].as_array().map(Vec::len), Some(3));
    assert_eq!(
        retry.pointer("/input/0/internal_chat_message_metadata_passthrough/turn_id"),
        Some(&json!("t1"))
    );
    assert_eq!(
        retry.pointer("/input/2/internal_chat_message_metadata_passthrough/content_item_kinds/0"),
        Some(&json!("text"))
    );
    assert_eq!(
        retry.pointer("/input/2/content/0/text"),
        Some(&json!("again"))
    );
}

#[tokio::test]
async fn d10g_routed_learned_member_is_left_out_next_time() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(RefuseOnce {
            refusal: D10_REFUSAL,
            success: d10_success(),
            calls: AtomicUsize::new(0),
        })
        .expect(3)
        .mount(&server)
        .await;
    let provider = OpenAiCompat::new(
        OpenAiProtocol::Responses,
        Endpoint::new("relay", server.uri()),
    )
    .expect("provider");
    for _ in 0..2 {
        provider
            .execute(d10_request(), "openai-responses")
            .await
            .expect("completion");
    }
    let requests = server.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 3);
    assert!(d10_has_token_count(&requests[0].body));
    assert!(!d10_has_token_count(&requests[1].body));
    assert!(!d10_has_token_count(&requests[2].body));
}

#[tokio::test]
async fn d10h_routed_second_refusal_of_same_member_is_returned() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(400).set_body_string(D10_REFUSAL))
        .expect(2)
        .mount(&server)
        .await;
    let provider = OpenAiCompat::new(
        OpenAiProtocol::Responses,
        Endpoint::new("relay", server.uri()),
    )
    .expect("provider");
    let error = provider
        .execute(d10_request(), "openai-responses")
        .await
        .err()
        .expect("second refusal");
    assert_eq!(error.status, Some(400));
    assert_eq!(server.received_requests().await.expect("requests").len(), 2);
}

#[test]
fn d10i_refusal_wordings_match_the_reference() {
    for (body, expected) in [
        (D10_REFUSAL, Some("content_token_count")),
        (
            "Codex: [ObjectParam] [input[55].internal_chat_message_metadata_passthrough.content_token_count] [unknown_parameter] Unknown parameter: 'input[55].internal_chat_message_metadata_passthrough.content_token_count'.",
            Some("content_token_count"),
        ),
        (
            r#"{"error":{"message":"Unknown parameter: 'input[0].internal_chat_message_metadata_passthrough.content_item_metadata'.","param":"input[0].internal_chat_message_metadata_passthrough.content_item_metadata","code":"unknown_parameter"}}"#,
            Some("content_item_metadata"),
        ),
        (
            r#"{"error":{"message":"Unknown parameter: 'input[4].internal_chat_message_metadata_passthrough'.","code":"unknown_parameter"}}"#,
            Some(""),
        ),
        (
            r#"{"error":{"message":"Unknown parameter: '\u0069nput[0].internal_chat_message_metadata_passthrough.content_item_metadata'.","code":"unknown_parameter"}}"#,
            Some("content_item_metadata"),
        ),
    ] {
        assert_eq!(
            hofund_quirks::refused_passthrough(body.as_bytes()).as_deref(),
            expected,
            "{body}"
        );
    }
    for body in [
        r#"{"error":{"message":"Unknown parameter: 'input[0].foo'.","code":"unknown_parameter"}}"#,
        r#"{"error":{"message":"Invalid type for 'input[0].internal_chat_message_metadata_passthrough.turn_id': expected a string.","code":"invalid_type"}}"#,
        r#"{"error":{"message":"Unsupported parameter: 'store'."}}"#,
    ] {
        assert_eq!(
            hofund_quirks::refused_passthrough(body.as_bytes()),
            None,
            "{body}"
        );
    }
}

#[tokio::test]
async fn d10j_cross_protocol_request_is_untouched() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(400).set_body_string(D10_REFUSAL))
        .expect(1)
        .mount(&server)
        .await;
    let provider = OpenAiCompat::new(
        OpenAiProtocol::Responses,
        Endpoint::new("relay", server.uri()),
    )
    .expect("provider");
    let error = provider
        .execute(d10_request(), "anthropic-messages")
        .await
        .err()
        .expect("cross-protocol refusal");
    assert_eq!(error.status, Some(400));
    let requests = server.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 1);
    assert!(!d10_has_token_count(&requests[0].body));
}
