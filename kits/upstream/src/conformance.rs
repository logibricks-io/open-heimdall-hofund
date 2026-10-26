use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use hofund_plugin_api::{
    DynUpstreamFactory, ListKind, ListQuery, PluginErrorKind, ProbeQuery, ProtocolId, RawTable,
    UpstreamProvider,
};
use hofund_plugin_host::{HttpLimits, runtime_host};
use serde_json::{Value, json};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Match, Mock, MockServer, Request, Respond, ResponseTemplate};

const MODEL: &str = "conformance-model";

/// Checks model listing and, for a live factory, one minimal probe.
///
/// `raw_for_base` returns the complete factory input for a temporary loopback
/// base URL. `protocol` selects the probe wire. `api_key` is installed in the
/// runtime secret store; callers should also place it in the raw table when
/// their factory reads credentials from that table. Every call uses local mock
/// servers and never a live vendor.
///
/// A live Generative Language wire uses `pageToken` paging and accepts a
/// partial result at 20 pages. Other live wires use `after_id` and reject that
/// partial result. A catalog must return rows without dialing. Unsupported and
/// unknown future list kinds must return [`PluginErrorKind::Unsupported`].
/// `Deployments` is an instance-level result on a live factory spec, so it is
/// covered by preset-aware host tests rather than this factory-only helper.
///
/// # Panics
///
/// Panics when the factory rejects a fixture, a mock expectation fails, or the
/// provider violates its advertised list or probe contract.
pub async fn check_list_and_probe(
    factory: &dyn DynUpstreamFactory,
    raw_for_base: impl Fn(&str) -> RawTable,
    protocol: ProtocolId,
    api_key: &str,
) {
    match factory.spec().list_kind() {
        ListKind::Live => {
            if protocol == ProtocolId::GENERATIVE_LANGUAGE {
                check_live(factory, &raw_for_base, protocol, api_key, true).await;
            } else {
                check_live(factory, &raw_for_base, protocol, api_key, false).await;
            }
        }
        ListKind::Catalog => check_catalog(factory, &raw_for_base, api_key).await,
        ListKind::Deployments | ListKind::Unsupported | _ => {
            check_unsupported(factory, &raw_for_base, api_key).await;
        }
    }
}

async fn check_live(
    factory: &dyn DynUpstreamFactory,
    raw_for_base: &impl Fn(&str) -> RawTable,
    protocol: ProtocolId,
    api_key: &str,
    gemini: bool,
) {
    multi_page(factory, raw_for_base, api_key, gemini).await;
    page_cap(factory, raw_for_base, api_key, gemini).await;
    if !gemini {
        repeated_cursor(factory, raw_for_base, api_key).await;
    }
    empty_list(factory, raw_for_base, api_key, gemini).await;
    minimal_probe(factory, raw_for_base, protocol, api_key).await;
}

async fn multi_page(
    factory: &dyn DynUpstreamFactory,
    raw_for_base: &impl Fn(&str) -> RawTable,
    api_key: &str,
    gemini: bool,
) {
    let server = MockServer::start().await;
    let list_path = list_path(gemini);
    if gemini {
        Mock::given(method("GET"))
            .and(path(list_path))
            .and(query_param("pageSize", "1000"))
            .and(NoPageToken)
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "models": [{"name": "models/model-1"}],
                "nextPageToken": "page-2"
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(list_path))
            .and(query_param("pageSize", "1000"))
            .and(query_param("pageToken", "page-2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "models": [{"name": "models/model-2"}]
            })))
            .expect(1)
            .mount(&server)
            .await;
    } else {
        Mock::given(method("GET"))
            .and(path(list_path))
            .and(NoAfterId)
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{"id": "model-1"}],
                "has_more": true,
                "last_id": "model-1"
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(list_path))
            .and(query_param("after_id", "model-1"))
            .and(query_param("limit", "1000"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{"id": "model-2"}]
            })))
            .expect(1)
            .mount(&server)
            .await;
    }
    let (provider, root) = build(factory, raw_for_base, &server, api_key, gemini);
    let listed = provider
        .list_models(&ListQuery::new())
        .await
        .expect("multi-page list");
    assert_eq!(listed.models().len(), 2);
    assert_eq!(listed.models()[0].id(), "model-1");
    assert_eq!(listed.models()[1].id(), "model-2");
    assert_eq!(request_count(&server).await, 2);
    finish(provider, root);
    server.verify().await;
}

async fn page_cap(
    factory: &dyn DynUpstreamFactory,
    raw_for_base: &impl Fn(&str) -> RawTable,
    api_key: &str,
    gemini: bool,
) {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("GET"))
        .and(path(list_path(gemini)))
        .respond_with(EndlessPages {
            calls: Arc::clone(&calls),
            gemini,
        })
        .expect(20)
        .mount(&server)
        .await;
    let (provider, root) = build(factory, raw_for_base, &server, api_key, gemini);
    let result = provider.list_models(&ListQuery::new()).await;
    if gemini {
        assert_eq!(result.expect("partial Gemini list").models().len(), 20);
    } else {
        let error = result.expect_err("partial after_id list must fail");
        assert!(error.to_string().contains("20 pages"), "{error}");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 20);
    let requests = server.received_requests().await.expect("requests");
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path() == list_path(gemini))
            .count(),
        20
    );
    finish(provider, root);
    server.verify().await;
}

