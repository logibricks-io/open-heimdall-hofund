//! Per-wire model listing and probes against local mock upstreams.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use hofund_plugin_api::{AuthChoice, ListKind, ListQuery, ProbeQuery, ProtocolId};
use hofund_provider::{
    Anthropic, Bedrock, Credential, Endpoint, Generative, OpenAiCompat, OpenAiProtocol, Provider,
    Vertex, azure_host,
};
use serde_json::{Value, json};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Match, Mock, MockServer, Request, Respond, ResponseTemplate};

fn endpoint(server: &MockServer, suffix: &str) -> Endpoint {
    let mut endpoint = Endpoint::new("test", format!("{}{suffix}", server.uri()));
    endpoint.api_key = "test-key".to_owned();
    endpoint
}

fn resolved_endpoint(server: &MockServer, host: &str, suffix: &str) -> Endpoint {
    let port = server.address().port();
    let mut endpoint = Endpoint::new("test", format!("http://{host}:{port}{suffix}"));
    endpoint.api_key = "test-key".to_owned();
    endpoint.client = Some(
        reqwest::Client::builder()
            .resolve(host, *server.address())
            .build()
            .expect("client"),
    );
    endpoint
}

fn openai(endpoint: Endpoint) -> OpenAiCompat {
    OpenAiCompat::new(OpenAiProtocol::Chat, endpoint).expect("openai")
}

fn vertex_endpoint(server: &MockServer) -> Endpoint {
    let mut endpoint = endpoint(server, "/v1/projects/p/locations/us");
    endpoint.api_key.clear();
    endpoint
}

fn bedrock_endpoint(server: &MockServer) -> Endpoint {
    let mut endpoint = endpoint(server, "");
    endpoint.api_key.clear();
    endpoint
}

fn probe(protocol: ProtocolId, model: &str) -> ProbeQuery {
    ProbeQuery::new(protocol, model).expect("probe")
}

fn ids(list: &hofund_plugin_api::ModelList) -> Vec<&str> {
    list.models().iter().map(|model| model.id()).collect()
}

#[tokio::test]
async fn versioned_base_lists_only_models_and_classifies_rows_with_metadata() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v4/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                {"id": "gpt-5.5", "display_name": "GPT", "context_length": 1000,
                 "max_output_tokens": 128, "modalities": {"input": ["text", "image"], "output": ["text"]},
                 "native_endpoints": ["/v1/chat/completions"], "supported_reasoning_levels": [{"effort": "high"}],
                 "web_search": "native", "service_tiers": [{"id": "priority"}]},
                {"id": "text-embedding-3-small"}, {"id": "gpt-4o-mini-tts"}, {"id": "tts-1"},
                {"id": "gpt-image-2"}, {"id": "dall-e-3"}, {"id": "flux-kontext-pro"},
                {"id": "sora-2", "kind": "video"}, {"id": "route", "kind": "decision"},
                {"id": "prefixed", "name": "Vendor: model", "supported_endpoints": ["/v1/responses"]}
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let listed = openai(endpoint(&server, "/v4"))
        .list_models(&ListQuery::new())
        .await
        .expect("list");
    assert_eq!(listed.kind(), ListKind::Live);
    assert_eq!(
        ids(&listed),
        vec![
            "gpt-5.5",
            "tts-1",
            "gpt-image-2",
            "dall-e-3",
            "flux-kontext-pro",
            "sora-2",
            "route",
            "prefixed"
        ]
    );
    let first = &listed.models()[0];
    assert_eq!(first.display_name(), Some("GPT"));
    assert_eq!(first.context(), 1000);
    assert_eq!(first.max_output(), 128);
    assert_eq!(first.image_input(), Some(true));
    assert_eq!(first.endpoints(), ["/v1/chat/completions"]);
    assert_eq!(first.efforts(), ["high"]);
    assert_eq!(first.web_search(), Some("native"));
    assert_eq!(first.service_tiers(), ["priority"]);
    assert!(listed.models()[2].draws());
    assert!(listed.models()[5].films());
    assert!(listed.models()[6].decides());
    assert_eq!(listed.models()[7].display_name(), None);
    let requests = server.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0]
            .headers
            .get("authorization")
            .expect("bearer")
            .to_str()
            .expect("text"),
        "Bearer test-key"
    );
    assert_eq!(
        requests[0]
            .headers
            .get("x-api-key")
            .expect("x-key")
            .to_str()
            .expect("text"),
        "test-key"
    );
}

