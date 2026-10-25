//! Live model lists against a local mock. No real network.

use hofund_provider::{Credential, ListQuery, Wire, list_models};
use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Match, Mock, MockServer, Request, ResponseTemplate};

struct NoQueryName(&'static str);

impl Match for NoQueryName {
    fn matches(&self, request: &Request) -> bool {
        !request.url.query_pairs().any(|(name, _)| name == self.0)
    }
}

async fn listed(
    server: &MockServer,
    wire: Wire,
    credential: Credential,
    key: &str,
) -> Result<Vec<String>, hofund_provider::ProviderError> {
    let base = format!("{}/v1", server.uri());
    let headers = Vec::new();
    list_models(&ListQuery {
        base_url: &base,
        wire,
        credential,
        api_key: key,
        headers: &headers,
        preset_id: "",
    })
    .await
}

#[tokio::test]
async fn an_openai_list_follows_after_id() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(NoQueryName("after_id"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [{"id": "a", "object": "model"}],
            "has_more": true,
            "last_id": "a"
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(query_param("after_id", "a"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [{"id": "b", "object": "model"}],
            "has_more": false
        })))
        .expect(1)
        .mount(&server)
        .await;

    let ids = listed(&server, Wire::Chat, Credential::Bearer, "secret")
        .await
        .expect("list");
    assert_eq!(ids, ["a", "b"]);
    let requests = server.received_requests().await.expect("requests");
    assert_eq!(
        requests[0]
            .headers
            .get("authorization")
            .expect("authorization")
            .to_str()
            .expect("text"),
        "Bearer secret"
    );
}

#[tokio::test]
async fn a_gemini_list_keeps_generate_content_and_strips_the_prefix() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "models": [
                {
                    "name": "models/gemini-2.5-flash",
                    "supportedGenerationMethods": ["generateContent", "countTokens"]
                },
                {
                    "name": "models/embedding-001",
                    "supportedGenerationMethods": ["embedContent"]
                }
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let base = server.uri();
    let headers = Vec::new();
    let ids = list_models(&ListQuery {
        base_url: &base,
        wire: Wire::Generative,
        credential: Credential::Bearer,
        api_key: "gem-key",
        headers: &headers,
        preset_id: "google",
    })
    .await
    .expect("list");
    assert_eq!(ids, ["gemini-2.5-flash"]);
    let requests = server.received_requests().await.expect("requests");
    assert_eq!(
        requests[0]
            .headers
            .get("x-goog-api-key")
            .expect("key")
            .to_str()
            .expect("text"),
        "gem-key"
    );
    assert!(
        requests[0]
            .url
            .query()
            .unwrap_or("")
            .contains("pageSize=1000")
    );
}

#[tokio::test]
async fn a_messages_list_sends_the_version_header() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "claude-sonnet", "type": "model"}]
        })))
        .mount(&server)
        .await;

    let ids = listed(&server, Wire::Messages, Credential::XApiKey, "ant-key")
        .await
        .expect("list");
    assert_eq!(ids, ["claude-sonnet"]);
    let requests = server.received_requests().await.expect("requests");
    let hit = requests
        .iter()
        .find(|request| request.url.path() == "/v1/models")
        .expect("models");
    assert_eq!(
        hit.headers
            .get("x-api-key")
            .expect("x-api-key")
            .to_str()
            .expect("text"),
        "ant-key"
    );
    assert_eq!(
        hit.headers
            .get("anthropic-version")
            .expect("version")
            .to_str()
            .expect("text"),
        "2023-06-01"
    );
}

#[tokio::test]
async fn a_refused_list_tells_the_caller_to_type_ids() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": {"message": "bad key"}
        })))
        .mount(&server)
        .await;

    let error = listed(&server, Wire::Chat, Credential::Bearer, "nope")
        .await
        .expect_err("refused");
    assert!(
        error.to_string().contains("type its model ids in by hand"),
        "{error}"
    );
    assert!(!error.to_string().contains("nope"));
}
