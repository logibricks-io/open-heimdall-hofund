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
static ROUTES: [RouteSpec; 2] = [
    RouteSpec::from_static(Method::Post, "/embed"),
    RouteSpec::from_static(Method::Post, "/speech"),
];

/// Forwards the body's text as the model name to a retrieval path, and answers
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
        let path = if request.uri().path() == "/speech" {
            "/audio/speech"
        } else {
            "/embeddings"
        };
        let model = String::from_utf8_lossy(request.body().bytes()).into_owned();
        let call = Forward::new(model, path, |wire| {
            Ok(RawBody::new("application/json", format!("m={wire}")))
        })?;
        let (status, body) = match host.gateway().forward(call).await {
            Ok(answer) => (answer.status(), answer.into_body_stream()),
            Err(error) => (
                error.status().unwrap_or(599),
                BodyStream::from_bytes(
                    format!("{:?}:{}", error.kind(), error.message()).into_bytes(),
                ),
            ),
        };
        Ok(axum::http::Response::builder()
            .status(status)
            .body(body)
            .expect("response"))
    }
}

/// The member, status, and input and output tokens of a finished request.
type Ended = (Option<String>, u16, Option<u64>, Option<u64>);

#[derive(Default)]
struct Events {
    started: Mutex<Vec<(String, String)>>,
    finished: Mutex<Vec<Ended>>,
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
            context_window: None,
            patience: Some(std::time::Duration::from_secs(60)),
            weight: 1,
            free: false,
            nearly_spent: false,
            fallback: false,
            effort: String::new(),
            cold_after: hofund_router::affinity::CACHE_COLD,
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
    send(fixture, "/embed", model).await
}

async fn speech(fixture: &Fixture) -> (StatusCode, String) {
    send(fixture, "/speech", "vectors").await
}

async fn send(fixture: &Fixture, path: &str, model: &'static str) -> (StatusCode, String) {
    let request = Request::builder()
        .method("POST")
        .uri(path)
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

fn streamed_speech(chunks: Vec<Vec<u8>>) -> Result<RawAnswer, PluginError> {
    let body = BodyStream::unfold(chunks.into_iter(), |mut chunks| async move {
        chunks.next().map(|chunk| (Ok(chunk), chunks))
    });
    Ok(RawAnswer::streaming(
        200,
        Some("text/event-stream".to_owned()),
        body,
    ))
}

fn counted_speech() -> Result<RawAnswer, PluginError> {
    streamed_speech(vec![
        br#"data: {"type":"speech.audio.delta","audio":"SUQz"}"#.to_vec(),
        b"\n\ndata: {\"type\":\"speech.audio.done\",\"usage\":{\"input_tokens\":12,\"output_tokens\":340}}\n\n".to_vec(),
    ])
}

fn stale_speech_usage() -> Result<RawAnswer, PluginError> {
    streamed_speech(vec![
        b"data: {\"type\":\"speech.audio.done\",\"usage\":{\"input_tokens\":99,\"output_tokens\":99}}\n\n".to_vec(),
        format!(": {}\n", "x".repeat(16 * 1024)).into_bytes(),
    ])
}

fn speech_audio() -> Result<RawAnswer, PluginError> {
    Ok(RawAnswer::streaming(
        200,
        Some("audio/mpeg".to_owned()),
        BodyStream::from_bytes(b"audio".to_vec()),
    ))
}

#[tokio::test]
async fn p1_4_08_sse_usage_tail() {
    let fixture = fixture(&[("member", None, counted_speech)]);
    let (status, body) = speech(&fixture).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("speech.audio.done"), "{body}");
    let rows = fixture.usage.rows().expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].input_tokens, rows[0].output_tokens),
        (Some(12), Some(340))
    );
    assert_eq!(
        fixture.events.started.lock().expect("started").as_slice(),
        [("vectors".to_owned(), "speech".to_owned())]
    );
    assert_eq!(
        fixture.events.finished.lock().expect("finished").as_slice(),
        [(Some("member".to_owned()), 200, Some(12), Some(340))]
    );

    let stale = crate::fixture(&[("member", None, stale_speech_usage)]);
    let (status, _) = speech(&stale).await;
    assert_eq!(status, StatusCode::OK);
    let rows = stale.usage.rows().expect("rows");
    assert_eq!((rows[0].input_tokens, rows[0].output_tokens), (None, None));
}

fn vendor_json_speech_usage() -> Result<RawAnswer, PluginError> {
    streamed_speech(vec![
        br#"{"audio":"SUQz"}"#.to_vec(),
        b"\n{\"usage\":{\"input_tokens\":5,\"output_tokens\":7}}\n".to_vec(),
    ])
}

#[tokio::test]
async fn p1_4_13_usage_from_last_line_with_usage_whatever_its_type() {
    let fixture = fixture(&[("member", None, vendor_json_speech_usage)]);
    let (status, _) = speech(&fixture).await;
    assert_eq!(status, StatusCode::OK);
    let rows = fixture.usage.rows().expect("rows");
    assert_eq!(
        (rows[0].input_tokens, rows[0].output_tokens),
        (Some(5), Some(7))
    );
}

#[tokio::test]
async fn p1_4_09_errors_failover() {
    let fixture = fixture(&[
        ("first", Some("wire-a"), || {
            Err(PluginError::quota_status(429, "busy"))
        }),
        ("second", Some("wire-b"), speech_audio),
    ]);
    let (status, body) = speech(&fixture).await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, "audio"));
    assert_eq!(
        fixture.seen.lock().expect("seen").as_slice(),
        [
            "first/audio/speech:m=wire-a",
            "second/audio/speech:m=wire-b"
        ]
    );
    let rows = fixture.usage.rows().expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].upstream.as_str(), rows[0].status.as_str()),
        ("second", "200")
    );

    let refused = crate::fixture(&[("only", None, || {
        Err(PluginError::status_error(400, "voice refused"))
    })]);
    let (status, body) = speech(&refused).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("voice refused"), "{body}");
    assert_eq!(refused.usage.rows().expect("rows")[0].status, "400");
}