#[tokio::test]
async fn unversioned_base_uses_direct_v1_fallback_and_sets_the_flag() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/relay/models"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/relay/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "chat"}]})))
        .expect(1)
        .mount(&server)
        .await;
    let listed = openai(endpoint(&server, "/relay"))
        .list_models(&ListQuery::new())
        .await
        .expect("list");
    assert!(listed.v1_fallback());
    assert_eq!(ids(&listed), ["chat"]);
    assert_eq!(server.received_requests().await.expect("requests").len(), 2);
}

#[tokio::test]
async fn anthropic_api_v1_base_strips_both_suffixes_before_root_models() {
    let server = MockServer::start().await;
    for candidate in ["/api/v1/models", "/api/v1/v1/models", "/v1/models"] {
        Mock::given(method("GET"))
            .and(path(candidate))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;
    }
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "claude"}]})))
        .expect(1)
        .mount(&server)
        .await;
    let listed = Anthropic::new(endpoint(&server, "/api/v1"))
        .expect("anthropic")
        .list_models(&ListQuery::new())
        .await
        .expect("list");
    assert_eq!(ids(&listed), ["claude"]);
    assert!(!listed.v1_fallback());
    let requests = server.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 4);
    assert!(
        requests
            .iter()
            .all(|request| request.headers.get("anthropic-version").is_some())
    );
}

struct BothAuthRefusal;

impl Respond for BothAuthRefusal {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let bearer = request.headers.get("authorization").is_some();
        let x_key = request.headers.get("x-api-key").is_some();
        match (bearer, x_key) {
            (true, true) => ResponseTemplate::new(401).set_body_json(json!({
                "error": {"message": "x-api-key and Authorization cannot both be present"}
            })),
            (false, true) => ResponseTemplate::new(401).set_body_string("x key alone refused"),
            (true, false) => {
                ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "claude"}]}))
            }
            (false, false) => ResponseTemplate::new(400),
        }
    }
}

// reference: internal/catalog/authheader.go:20-41
#[tokio::test]
async fn anthropic_both_auth_refusal_tries_each_single_header_and_learns_bearer() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(BothAuthRefusal)
        .expect(3)
        .mount(&server)
        .await;
    let listed = Anthropic::new(endpoint(&server, "/v1"))
        .expect("anthropic")
        .list_models(&ListQuery::new())
        .await
        .expect("learned");
    assert_eq!(listed.auth_choice(), Some(AuthChoice::Bearer));
    let requests = server.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 3);
    assert!(requests[2].headers.get("x-api-key").is_none());
}

#[tokio::test]
async fn openai_auth_refusal_is_not_retried_and_an_echoed_key_is_redacted() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": {"message": "bad key test-key; x-api-key and Authorization cannot both be present"}
        })))
        .expect(1)
        .mount(&server)
        .await;
    let error = openai(endpoint(&server, "/v1"))
        .list_models(&ListQuery::new())
        .await
        .expect_err("refused");
    assert_eq!(error.status(), Some(401));
    assert!(!error.to_string().contains("test-key"));
    assert!(!format!("{error:?}").contains("test-key"));
    assert_eq!(server.received_requests().await.expect("requests").len(), 1);
}

