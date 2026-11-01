//! Streaming patience and disconnects, without sockets or real time.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{StreamExt, stream};
use hofund_codec::{Codec, CodecError, StreamDecoder, StreamEncoder};
use hofund_gateway::{Gateway, RequestEnd, RequestEvents, RequestStart};
use hofund_ir::{
    ContentPart, Extensions, Request as ModelRequest, Response, StreamEvent, TextDelta,
};
use hofund_plugin_api::{MemberId, MemberKey, PluginError, PresetId};
use hofund_router::{Completion, FailoverPolicy, Hop, Member, Router, Snapshot};
use hofund_usage::Ledger;
use tower::ServiceExt;

struct Plain;

impl Codec for Plain {
    fn id(&self) -> &'static str {
        "plain"
    }

    fn decode_request(&self, bytes: &[u8]) -> Result<ModelRequest, CodecError> {
        let value: serde_json::Value = serde_json::from_slice(bytes)?;
        let model = value
            .get("model")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if model.is_empty() {
            return Err(CodecError::MissingField("model"));
        }
        let mut request = ModelRequest::new(model);
        request.stream = value
            .get("stream")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        Ok(request)
    }

    fn encode_request(&self, request: &ModelRequest) -> Result<Vec<u8>, CodecError> {
        serde_json::to_vec(&serde_json::json!({"model": request.model})).map_err(CodecError::from)
    }

    fn decode_response(&self, _bytes: &[u8]) -> Result<Response, CodecError> {
        Err(CodecError::Unsupported {
            message: "unused".to_owned(),
        })
    }

    fn encode_response(&self, response: &Response) -> Result<Vec<u8>, CodecError> {
        let text = response
            .message
            .content
            .iter()
            .find_map(|part| match part {
                ContentPart::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .unwrap_or_default();
        serde_json::to_vec(&serde_json::json!({"text": text})).map_err(CodecError::from)
    }

    fn encode_stream(&self, events: &[StreamEvent]) -> Result<Vec<u8>, CodecError> {
        let mut encoder = self.stream_encoder();
        let mut out = Vec::new();
        for event in events {
            out.extend(encoder.push(event)?);
        }
        Ok(out)
    }

    fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(NoDecode)
    }

    fn stream_encoder(&self) -> Box<dyn StreamEncoder> {
        Box::new(PlainEncoder)
    }

    fn encode_error(&self, _status: u16, message: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({"message": message})).unwrap_or_default()
    }
}

struct NoDecode;

impl StreamDecoder for NoDecode {
    fn push(&mut self, _chunk: &[u8]) -> Result<Vec<StreamEvent>, CodecError> {
        Ok(Vec::new())
    }

    fn finish(&mut self) -> Result<Vec<StreamEvent>, CodecError> {
        Ok(Vec::new())
    }
}

struct PlainEncoder;

impl StreamEncoder for PlainEncoder {
    fn push(&mut self, event: &StreamEvent) -> Result<Vec<u8>, CodecError> {
        match event {
            StreamEvent::TextDelta(delta) => Ok(format!("data: {}\n\n", delta.text).into_bytes()),
            _ => Ok(Vec::new()),
        }
    }
}

struct Fake {
    id: MemberId,
    hits: Arc<AtomicUsize>,
    answers: Mutex<VecDeque<Result<Completion, PluginError>>>,
}

#[async_trait]
impl Member for Fake {
    fn id(&self) -> &MemberId {
        &self.id
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
        self.answers
            .lock()
            .expect("answers")
            .pop_front()
            .unwrap_or_else(|| Err(PluginError::status_error(503, "busy")))
    }
}

#[derive(Default)]
struct Spy {
    ends: Mutex<Vec<RequestEnd>>,
}

impl RequestEvents for Spy {
    fn started(&self, _start: &RequestStart) {}

    fn finished(&self, end: &RequestEnd) {
        self.ends.lock().expect("ends").push(end.clone());
    }
}

struct Fixture {
    app: axum::Router,
    usage: Arc<Ledger>,
    hits: Arc<AtomicUsize>,
    spy: Arc<Spy>,
    _dir: tempfile::TempDir,
}

