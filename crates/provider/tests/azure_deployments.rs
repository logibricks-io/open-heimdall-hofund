//! Azure deployment listing against a local mock. No real network.

use hofund_provider::{ProviderErrorKind, list_deployments};
use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Match, Mock, MockServer, Request, ResponseTemplate};

struct NoQueryName(&'static str);

impl Match for NoQueryName {
    fn matches(&self, request: &Request) -> bool {
        !request.url.query_pairs().any(|(name, _)| name == self.0)
    }
}

#[tokio::test]
async fn deployments_are_model_ids_and_the_key_is_api_key() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/openai/deployments"))
        .and(query_param("api-version", "2022-12-01"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [
                {"id": "team-gpt", "model": "gpt-5", "object": "deployment", "status": "succeeded"},
                {"id": "text-embedding-3-small", "model": "text-embedding-3-small", "object": "deployment", "status": "succeeded"},
                {"id": "gpt-5-codex", "model": "gpt-5-codex", "object": "deployment", "status": "succeeded"}
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let listed = list_deployments(&format!("{}/openai/v1", server.uri()), "k", &[])
        .await
        .expect("list");
    assert_eq!(
        listed
            .iter()
            .map(|row| (row.id.as_str(), row.model.as_str()))
            .collect::<Vec<_>>(),
        vec![("team-gpt", "gpt-5"), ("gpt-5-codex", "gpt-5-codex")]
    );

    let requests = server.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url.path(), "/openai/deployments");
    assert_eq!(
        requests[0].url.query().expect("query"),
        "api-version=2022-12-01"
    );
    assert_eq!(
        requests[0]
            .headers
            .get("api-key")
            .expect("api-key")
            .to_str()
            .expect("text"),
        "k"
    );
    assert!(requests[0].headers.get("authorization").is_none());
    assert!(requests[0].headers.get("x-api-key").is_none());
}

#[tokio::test]
async fn a_refused_deployment_list_falls_back_to_v1_models() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/openai/deployments"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "error": {"code": "404", "message": "Resource not found"}
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/openai/v1/models"))
        .and(NoQueryName("after_id"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "has_more": true,
            "last_id": "team-gpt",
            "data": [{"id": "team-gpt", "object": "model"}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/openai/v1/models"))
        .and(query_param("after_id", "team-gpt"))
        .and(query_param("limit", "1000"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [{"id": "gpt-4.1", "object": "model"}]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let listed = list_deployments(
        &format!("{}/openai/v1", server.uri()),
        "k",
        &[("X-Request-Origin", "hofund")],
    )
    .await
    .expect("list");
    assert_eq!(
        listed.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
        vec!["team-gpt", "gpt-4.1"]
    );
    assert!(listed.iter().all(|row| row.model.is_empty()));

    let requests = server.received_requests().await.expect("requests");
    assert_eq!(requests[0].url.path(), "/openai/deployments");
    assert_eq!(requests[1].url.path(), "/openai/v1/models");
    assert!(requests[1].url.query().is_none());
    assert_eq!(requests[2].url.path(), "/openai/v1/models");
    assert_eq!(
        requests[2]
            .headers
            .get("x-request-origin")
            .expect("origin")
            .to_str()
            .expect("text"),
        "hofund"
    );
}

#[tokio::test]
async fn both_lists_refusing_the_key_names_the_deployments() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let error = list_deployments(&format!("{}/openai/v1", server.uri()), "wrong", &[])
        .await
        .expect_err("refused");
    assert_eq!(error.kind, ProviderErrorKind::Status);
    assert_eq!(error.status, Some(401));
    assert!(
        error.message.contains("deployments' names"),
        "{}",
        error.message
    );
    assert!(error.message.contains("401"), "{}", error.message);
    let requests = server.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|request| request.headers.get("authorization").is_none())
    );
}