async fn repeated_cursor(
    factory: &dyn DynUpstreamFactory,
    raw_for_base: &impl Fn(&str) -> RawTable,
    api_key: &str,
) {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "model-1"}],
            "has_more": true,
            "last_id": "same-cursor"
        })))
        .expect(2)
        .mount(&server)
        .await;
    let (provider, root) = build(factory, raw_for_base, &server, api_key, false);
    let error = provider
        .list_models(&ListQuery::new())
        .await
        .expect_err("repeated cursor must fail");
    assert!(error.to_string().contains("cursor repeated"), "{error}");
    let requests = server.received_requests().await.expect("requests");
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path() == "/v1/models")
            .count(),
        2
    );
    finish(provider, root);
    server.verify().await;
}

async fn empty_list(
    factory: &dyn DynUpstreamFactory,
    raw_for_base: &impl Fn(&str) -> RawTable,
    api_key: &str,
    gemini: bool,
) {
    let server = MockServer::start().await;
    let body = if gemini {
        json!({"models": []})
    } else {
        json!({"data": []})
    };
    Mock::given(method("GET"))
        .and(path(list_path(gemini)))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .expect(1)
        .mount(&server)
        .await;
    let (provider, root) = build(factory, raw_for_base, &server, api_key, gemini);
    let error = provider
        .list_models(&ListQuery::new())
        .await
        .expect_err("empty list must fail");
    assert!(error.to_string().contains("no models listed"), "{error}");
    finish(provider, root);
    server.verify().await;
}

async fn minimal_probe(
    factory: &dyn DynUpstreamFactory,
    raw_for_base: &impl Fn(&str) -> RawTable,
    protocol: ProtocolId,
    api_key: &str,
) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&server)
        .await;
    let gemini = protocol == ProtocolId::GENERATIVE_LANGUAGE;
    let (provider, root) = build(factory, raw_for_base, &server, api_key, gemini);
    let query = ProbeQuery::new(protocol, MODEL).expect("probe query");
    provider.probe(&query).await.expect("minimal probe");
    let requests = server.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 1);
    let body: Value = serde_json::from_slice(&requests[0].body).expect("probe JSON");
    assert!(has_16_token_limit(&body), "probe body has no 16-token cap");
    finish(provider, root);
    server.verify().await;
}

async fn check_catalog(
    factory: &dyn DynUpstreamFactory,
    raw_for_base: &impl Fn(&str) -> RawTable,
    api_key: &str,
) {
    let server = MockServer::start().await;
    let (provider, root) = build(factory, raw_for_base, &server, api_key, false);
    let listed = provider
        .list_models(&ListQuery::new())
        .await
        .expect("catalog list");
    assert!(!listed.models().is_empty());
    assert_eq!(request_count(&server).await, 0);
    finish(provider, root);
}

async fn check_unsupported(
    factory: &dyn DynUpstreamFactory,
    raw_for_base: &impl Fn(&str) -> RawTable,
    api_key: &str,
) {
    let server = MockServer::start().await;
    let (provider, root) = build(factory, raw_for_base, &server, api_key, false);
    let error = provider
        .list_models(&ListQuery::new())
        .await
        .expect_err("unsupported list must fail");
    assert_eq!(error.kind(), PluginErrorKind::Unsupported);
    assert_eq!(request_count(&server).await, 0);
    finish(provider, root);
}

fn build(
    factory: &dyn DynUpstreamFactory,
    raw_for_base: &impl Fn(&str) -> RawTable,
    server: &MockServer,
    api_key: &str,
    gemini: bool,
) -> (Arc<dyn UpstreamProvider>, PathBuf) {
    let base = if gemini {
        format!("{}/v1beta", server.uri())
    } else {
        format!("{}/v1", server.uri())
    };
    let root = scratch();
    let host = runtime_host(
        factory.spec().plugin().clone(),
        api_key,
        &root,
        HttpLimits::default().with_max_idle_per_host(0),
    )
    .expect("runtime host");
    let provider = factory
        .validate_and_build(&raw_for_base(&base), &host)
        .expect("factory build");
    (provider, root)
}

fn scratch() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "hofund-upstream-conformance-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&root).expect("conformance directory");
    root
}

fn finish(provider: Arc<dyn UpstreamProvider>, root: PathBuf) {
    drop(provider);
    let _ = std::fs::remove_dir_all(root);
}

fn list_path(gemini: bool) -> &'static str {
    if gemini {
        "/v1beta/models"
    } else {
        "/v1/models"
    }
}

async fn request_count(server: &MockServer) -> usize {
    server.received_requests().await.expect("requests").len()
}

fn has_16_token_limit(body: &Value) -> bool {
    [
        "/max_tokens",
        "/max_output_tokens",
        "/max_completion_tokens",
        "/generationConfig/maxOutputTokens",
        "/inferenceConfig/maxTokens",
    ]
    .iter()
    .any(|pointer| body.pointer(pointer).and_then(Value::as_u64) == Some(16))
}

struct NoAfterId;

impl Match for NoAfterId {
    fn matches(&self, request: &Request) -> bool {
        !request
            .url
            .query_pairs()
            .any(|(name, _)| name == "after_id")
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
                "nextPageToken": format!("page-{}", page + 1)
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
