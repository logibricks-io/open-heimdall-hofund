//! Upstream adapters against a local mock. No real network.

use futures_util::StreamExt;
use hofund_ir::{
    ContentPart, Extensions, Message, Request, Role, StreamEvent, ToolArguments, ToolCallPart,
};
use hofund_provider::{
    Anthropic, Completion, CountOutcome, Credential, Endpoint, Generative, OpenAiCompat,
    OpenAiProtocol, Provider, ProviderErrorKind,
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
    assert!(requests[0].headers.get("authorization").is_none());
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
