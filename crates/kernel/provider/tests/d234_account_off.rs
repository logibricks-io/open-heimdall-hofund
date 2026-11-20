//! Official Messages accounts the vendor turned off are marked restricted.
//! A relay that returns the same words stays an ordinary failure.

use hofund_ir::{ContentPart, Message, Request, Role};
use hofund_provider::{Anthropic, Endpoint, Provider};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn resolved(server: &MockServer, host: &str) -> Endpoint {
    let port = server.address().port();
    let mut endpoint = Endpoint::new("test", format!("http://{host}:{port}"));
    endpoint.api_key = "test-key".to_owned();
    endpoint.client = Some(
        reqwest::Client::builder()
            .resolve(host, *server.address())
            .build()
            .expect("client"),
    );
    endpoint
}

fn request() -> Request {
    let mut request = Request::new("demo");
    let mut message = Message::new(Role::User);
    message.content.push(ContentPart::text("hi"));
    request.messages.push(message);
    request.params.max_tokens = Some(16);
    request
}

async fn refuse(server: &MockServer, status: u16, message: &str) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(status).set_body_json(json!({
            "type": "error",
            "error": {"type": "invalid_request_error", "message": message}
        })))
        .mount(server)
        .await;
}

#[tokio::test]
async fn d234_official_host_execute_marks_organization_disabled() {
    let server = MockServer::start().await;
    refuse(&server, 400, "This organization has been disabled.").await;
    let provider = Anthropic::new(resolved(&server, "api.anthropic.com")).expect("provider");
    let Err(error) = provider.execute(request(), "anthropic-messages").await else {
        panic!("restricted");
    };
    assert!(error.is_account_restricted());
    assert_eq!(error.status, Some(400));
    assert!(error.cools());
}

#[tokio::test]
async fn d234_relay_host_execute_keeps_the_same_body_ordinary() {
    let server = MockServer::start().await;
    refuse(&server, 400, "This organization has been disabled.").await;
    let provider = Anthropic::new(resolved(&server, "relay.example.com")).expect("provider");
    let Err(error) = provider.execute(request(), "anthropic-messages").await else {
        panic!("ordinary");
    };
    assert!(!error.is_account_restricted());
    assert_eq!(error.status, Some(400));
}

#[tokio::test]
async fn d234_official_host_leaves_extra_usage_usage_policy_and_5xx_unmarked() {
    for (status, message) in [
        (
            400_u16,
            "Third-party apps now draw from your extra usage, not your plan limits.",
        ),
        (
            400,
            "Claude Code is unable to respond to this request, which appears to violate our Usage Policy",
        ),
        (500, "This organization has been disabled."),
        (403, "Your account can't receive replies from Claude."),
    ] {
        let server = MockServer::start().await;
        refuse(&server, status, message).await;
        let provider = Anthropic::new(resolved(&server, "api.anthropic.com")).expect("provider");
        let Err(error) = provider.execute(request(), "anthropic-messages").await else {
            panic!("{message}");
        };
        let restricted = message.contains("receive replies");
        assert_eq!(
            error.is_account_restricted(),
            restricted,
            "{status} {message}"
        );
        assert_eq!(error.status, Some(status), "{message}");
    }
}
