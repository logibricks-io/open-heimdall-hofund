//! Pre-route unrouted hold lifecycle over HTTP.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use hofund_codec::{Codec, OpenAiChat};
use hofund_gateway::Gateway;
use hofund_ir::{Message, Request as ModelRequest, Response, Role, Usage};
use hofund_plugin_api::{MemberId, MemberKey, PluginError, PresetId};
use hofund_router::{
    Completion, FailoverPolicy, Hop, Member, RouteTraceBook, Router, Snapshot, TraceNow,
    UNROUTED_SHOW,
};
use hofund_secrets::KeyStore;
use hofund_usage::Ledger;
use tower::ServiceExt;

const BODY_LIMIT: usize = 8 * 1024 * 1024;

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
            input_tokens: Some(1),
            output_tokens: Some(1),
            ..Usage::default()
        }));
        Ok(Completion::Unary(Box::new(response)))
    }
}

fn test_clock(start: u64) -> (TraceNow, Arc<AtomicU64>) {
    let millis = Arc::new(AtomicU64::new(start));
    let read = Arc::clone(&millis);
    let now: TraceNow = Arc::new(move || read.load(Ordering::SeqCst));
    (now, millis)
}

struct Fixture {
    app: axum::Router,
    book: Arc<RouteTraceBook>,
    clock: Arc<AtomicU64>,
    secret: String,
    _dir: tempfile::TempDir,
}

fn fixture() -> Fixture {
    fixture_with(None)
}

fn fixture_with(slow: Option<Arc<dyn Member>>) -> Fixture {
    let dir = tempfile::tempdir().expect("dir");
    let store = Arc::new(KeyStore::beside_token(dir.path().join("hofund.token")));
    store.list().expect("migration");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let hits = Arc::new(AtomicUsize::new(0));
    let member: Arc<dyn Member> = slow.unwrap_or_else(|| {
        Arc::new(Fake {
            member_id: MemberId::from_preset(
                PresetId::try_from("primary").expect("preset"),
                MemberKey::from_static("row"),
            ),
            hits: Arc::clone(&hits),
        })
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
            units: Vec::new(),
        }],
    )]);
    let (clock, millis) = test_clock(0);
    let book = RouteTraceBook::with_clock(clock);
    let token = store.token_file().read().expect("token");
    let secret = token.clone();
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
    .with_route_trace(Arc::clone(&book))
    .into_router();
    Fixture {
        app,
        book,
        clock: millis,
        secret,
        _dir: dir,
    }
}

async fn post(app: axum::Router, secret: &str, body: Body) -> StatusCode {
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("authorization", format!("Bearer {secret}"))
                .body(body)
                .expect("request"),
        )
        .await
        .expect("response");
    response.status()
}

#[tokio::test]
async fn r1_17_early_error_responses_drop_unrouted_holds() {
    let fixture = fixture();

    let oversized = vec![b'x'; BODY_LIMIT + 1];
    let status = post(fixture.app.clone(), &fixture.secret, Body::from(oversized)).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);

    let status = post(
        fixture.app.clone(),
        &fixture.secret,
        Body::from("{not valid json"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    fixture
        .clock
        .store(UNROUTED_SHOW.as_millis() as u64 + 1, Ordering::SeqCst);
    assert!(
        fixture.book.unrouted().holds.is_empty(),
        "early failures must not leak holds into the unrouted view"
    );
}

struct Slow {
    member_id: MemberId,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl Member for Slow {
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
        self.entered.notify_one();
        self.release.notified().await;
        let response = Response::new(Message::new(Role::Assistant)).with_usage(Some(Usage {
            input_tokens: Some(1),
            output_tokens: Some(1),
            ..Usage::default()
        }));
        Ok(Completion::Unary(Box::new(response)))
    }
}

#[tokio::test]
async fn r1_31_hold_is_released_once_routing_begins() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let slow: Arc<dyn Member> = Arc::new(Slow {
        member_id: MemberId::from_preset(
            PresetId::try_from("primary").expect("preset"),
            MemberKey::from_static("row"),
        ),
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
    });
    let fixture = fixture_with(Some(slow));
    let app = fixture.app.clone();
    let secret = fixture.secret.clone();
    let call = tokio::spawn(async move {
        post(
            app,
            &secret,
            Body::from(r#"{"model":"demo","messages":[{"role":"user","content":"hi"}]}"#),
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), entered.notified())
        .await
        .expect("member reached");
    fixture
        .clock
        .store(UNROUTED_SHOW.as_millis() as u64 + 1, Ordering::SeqCst);
    let holds = fixture.book.unrouted().holds;
    release.notify_one();
    let status = call.await.expect("join");
    assert!(
        holds.is_empty(),
        "a request being served must not stay in the unrouted view"
    );
    assert_eq!(status, StatusCode::OK);
}