#[tokio::test]
async fn official_anthropic_auth_refusal_is_not_retried() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string("x-api-key and Authorization cannot both be present"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let provider =
        Anthropic::new(resolved_endpoint(&server, "api.anthropic.com", "/v1")).expect("anthropic");
    provider
        .list_models(&ListQuery::new())
        .await
        .expect_err("refused");
    let requests = server.received_requests().await.expect("requests");
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path() == "/v1/models")
            .count(),
        1
    );
    assert!(requests.iter().all(|request| {
        request.headers.get("authorization").is_some() && request.headers.get("x-api-key").is_some()
    }));
}

#[tokio::test]
async fn azure_anthropic_uses_x_api_key_without_learning() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/openai/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "claude"}]})))
        .expect(1)
        .mount(&server)
        .await;
    let mut endpoint = endpoint(&server, "/openai/v1");
    endpoint.credential = Credential::ApiKey;
    let listed = Anthropic::new(endpoint)
        .expect("anthropic")
        .list_models(&ListQuery::new())
        .await
        .expect("list");
    assert_eq!(listed.auth_choice(), None);
    let requests = server.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 1);
    assert!(requests[0].headers.get("authorization").is_none());
    assert!(requests[0].headers.get("api-key").is_none());
    assert!(requests[0].headers.get("x-api-key").is_some());
}

#[tokio::test]
async fn gemini_pages_filters_methods_and_tags_and_uses_only_google_key() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1beta/models"))
        .and(NoPageToken)
        .and(query_param("pageSize", "1000"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "models": [
                {"name": "models/gemini-chat", "displayName": "Gemini", "inputTokenLimit": 100,
                 "outputTokenLimit": 20, "supportedGenerationMethods": ["generateContent"]},
                {"name": "models/embed", "supportedGenerationMethods": ["embedContent"]},
                {"name": "models/gemini-image", "supportedGenerationMethods": []}
            ],
            "nextPageToken": "two"
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1beta/models"))
        .and(query_param("pageToken", "two"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "models": [{"name": "models/gemini-audio"}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let listed = Generative::new(endpoint(&server, "/v1beta"))
        .expect("generative")
        .list_models(&ListQuery::new())
        .await
        .expect("list");
    assert_eq!(ids(&listed), ["gemini-chat", "gemini-image"]);
    assert_eq!(listed.models()[0].display_name(), Some("Gemini"));
    assert_eq!(listed.models()[0].context(), 100);
    assert_eq!(listed.models()[0].max_output(), 20);
    assert!(listed.models()[1].draws());
    let requests = server.received_requests().await.expect("requests");
    assert!(
        requests
            .iter()
            .all(|request| request.headers.get("x-goog-api-key").is_some())
    );
    assert!(
        requests
            .iter()
            .all(|request| request.headers.get("authorization").is_none())
    );
    assert!(
        requests
            .iter()
            .all(|request| request.headers.get("x-api-key").is_none())
    );
}

struct EndlessPages {
    calls: Arc<AtomicUsize>,
    gemini: bool,
}

impl Respond for EndlessPages {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        let page = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if self.gemini {
            ResponseTemplate::new(200).set_body_json(json!({
                "models": [{"name": format!("models/model-{page}")}],
                "nextPageToken": format!("next-{page}")
            }))
        } else {
            ResponseTemplate::new(200).set_body_json(json!({
                "data": [{"id": format!("model-{page}")}],
                "has_more": true,
                "last_id": format!("model-{page}")
            }))
        }
    }
}

#[tokio::test]
async fn after_id_twentieth_page_fails_without_a_twenty_first_request() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(EndlessPages {
            calls: Arc::clone(&calls),
            gemini: false,
        })
        .expect(20)
        .mount(&server)
        .await;
    let error = openai(endpoint(&server, "/v1"))
        .list_models(&ListQuery::new())
        .await
        .expect_err("partial list");
    assert!(error.to_string().contains("20 pages"));
    assert_eq!(calls.load(Ordering::SeqCst), 20);
}

#[tokio::test]
async fn gemini_twentieth_page_returns_the_partial_list() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("GET"))
        .and(path("/v1beta/models"))
        .respond_with(EndlessPages {
            calls: Arc::clone(&calls),
            gemini: true,
        })
        .expect(20)
        .mount(&server)
        .await;
    let listed = Generative::new(endpoint(&server, "/v1beta"))
        .expect("generative")
        .list_models(&ListQuery::new())
        .await
        .expect("partial list");
    assert_eq!(listed.models().len(), 20);
    assert_eq!(calls.load(Ordering::SeqCst), 20);
}

