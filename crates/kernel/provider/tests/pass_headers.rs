//! Per-provider pass_headers behaviour.

use std::collections::BTreeMap;

use hofund_ir::{ContentPart, Message, Request, Role};
use hofund_provider::{Endpoint, OpenAiCompat, OpenAiProtocol, Provider, pass_headers};
use reqwest::header::HeaderMap;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn user_request() -> Request {
    let mut request = Request::new("m");
    let mut message = Message::new(Role::User);
    message.content.push(ContentPart::text("hi"));
    request.messages.push(message);
    request
}

#[tokio::test]
async fn d62_1_listed_header_forwarded() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("x-route", "from-client"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "ok"}}]
        })))
        .mount(&server)
        .await;

    let mut endpoint = Endpoint::new("alpha".to_owned(), format!("{}/v1", server.uri()));
    endpoint.pass_headers = vec!["x-route".to_owned()];
    endpoint.headers = vec![("x-route".to_owned(), "fixed".to_owned())];
    let provider = OpenAiCompat::new(OpenAiProtocol::Chat, endpoint).expect("provider");
    let mut request = user_request();
    request.extensions.insert(
        pass_headers::CLIENT_MAP,
        serde_json::json!({"x-route": "from-client"}),
    );
    provider
        .execute(request, "openai-chat")
        .await
        .expect("execute");
}

#[tokio::test]
async fn d62_2_not_forwarded_to_another_provider() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "ok"}}]
        })))
        .mount(&server)
        .await;

    let endpoint = Endpoint::new("beta".to_owned(), format!("{}/v1", server.uri()));
    let provider = OpenAiCompat::new(OpenAiProtocol::Chat, endpoint).expect("provider");
    let mut request = user_request();
    request.extensions.insert(
        pass_headers::CLIENT_MAP,
        serde_json::json!({"x-route": "from-client"}),
    );
    provider
        .execute(request, "openai-chat")
        .await
        .expect("execute");
}

#[test]
fn d62_3_overrides_fixed_header() {
    let mut headers = HeaderMap::new();
    headers.insert(
        reqwest::header::HeaderName::from_static("x-custom"),
        reqwest::header::HeaderValue::from_static("fixed"),
    );
    let mut client = BTreeMap::new();
    client.insert("x-custom".to_owned(), "agent".to_owned());
    pass_headers::apply(&["x-custom".to_owned()], &client, &mut headers);
    assert_eq!(
        headers
            .get("x-custom")
            .and_then(|value| value.to_str().ok()),
        Some("agent")
    );
}

#[test]
fn d62_5_listed_header_not_sent_by_client_adds_nothing() {
    let mut headers = HeaderMap::new();
    headers.insert(
        reqwest::header::HeaderName::from_static("x-fixed"),
        reqwest::header::HeaderValue::from_static("stay"),
    );
    let client = BTreeMap::new();
    pass_headers::apply(&["x-missing".to_owned()], &client, &mut headers);
    assert_eq!(
        headers.get("x-fixed").and_then(|value| value.to_str().ok()),
        Some("stay")
    );
    assert!(!headers.contains_key("x-missing"));
}

#[test]
fn d62_4_forbidden_names_rejected_at_load() {
    assert!(hofund_config::pass_headers::validated("p", vec!["Authorization".to_owned()]).is_err());
}
