//! Gateway-key limit enforcement over HTTP, without network access.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use hofund_codec::{Codec, OpenAiChat};
use hofund_gateway::Gateway;
use hofund_ir::{Message, Request as ModelRequest, Response, Role, Usage};
use hofund_plugin_api::{MemberId, MemberKey, PluginError, PresetId};
use hofund_router::{Completion, FailoverPolicy, Hop, Member, Router, Snapshot};
use hofund_secrets::{KeyLimit, KeyStore};
use hofund_usage::Ledger;
use tower::ServiceExt;

struct Fake {
    member_id: MemberId,
    hits: Arc<AtomicUsize>,
}

#[async_trait]
impl Member for Fake {
    fn id(&self) -> &MemberId {
        &self.member_id
    }

    fn route_id(&self) -> &str {
        "primary"
    }

    fn protocol(&self) -> &str {
        "plain"
    }

    async fn execute(
        &self,
        _request: ModelRequest,
        _inbound: &str,
    ) -> Result<Completion, PluginError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        let response = Response::new(Message::new(Role::Assistant)).with_usage(Some(Usage {
            input_tokens: Some(10),
            output_tokens: Some(5),
            cache_read_tokens: Some(4),
            cache_write_tokens: Some(2),
            reasoning_tokens: Some(3),
            ..Usage::default()
        }));
        Ok(Completion::Unary(Box::new(response)))
    }
}

struct Fixture {
    app: axum::Router,
    store: Arc<KeyStore>,
    usage: Arc<Ledger>,
    hits: Arc<AtomicUsize>,
    _dir: tempfile::TempDir,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("dir");
    let store = Arc::new(KeyStore::beside_token(dir.path().join("hofund.token")));
    store.list().expect("migration");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let hits = Arc::new(AtomicUsize::new(0));
    let member = Arc::new(Fake {
        member_id: MemberId::from_preset(
            PresetId::try_from("primary").expect("preset"),
            MemberKey::from_static("row"),
        ),
        hits: Arc::clone(&hits),
    });
    let routes = BTreeMap::from([(
        "demo".to_owned(),
        vec![Hop {
            member,
            wire_model: None,
            max_output_tokens: None,
            context_window: None,
            patience: None,
            weight: 1,
            free: false,
            cold_after: hofund_router::affinity::CACHE_COLD,
            nearly_spent: false,
            fallback: false,
            effort: String::new(),
        }],
    )]);
    let token = store.token_file().read().expect("token");
    let app = Gateway::new(
        Snapshot::new(Router::from_hops(routes, FailoverPolicy::default())),
        token,
        HashMap::from([(
            "/v1/chat/completions".to_owned(),
            Arc::new(OpenAiChat) as Arc<dyn Codec>,
        )]),
        Arc::clone(&usage),
    )
    .with_key_store(Arc::clone(&store))
    .into_router();
    Fixture {
        app,
        store,
        usage,
        hits,
        _dir: dir,
    }
}

fn limited_key(fixture: &Fixture, limit: KeyLimit) -> String {
    let added = fixture
        .store
        .add("Capped", Some("limited-secret-123"))
        .expect("add");
    fixture
        .store
        .set_limit(&added.key.id, Some(limit), None, 100)
        .expect("limit");
    added.secret
}

async fn call(app: axum::Router, secret: &str) -> (StatusCode, HeaderMap, String) {
    request(
        app,
        "POST",
        "/v1/chat/completions",
        secret,
        r#"{"model":"demo","messages":[{"role":"user","content":"hi"}]}"#,
    )
    .await
}

async fn request(
    app: axum::Router,
    method: &str,
    uri: &str,
    secret: &str,
    body: &str,
) -> (StatusCode, HeaderMap, String) {
    let response = app
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("authorization", format!("Bearer {secret}"))
                .body(Body::from(body.to_owned()))
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    (status, headers, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test]
async fn l5_http_refusal_has_reason_reset_and_retry_headers() {
    let fixture = fixture();
    let secret = limited_key(
        &fixture,
        KeyLimit {
            period: "day".to_owned(),
            tokens: 13,
            ..KeyLimit::default()
        },
    );
    let (status, _, body) = call(fixture.app.clone(), &secret).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, headers, body) = call(fixture.app.clone(), &secret).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert!(
        body.contains("Capped") && body.contains("13 of its 13"),
        "{body}"
    );
    assert!(body.contains("resets at"), "{body}");
    assert!(headers.get("retry-after").is_some());
    assert!(headers.get("x-hofund-limit-reset").is_some());
    assert_eq!(
        headers
            .get("x-should-retry")
            .and_then(|value| value.to_str().ok()),
        Some("false")
    );
    assert_eq!(fixture.hits.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.usage.rows().expect("rows").len(), 2);
}

#[tokio::test]
async fn l7_limit_route_reports_all_and_binding_limits() {
    let fixture = fixture();
    let secret = limited_key(
        &fixture,
        KeyLimit {
            period: "day".to_owned(),
            tokens: 17,
            cost: 2.5,
            cache_reads: true,
            ..KeyLimit::default()
        },
    );
    let (status, _, body) = request(
        fixture.app.clone(),
        "GET",
        "/v1/hofund/limit",
        &fixture.store.token_file().read().expect("default token"),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let unlimited: serde_json::Value = serde_json::from_str(&body).expect("unlimited json");
    assert_eq!(unlimited["limited"], false);

    let (status, _, body) = call(fixture.app.clone(), &secret).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let usage = fixture.usage.key_usage("unused", 0);
    assert_eq!(usage.expect("unknown key").calls, 0);
    let key_id = fixture
        .store
        .authenticate(&secret)
        .expect("auth")
        .expect("identity")
        .key_id;
    let usage = fixture.usage.key_usage(&key_id, 0).expect("key usage");
    assert_eq!(usage.input_tokens, 6, "cached input is not counted twice");
    assert_eq!(usage.output_tokens, 5, "reasoning stays in output");
    assert_eq!(usage.cache_read_tokens, 4);
    assert_eq!(usage.cache_write_tokens, 2);
    assert_eq!(usage.tokens(false), 13);
    assert_eq!(usage.tokens(true), 17);
    assert_eq!(
        fixture.usage.rows().expect("ordinary rows")[0].input_tokens,
        Some(10),
        "the ordinary ledger keeps the protocol total"
    );

    let (status, _, body) =
        request(fixture.app.clone(), "GET", "/v1/hofund/limit", &secret, "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let value: serde_json::Value = serde_json::from_str(&body).expect("limit json");
    assert_eq!(value["limited"], true);
    assert_eq!(value["key"], "Capped");
    assert_eq!(value["limits"].as_array().expect("limits").len(), 1);
    assert_eq!(value["limit"]["period"], "day");
    assert_eq!(value["limit"]["tokens"], 17);
    assert_eq!(value["limit"]["tokenLimit"], 17);
    assert_eq!(value["limit"]["costLimit"], 2.5);
    assert_eq!(value["limit"]["cost"], 0.0);
    assert_eq!(value["limit"]["unpriced"], 1);
    assert_eq!(value["limit"]["cacheReads"], true);
    assert_eq!(value["limit"]["tokensLeft"], 0);
    assert!(
        value["limit"]["reset"]
            .as_str()
            .expect("reset")
            .contains('T')
    );
}