#[tokio::test]
async fn azure_plugin_returns_deployments_then_marks_the_base_model_fallback() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/openai/deployments"))
        .and(query_param("api-version", "2022-12-01"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/openai/v1/models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "base-gpt"}]})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let mut endpoint = endpoint(&server, "/openai/v1");
    endpoint.credential = Credential::ApiKey;
    let listed = openai(endpoint)
        .list_models(&ListQuery::new())
        .await
        .expect("fallback");
    assert_eq!(listed.kind(), ListKind::Deployments);
    assert!(listed.base_model_fallback());
    assert_eq!(ids(&listed), ["base-gpt"]);
    let requests = server.received_requests().await.expect("requests");
    assert!(
        requests
            .iter()
            .all(|request| request.headers.get("api-key").is_some())
    );
    assert!(
        requests
            .iter()
            .all(|request| request.headers.get("authorization").is_none())
    );
    assert!(
        requests
            .iter()
            .all(|request| request.headers.get("x-api-key").is_none())
    );
}

#[tokio::test]
async fn azure_both_failures_end_with_deployment_name_guidance() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .expect(2)
        .mount(&server)
        .await;
    let mut endpoint = endpoint(&server, "/openai/v1");
    endpoint.credential = Credential::ApiKey;
    let error = openai(endpoint)
        .list_models(&ListQuery::new())
        .await
        .expect_err("failure");
    assert!(
        error
            .to_string()
            .ends_with("type your deployments' names in as model ids")
    );
}

#[tokio::test]
async fn vertex_and_bedrock_static_catalogs_make_no_network_requests() {
    let server = MockServer::start().await;
    let vertex = Vertex::new(vertex_endpoint(&server), "", "").expect("vertex");
    let listed = vertex
        .list_models(&ListQuery::new())
        .await
        .expect("catalog");
    assert_eq!(listed.kind(), ListKind::Catalog);
    assert!(ids(&listed).contains(&"gemini-3.5-flash"));
    assert!(!ids(&listed).contains(&"gemini-2.5-pro"));

    let bedrock = Bedrock::new(
        bedrock_endpoint(&server),
        "us-east-1",
        "access",
        "secret",
        "",
    )
    .expect("bedrock");
    let listed = bedrock
        .list_models(&ListQuery::new())
        .await
        .expect("catalog");
    assert_eq!(listed.kind(), ListKind::Catalog);
    assert!(ids(&listed).contains(&"openai.gpt-oss-120b-1:0"));
    assert!(
        server
            .received_requests()
            .await
            .expect("requests")
            .is_empty()
    );
    assert!(azure_host("resource.openai.azure.com:443"));
    assert!(!azure_host("openai.azure.com"));
}

