//! Upstream adapters against a local mock. No real network.

use futures_util::StreamExt;
use hofund_ir::{ContentPart, Message, Request, Role, StreamEvent};
use hofund_provider::{
    Anthropic, Completion, Endpoint, OpenAiCompat, OpenAiProtocol, Provider, ProviderErrorKind,
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

async fn text_of(completion: Completion) -> String {
    match completion {
        Completion::Unary(response) => response
            .message
            .content
            .iter()
            .find_map(|part| match part {
                ContentPart::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .unwrap_or_default(),
        Completion::Stream(mut stream) => {
            let mut text = String::new();
            while let Some(item) = stream.next().await {
                if let Ok(StreamEvent::TextDelta(delta)) = item {
                    text.push_str(&delta.text);
                }
            }
            text
        }
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
    let Completion::Stream(mut stream) = provider
        .execute(user_request(true), "openai-responses")
        .await
        .expect("call")
    else {
        panic!("expected a stream");
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

async fn event_has_created(completion: Completion) -> bool {
    let Completion::Stream(mut stream) = completion else {
        return false;
    };
    let mut found = false;
    while let Some(item) = stream.next().await {
        if let Ok(event) = item
            && event
                .extensions()
                .is_some_and(|extensions| extensions.get("created").is_some())
        {
            found = true;
        }
    }
    found
}
