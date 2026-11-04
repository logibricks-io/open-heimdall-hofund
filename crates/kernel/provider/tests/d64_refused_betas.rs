//! Anthropic beta refusals retried without the named values.

use hofund_ir::{Message, Request, Role};
use hofund_provider::{Anthropic, Endpoint, Provider};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn d64_5_refused_betas_are_retried_without_them_and_stay_off() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_string("unsupported beta header(s): tool-search-tool-2025-10-19"),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "hello"}],
            "stop_reason": "end_turn"
        })))
        .expect(2)
        .mount(&server)
        .await;
    let provider = Anthropic::new(Endpoint::new("relay", server.uri())).expect("provider");
    let mut request = Request::new("claude-opus-4-20250514");
    request.params.max_tokens = Some(20);
    request.messages.push({
        let mut message = Message::new(Role::User);
        message.content.push(hofund_ir::ContentPart::text("ping"));
        message
    });
    request.extensions.insert(
        "anthropic_beta",
        json!(["keep-me", "tool-search-tool-2025-10-19"]),
    );
    provider
        .execute(request.clone(), "anthropic-messages")
        .await
        .expect("first call");
    provider
        .execute(request, "anthropic-messages")
        .await
        .expect("second call");
    let requests = server.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 3);
    for req in &requests[1..] {
        let body: Value = serde_json::from_slice(&req.body).expect("json");
        assert_eq!(body["anthropic_beta"], json!(["keep-me"]));
    }
}