#[tokio::test]
async fn chat_and_responses_probes_send_minimal_bodies_and_ignore_learned_auth() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .expect(1)
        .mount(&server)
        .await;
    let query = probe(ProtocolId::OPENAI_CHAT, "gpt").with_auth_choice(AuthChoice::XApiKey);
    openai(endpoint(&server, "/v1"))
        .probe(&query)
        .await
        .expect("chat");
    let responses =
        OpenAiCompat::new(OpenAiProtocol::Responses, endpoint(&server, "/v1")).expect("responses");
    responses
        .probe(&probe(ProtocolId::OPENAI_RESPONSES, "gpt"))
        .await
        .expect("responses");
    let requests = server.received_requests().await.expect("requests");
    let chat = requests[0].body_json::<Value>().expect("chat body");
    assert_eq!(chat["max_tokens"], 16);
    assert_eq!(chat["messages"][0]["content"], "hi");
    let responses = requests[1].body_json::<Value>().expect("responses body");
    assert_eq!(responses["max_output_tokens"], 16);
    assert_eq!(responses["input"][0]["content"][0]["text"], "hi");
    assert!(
        requests
            .iter()
            .all(|request| request.headers.get("x-api-key").is_none())
    );
    assert!(
        requests
            .iter()
            .all(|request| request.headers.get("authorization").is_some())
    );
}

#[tokio::test]
async fn azure_and_bedrock_chat_probes_use_max_completion_tokens() {
    for (host, azure) in [
        ("resource.openai.azure.com", true),
        ("bedrock-runtime.us-east-1.amazonaws.com", false),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(1)
            .mount(&server)
            .await;
        let provider = openai(resolved_endpoint(&server, host, "/v1"));
        provider
            .probe(&probe(ProtocolId::OPENAI_CHAT, "gpt"))
            .await
            .expect("probe");
        let requests = server.received_requests().await.expect("requests");
        let body = requests[0].body_json::<Value>().expect("body");
        assert_eq!(body["max_completion_tokens"], 16);
        assert!(body.get("max_tokens").is_none());
        assert_eq!(requests[0].headers.get("api-key").is_some(), azure);
    }
}

#[tokio::test]
async fn mantle_probe_retries_the_other_model_prefix() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": {"message": "model is not supported on this route"}
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/openai/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&server)
        .await;
    let provider = openai(resolved_endpoint(
        &server,
        "bedrock-mantle.us-east-1.api.aws",
        "/v1",
    ));
    provider
        .probe(&probe(ProtocolId::OPENAI_CHAT, "newmaker.model"))
        .await
        .expect("retry");
    assert_eq!(server.received_requests().await.expect("requests").len(), 2);
}

#[tokio::test]
async fn anthropic_probe_merges_extra_body_and_learns_one_auth_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/v1/messages"))
        .respond_with(BothAuthRefusal)
        .expect(3)
        .mount(&server)
        .await;
    let query = probe(ProtocolId::ANTHROPIC_MESSAGES, "claude").with_extra_body(
        bytes::Bytes::from_static(br#"{"stream":false,"temperature":0}"#),
    );
    let report = Anthropic::new(endpoint(&server, "/v1"))
        .expect("anthropic")
        .probe(&query)
        .await
        .expect("probe");
    assert_eq!(report.auth_choice(), Some(AuthChoice::Bearer));
    let requests = server.received_requests().await.expect("requests");
    let body = requests[2].body_json::<Value>().expect("body");
    assert_eq!(body["max_tokens"], 16);
    assert_eq!(body["temperature"], 0);
    assert!(requests[2].headers.get("x-api-key").is_none());
    assert!(
        requests
            .iter()
            .all(|request| request.headers.get("anthropic-version").is_some())
    );
}

#[tokio::test]
async fn generative_probe_uses_native_url_body_and_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini:generateContent"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&server)
        .await;
    Generative::new(endpoint(&server, "/v1beta"))
        .expect("generative")
        .probe(&probe(ProtocolId::GENERATIVE_LANGUAGE, "gemini"))
        .await
        .expect("probe");
    let requests = server.received_requests().await.expect("requests");
    let body = requests[0].body_json::<Value>().expect("body");
    assert_eq!(body["generationConfig"]["maxOutputTokens"], 16);
    assert_eq!(body["contents"][0]["parts"][0]["text"], "hi");
    assert!(requests[0].headers.get("x-goog-api-key").is_some());
    assert!(requests[0].headers.get("authorization").is_none());
}

