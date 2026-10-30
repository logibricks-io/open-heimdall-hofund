//! An inbound surface forwards a raw call to the providers of a model, in
//! process. Stand-in surface and members. No vendor names.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use hofund_codec::{Codec, OpenAiChat};
use hofund_gateway::{Gateway, RequestEnd, RequestEvents, RequestStart};
use hofund_ir::Request as ModelRequest;
use hofund_plugin_api::{
    BodyStream, Forward, InboundSurface, MemberId, MemberKey, Method, PluginError, PresetId,
    RawAnswer, RawBody, RouteSpec, SurfaceHost, SurfaceId,
};
use hofund_router::{Completion, FailoverPolicy, Hop, Router, Snapshot};
use hofund_usage::Ledger;
use tower::ServiceExt;

type Answer = fn() -> Result<RawAnswer, PluginError>;

struct Fake {
    id: &'static str,
    member_id: MemberId,
    answer: Answer,
    seen: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl hofund_router::Member for Fake {
    fn id(&self) -> &MemberId {
        &self.member_id
    }

    fn route_id(&self) -> &str {
        self.id
    }

    fn protocol(&self) -> &str {
        "plain"
    }

    async fn execute(&self, _r: ModelRequest, _i: &str) -> Result<Completion, PluginError> {
        Err(PluginError::internal("not a chat member"))
    }

    async fn forward(&self, path: &str, body: RawBody) -> Result<RawAnswer, PluginError> {
        self.seen.lock().expect("seen").push(format!(
            "{}{path}:{}",
            self.id,
            String::from_utf8_lossy(body.bytes())
        ));
        (self.answer)()
    }
}

static ID: SurfaceId = SurfaceId::from_static("forwarder");
static ROUTES: [RouteSpec; 1] = [RouteSpec::from_static(Method::Post, "/embed")];

/// Forwards the body's text as the model name to `/embeddings`, and answers
/// with the provider's status and body, or the error's status and message.
struct Forwarder;

#[async_trait]
impl InboundSurface for Forwarder {
    fn id(&self) -> &SurfaceId {
        &ID
    }

    fn routes(&self) -> &'static [RouteSpec] {
        &ROUTES
    }

    async fn handle(
        &self,
        _route: &RouteSpec,
        request: axum::http::Request<hofund_plugin_api::Body>,
        host: &SurfaceHost,
    ) -> Result<axum::http::Response<BodyStream>, PluginError> {
        let model = String::from_utf8_lossy(request.body().bytes()).into_owned();
        let call = Forward::new(model, "/embeddings", |wire| {
            Ok(RawBody::new("application/json", format!("m={wire}")))
        })?;
        let (status, body) = match host.gateway().forward(call).await {
            Ok(answer) => (answer.status(), answer.body().to_vec()),
            Err(error) => (
                error.status().unwrap_or(599),
                format!("{:?}:{}", error.kind(), error.message()).into_bytes(),
            ),
        };
        Ok(axum::http::Response::builder()
            .status(status)
            .body(BodyStream::from_bytes(body))
            .expect("response"))
    }
}

#[derive(Default)]
struct Events {
    started: Mutex<Vec<(String, String)>>,
    finished: Mutex<Vec<(Option<String>, u16, Option<u64>, Option<u64>)>>,
}

impl RequestEvents for Events {
    fn started(&self, start: &RequestStart) {
        self.started
            .lock()
            .expect("started")
            .push((start.route.clone(), start.inbound_protocol.clone()));
    }

    fn finished(&self, end: &RequestEnd) {
        self.finished.lock().expect("finished").push((
            end.member.clone(),
            end.status,
            end.input_tokens,
            end.output_tokens,
        ));
    }
}

struct Fixture {
    app: axum::Router,
    usage: Arc<Ledger>,
    events: Arc<Events>,
    in_flight: Arc<AtomicU64>,
    seen: Arc<Mutex<Vec<String>>>,
    _dir: tempfile::TempDir,
}