fn fixture(answers: Vec<Result<Completion, PluginError>>) -> Fixture {
    let dir = tempfile::tempdir().expect("temp dir");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let hits = Arc::new(AtomicUsize::new(0));
    let member = Arc::new(Fake {
        id: MemberId::from_preset(
            PresetId::from_static("primary"),
            MemberKey::from_static("row"),
        ),
        hits: Arc::clone(&hits),
        answers: Mutex::new(answers.into_iter().collect()),
    });
    let hop = Hop {
        member,
        wire_model: None,
        max_output_tokens: None,
        context_window: None,
        patience: Some(Duration::from_secs(60)),
        weight: 1,
        free: false,
        cold_after: hofund_router::affinity::CACHE_COLD,
        nearly_spent: false,
        fallback: false,
        effort: String::new(),
        units: Vec::new(),
    };
    let snapshot = Snapshot::new(Router::from_hops(
        BTreeMap::from([("demo".to_owned(), vec![hop])]),
        FailoverPolicy::default(),
    ));
    let codecs = HashMap::from([(
        "/v1/chat/completions".to_owned(),
        Arc::new(Plain) as Arc<dyn Codec>,
    )]);
    let spy = Arc::new(Spy::default());
    let app = Gateway::new(snapshot, "secret".to_owned(), codecs, Arc::clone(&usage))
        .with_request_events(spy.clone(), Arc::new(AtomicU64::new(0)))
        .into_router();
    Fixture {
        app,
        usage,
        hits,
        spy,
        _dir: dir,
    }
}

fn request() -> axum::http::Request<axum::body::Body> {
    request_for("primary/demo")
}

fn request_for(model: &str) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", "Bearer secret")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(format!(
            r#"{{"model":"{model}","stream":true,"messages":[]}}"#
        )))
        .expect("request")
}

fn text_answer() -> Completion {
    Completion::Stream(Box::pin(stream::iter(vec![Ok(StreamEvent::TextDelta(
        TextDelta {
            index: 0,
            text: "answered".to_owned(),
            extensions: Extensions::new(),
        },
    ))])))
}

#[tokio::test(start_paused = true)]
async fn b6b_streaming_patience_emits_sse_keepalive_comments() {
    let fixture = fixture(vec![
        Err(PluginError::status_error(503, "busy")),
        Ok(text_answer()),
    ]);
    let response = fixture.app.oneshot(request()).await.expect("response");
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let mut body = response.into_body().into_data_stream();
    let first = body.next().await.expect("keepalive").expect("chunk");
    assert_eq!(first, Bytes::from_static(b": keepalive\n\n"));
    assert_eq!(fixture.hits.load(Ordering::SeqCst), 1);

    let mut rest = Vec::new();
    while let Some(chunk) = body.next().await {
        rest.extend_from_slice(&chunk.expect("chunk"));
    }
    let text = String::from_utf8(rest).expect("utf8");
    assert!(text.contains("answered"), "{text}");
    assert_eq!(fixture.hits.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn b6c_disconnect_during_patience_records_499_and_stops_retries() {
    let fixture = fixture(vec![Err(PluginError::status_error(503, "busy"))]);
    let response = fixture.app.oneshot(request()).await.expect("held response");
    let mut body = response.into_body().into_data_stream();
    let first = body.next().await.expect("keepalive").expect("chunk");
    assert_eq!(first, Bytes::from_static(b": keepalive\n\n"));
    drop(body);
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(30)).await;
    tokio::task::yield_now().await;

    assert_eq!(fixture.hits.load(Ordering::SeqCst), 1);
    let ends = fixture.spy.ends.lock().expect("ends");
    assert_eq!(ends.len(), 1);
    assert_eq!(ends[0].status, 499);
    drop(ends);
    let rows = fixture.usage.rows().expect("usage rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, "499");
}

#[tokio::test(start_paused = true)]
async fn b11_a_route_request_is_held_with_keepalives_while_its_pool_waits() {
    let fixture = fixture(vec![
        Err(PluginError::status_error(503, "busy")),
        Ok(text_answer()),
    ]);
    let response = fixture
        .app
        .oneshot(request_for("demo"))
        .await
        .expect("response");
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let mut body = response.into_body().into_data_stream();
    let first = body.next().await.expect("keepalive").expect("chunk");
    assert_eq!(first, Bytes::from_static(b": keepalive\n\n"));
    let mut rest = Vec::new();
    while let Some(chunk) = body.next().await {
        rest.extend_from_slice(&chunk.expect("chunk"));
    }
    let text = String::from_utf8(rest).expect("utf8");
    assert!(text.contains("answered"), "{text}");
    assert_eq!(fixture.hits.load(Ordering::SeqCst), 2);
}