fn vertex_credentials() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("hofund-model-probe-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    let path = dir.join("adc.json");
    std::fs::write(&path, serde_json::to_vec(&json!({
        "type": "authorized_user", "client_id": "client", "client_secret": "secret", "refresh_token": "refresh"
    })).expect("json")).expect("credentials");
    path
}

#[tokio::test]
async fn vertex_probe_mints_a_token_and_applies_thinking_rules() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"access_token": "token", "expires_in": 3600})),
        )
        .expect(1)
        .mount(&server)
        .await;
    for model in [
        "gemini-3-flash",
        "gemini-2.5-pro",
        "gemini-2.5-flash",
        "gemini-flash-latest",
    ] {
        Mock::given(method("POST"))
            .and(path(format!(
                "/v1/projects/p/locations/us/publishers/google/models/{model}:generateContent"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(1)
            .mount(&server)
            .await;
    }
    let credentials = vertex_credentials();
    let provider = Vertex::with_token_endpoints(
        vertex_endpoint(&server),
        credentials.display().to_string(),
        "",
        format!("{}/token", server.uri()),
        format!("{}/iam", server.uri()),
    )
    .expect("vertex");
    for model in [
        "gemini-3-flash",
        "gemini-2.5-pro",
        "gemini-2.5-flash",
        "gemini-flash-latest",
    ] {
        provider
            .probe(&probe(ProtocolId::GENERATIVE_LANGUAGE, model))
            .await
            .expect("probe");
    }
    let requests = server.received_requests().await.expect("requests");
    let bodies = requests
        .iter()
        .skip(1)
        .map(|request| request.body_json::<Value>().expect("body"))
        .collect::<Vec<_>>();
    assert_eq!(
        bodies[0]["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "low"
    );
    assert_eq!(
        bodies[1]["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        128
    );
    assert_eq!(
        bodies[2]["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        0
    );
    assert!(
        bodies[3]["generationConfig"]
            .get("thinkingConfig")
            .is_none()
    );
    assert!(
        requests
            .iter()
            .skip(1)
            .all(|request| request.headers.get("x-goog-api-key").is_none())
    );
    assert!(
        requests
            .iter()
            .skip(1)
            .all(|request| request.headers.get("authorization").is_some())
    );
    let _ = std::fs::remove_dir_all(credentials.parent().expect("parent"));
}

#[tokio::test]
async fn bedrock_probe_signs_one_minimal_converse_call() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/model/anthropic.claude/converse"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&server)
        .await;
    Bedrock::new(
        bedrock_endpoint(&server),
        "us-east-1",
        "access",
        "secret",
        "",
    )
    .expect("bedrock")
    .probe(&probe(ProtocolId::BEDROCK_CONVERSE, "anthropic.claude"))
    .await
    .expect("probe");
    let requests = server.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].body_json::<Value>().expect("body")["inferenceConfig"]["maxTokens"],
        16
    );
    assert!(
        requests[0]
            .headers
            .get("authorization")
            .expect("signed")
            .to_str()
            .expect("text")
            .starts_with("AWS4-HMAC-SHA256")
    );
}

#[tokio::test]
async fn probe_status_hints_keep_kind_and_status() {
    let cases = [
        (
            json!("<html>request has been blocked</html>"),
            "network edge",
        ),
        (
            json!({"error": {"message": "denied", "details": [{"reason": "VALIDATION_REQUIRED", "metadata": {"validation_url": "https://accounts.example/verify"}}]}}),
            "https://accounts.example/verify",
        ),
        (
            json!({"error": {"message": "Your Go plan doesn't include API access"}}),
            "API-enabled plan",
        ),
    ];
    for (body, hint) in cases {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(403).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;
        let error = openai(endpoint(&server, "/v1"))
            .probe(&probe(ProtocolId::OPENAI_CHAT, "gpt"))
            .await
            .expect_err("status");
        assert_eq!(error.kind(), hofund_plugin_api::PluginErrorKind::Status);
        assert_eq!(error.status(), Some(403));
        assert!(error.to_string().contains(hint), "{} missing {hint}", error);
    }
}

#[tokio::test]
async fn probe_429_is_not_retried_and_preserves_retry_after() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "7"))
        .expect(1)
        .mount(&server)
        .await;
    let error = openai(endpoint(&server, "/v1"))
        .probe(&probe(ProtocolId::OPENAI_CHAT, "gpt"))
        .await
        .expect_err("429");
    assert_eq!(error.status(), Some(429));
    assert_eq!(error.retry_after(), Some(std::time::Duration::from_secs(7)));
    assert_eq!(server.received_requests().await.expect("requests").len(), 1);
}