fn fixture(members: &[(&'static str, Option<&str>, Answer)]) -> Fixture {
    let dir = tempfile::tempdir().expect("temp dir");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let hops = members
        .iter()
        .map(|(id, wire, answer)| Hop {
            member: Arc::new(Fake {
                id,
                member_id: MemberId::from_preset(
                    PresetId::try_from(*id).expect("preset"),
                    MemberKey::from_static("row"),
                ),
                answer: *answer,
                seen: Arc::clone(&seen),
            }),
            wire_model: wire.map(str::to_owned),
            max_output_tokens: None,
        })
        .collect();
    let mut routes = BTreeMap::new();
    routes.insert("vectors".to_owned(), hops);
    let mut codecs = HashMap::new();
    codecs.insert(
        "/v1/chat/completions".to_owned(),
        Arc::new(OpenAiChat) as Arc<dyn Codec>,
    );
    let events = Arc::new(Events::default());
    let in_flight = Arc::new(AtomicU64::new(0));
    let app = Gateway::new(
        Snapshot::new(Router::from_hops(routes, FailoverPolicy::default())),
        "secret".to_owned(),
        codecs,
        Arc::clone(&usage),
    )
    .with_request_events(
        Arc::clone(&events) as Arc<dyn RequestEvents>,
        Arc::clone(&in_flight),
    )
    .with_surfaces(vec![Arc::new(Forwarder) as Arc<dyn InboundSurface>])
    .into_router();
    Fixture {
        app,
        usage,
        events,
        in_flight,
        seen,
        _dir: dir,
    }
}

async fn embed(fixture: &Fixture, model: &'static str) -> (StatusCode, String) {
    let request = Request::builder()
        .method("POST")
        .uri("/embed")
        .header("authorization", "Bearer secret")
        .body(Body::from(model))
        .expect("request");
    let response = fixture
        .app
        .clone()
        .oneshot(request)
        .await
        .expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn embedded() -> Result<RawAnswer, PluginError> {
    Ok(RawAnswer::new(
        200,
        Some("application/json".to_owned()),
        br#"{"data":[],"usage":{"prompt_tokens":7,"total_tokens":7}}"#.to_vec(),
    ))
}

#[tokio::test]
async fn a_forward_reaches_the_member_and_writes_a_usage_row_and_events() {
    let fixture = fixture(&[
        ("first", Some("wire-a"), || {
            Err(PluginError::unsupported("no embeddings here"))
        }),
        ("second", Some("wire-b"), embedded),
    ]);
    let (status, body) = embed(&fixture, "vectors").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("prompt_tokens"), "{body}");
    assert_eq!(
        fixture.seen.lock().expect("seen").as_slice(),
        ["first/embeddings:m=wire-a", "second/embeddings:m=wire-b"]
    );
    let rows = fixture.usage.rows().expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].model, "vectors");
    assert_eq!(rows[0].upstream, "second");
    assert_eq!(rows[0].status, "200");
    assert_eq!(rows[0].input_tokens, Some(7));
    assert_eq!(rows[0].output_tokens, None);
    assert_eq!(
        fixture.events.started.lock().expect("started").as_slice(),
        [("vectors".to_owned(), "/embeddings".to_owned())]
    );
    assert_eq!(
        fixture.events.finished.lock().expect("finished").as_slice(),
        [(Some("second".to_owned()), 200, Some(7), None)]
    );
    assert_eq!(
        fixture.in_flight.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}

#[tokio::test]
async fn no_member_that_serves_the_path_is_a_not_found() {
    let fixture = fixture(&[
        ("first", None, || Err(PluginError::unsupported("no"))),
        ("second", None, || Err(PluginError::unsupported("no"))),
    ]);
    let (status, body) = embed(&fixture, "vectors").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        body,
        "Status:no provider of model `vectors` serves /embeddings"
    );
    let rows = fixture.usage.rows().expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].upstream, "second");
    assert_eq!(rows[0].status, "404");
    assert_eq!(
        fixture.events.finished.lock().expect("finished").as_slice(),
        [(Some("second".to_owned()), 404, None, None)]
    );
}

#[tokio::test]
async fn an_unknown_model_is_a_not_found_with_no_member() {
    let fixture = fixture(&[("first", None, embedded)]);
    let (status, body) = embed(&fixture, "other").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(body.starts_with("Status:"), "{body}");
    let rows = fixture.usage.rows().expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].upstream.as_str(), rows[0].status.as_str()),
        ("", "404")
    );
    assert_eq!(
        fixture.events.finished.lock().expect("finished").as_slice(),
        [(None, 404, None, None)]
    );
    assert!(fixture.seen.lock().expect("seen").is_empty());
}

#[tokio::test]
async fn an_upstream_failure_keeps_its_status_and_a_transport_failure_is_a_502() {
    let fixture = fixture(&[("first", None, || {
        Err(PluginError::status_error(400, "bad input"))
    })]);
    let (status, body) = embed(&fixture, "vectors").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, "Status:bad input");
    assert_eq!(fixture.usage.rows().expect("rows")[0].status, "400");

    let fixture = fixture_failing_connect();
    let (status, body) = embed(&fixture, "vectors").await;
    assert_eq!(status.as_u16(), 599, "{body}");
    assert_eq!(body, "Connect:refused");
    assert_eq!(fixture.usage.rows().expect("rows")[0].status, "502");
    assert_eq!(
        fixture.events.finished.lock().expect("finished").as_slice(),
        [(Some("first".to_owned()), 502, None, None)]
    );
}

fn fixture_failing_connect() -> Fixture {
    fixture(&[("first", None, || Err(PluginError::connect("refused")))])
}

#[tokio::test]
async fn a_name_two_providers_serve_is_a_bad_request() {
    let fixture = fixture(&[
        ("first", Some("same"), embedded),
        ("second", Some("same"), embedded),
    ]);
    let (status, body) = embed(&fixture, "same").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("more than one provider"), "{body}");
    let rows = fixture.usage.rows().expect("rows");
    assert_eq!(
        (rows[0].upstream.as_str(), rows[0].status.as_str()),
        ("", "400")
    );
    assert!(fixture.seen.lock().expect("seen").is_empty());
}