#[tokio::test]
async fn streamed_probe_stops_at_one_mib_and_keeps_a_capped_snippet() {
    let server = MockServer::start().await;
    let body = vec![b' '; 1024 * 1024 + 128 * 1024];
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .expect(1)
        .mount(&server)
        .await;
    let query = probe(ProtocolId::OPENAI_CHAT, "gpt")
        .with_extra_body(bytes::Bytes::from_static(br#"{"stream":true}"#));
    let report = openai(endpoint(&server, "/v1"))
        .probe(&query)
        .await
        .expect("answered");
    assert!(report.snippet().len() <= hofund_plugin_api::PROBE_CAP);
}

#[tokio::test]
async fn list_body_is_rejected_beyond_eight_mib() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b' '; 8 * 1024 * 1024 + 1]))
        .expect(1)
        .mount(&server)
        .await;
    let error = openai(endpoint(&server, "/v1"))
        .list_models(&ListQuery::new())
        .await
        .expect_err("too large");
    assert!(error.to_string().contains("too large"));
    assert_eq!(server.received_requests().await.expect("requests").len(), 1);
}

struct NoQuery;

impl Match for NoQuery {
    fn matches(&self, request: &Request) -> bool {
        request.url.query().is_none()
    }
}

struct NoPageToken;

impl Match for NoPageToken {
    fn matches(&self, request: &Request) -> bool {
        !request
            .url
            .query_pairs()
            .any(|(name, _)| name == "pageToken")
    }
}

#[tokio::test]
async fn mantle_list_uses_v1_models_only() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(NoQuery)
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "chat"}]})))
        .expect(1)
        .mount(&server)
        .await;
    let listed = openai(resolved_endpoint(
        &server,
        "bedrock-mantle.us-east-1.api.aws",
        "/v1",
    ))
    .list_models(&ListQuery::new())
    .await
    .expect("list");
    assert_eq!(ids(&listed), ["chat"]);
    let requests = server.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 1);
    assert!(
        requests
            .iter()
            .all(|request| request.url.path() != "/openai/v1/models")
    );
}

#[tokio::test]
async fn user_headers_override_list_defaults_case_insensitively() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "claude"}]})))
        .expect(1)
        .mount(&server)
        .await;
    let mut endpoint = endpoint(&server, "/v1");
    endpoint.headers = vec![
        ("Authorization".to_owned(), "custom-bearer".to_owned()),
        ("X-Api-Key".to_owned(), "custom-x-key".to_owned()),
        ("Anthropic-Version".to_owned(), "custom-version".to_owned()),
    ];
    Anthropic::new(endpoint)
        .expect("anthropic")
        .list_models(&ListQuery::new())
        .await
        .expect("list");
    let requests = server.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0]
            .headers
            .get("authorization")
            .expect("auth")
            .to_str()
            .expect("text"),
        "custom-bearer"
    );
    assert_eq!(
        requests[0]
            .headers
            .get("x-api-key")
            .expect("key")
            .to_str()
            .expect("text"),
        "custom-x-key"
    );
    assert_eq!(
        requests[0]
            .headers
            .get("anthropic-version")
            .expect("version")
            .to_str()
            .expect("text"),
        "custom-version"
    );
}
