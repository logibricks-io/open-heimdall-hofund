//! The HTTP edge, with a stand-in codec and provider. No vendor names.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures_util::stream;
use hofund_codec::{Codec, CodecError, GenerativeLanguage, StreamDecoder, StreamEncoder};
use hofund_gateway::Gateway;
use hofund_gateway::{GENERATIVE_STREAM, GENERATIVE_UNARY};
use hofund_ir::{
    ContentPart, Extensions, Message, Request as ModelRequest, Response, Role, StreamEvent,
    TextDelta,
};
use hofund_router::{Completion, FailoverPolicy, Provider, ProviderError, Router};
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
    id: &'static str,
    hits: Arc<AtomicUsize>,
    fail: Option<u16>,
    stream: bool,
}

#[async_trait]
impl Provider for Fake {
    fn id(&self) -> &str {
        self.id
    }

    async fn execute(
        &self,
        request: ModelRequest,
        _inbound_protocol: &str,
    ) -> Result<Completion, ProviderError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        if let Some(status) = self.fail {
            return Err(ProviderError::status(status, "nope"));
        }
        if request.stream || self.stream {
            let event = StreamEvent::TextDelta(TextDelta {
                index: 0,
                text: "hi".to_owned(),
                extensions: Extensions::new(),
            });
            return Ok(Completion::Stream(Box::pin(stream::iter(vec![Ok(event)]))));
        }
        let mut message = Message::new(Role::Assistant);
        message.content.push(ContentPart::text("ok"));
        Ok(Completion::Unary(Box::new(Response {
            id: None,
            model: None,
            message,
            stop: None,
            usage: None,
            extensions: Extensions::new(),
        })))
    }
}

fn gateway(providers: Vec<Arc<dyn Provider>>, token: &str) -> axum::Router {
    gateway_with(providers, token, false)
}

fn gateway_with(providers: Vec<Arc<dyn Provider>>, token: &str, generative: bool) -> axum::Router {
    let dir = std::env::temp_dir().join(format!(
        "hofund-edge-{}-{}-{generative}",
        std::process::id(),
        providers.len()
    ));
    let _ = std::fs::create_dir_all(&dir);
    let usage = Arc::new(Ledger::open(&dir.join("usage.sqlite")).expect("usage"));
    let mut routes = BTreeMap::new();
    routes.insert("demo".to_owned(), providers);
    let mut codecs = HashMap::new();
    codecs.insert("/v1/plain".to_owned(), Arc::new(Plain) as Arc<dyn Codec>);
    if generative {
        codecs.insert(
            GENERATIVE_UNARY.to_owned(),
            Arc::new(GenerativeLanguage) as Arc<dyn Codec>,
        );
        codecs.insert(GENERATIVE_STREAM.to_owned(), Arc::new(GenerativeLanguage));
    }
    Gateway::new(
        Router::new(routes, FailoverPolicy::default()),
        token.to_owned(),
        codecs,
        usage,
    )
    .into_router()
}

fn fake(id: &'static str, hits: &Arc<AtomicUsize>, fail: Option<u16>) -> Arc<dyn Provider> {
    Arc::new(Fake {
        id,
        hits: Arc::clone(hits),
        fail,
        stream: false,
    })
}

async fn call(app: axum::Router, token: Option<&str>, body: &'static str) -> (StatusCode, String) {
    let mut builder = Request::builder().method("POST").uri("/v1/plain");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .oneshot(builder.body(Body::from(body)).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
async fn bearer_is_required_and_a_route_answers() {
    let hits = Arc::new(AtomicUsize::new(0));
    let app = gateway(vec![fake("primary", &hits, None)], "secret");
    let (status, _) = call(app.clone(), None, r#"{"model":"demo"}"#).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, body) = call(app.clone(), Some("nope"), r#"{"model":"demo"}"#).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body.contains("unauthorized"));
    let (status, body) = call(app.clone(), Some("secret"), r#"{"model":"demo"}"#).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("ok"));
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    let listed = app
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .header("authorization", "Bearer secret")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("models");
    assert_eq!(listed.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(listed.into_body(), usize::MAX)
        .await
        .expect("body");
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("demo"));
    assert!(text.contains("hofund"));
}

#[tokio::test]
async fn a_stream_is_pulled_and_a_client_error_does_not_fail_over() {
    let second = Arc::new(AtomicUsize::new(0));
    let app = gateway(
        vec![
            Arc::new(Fake {
                id: "primary",
                hits: Arc::new(AtomicUsize::new(0)),
                fail: None,
                stream: true,
            }),
            fake("backup", &second, None),
        ],
        "secret",
    );
    let (status, body) = call(app, Some("secret"), r#"{"model":"demo","stream":true}"#).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("data: hi"));
    assert_eq!(second.load(Ordering::SeqCst), 0);

    let backup = Arc::new(AtomicUsize::new(0));
    let app = gateway(
        vec![
            fake("primary", &Arc::new(AtomicUsize::new(0)), Some(400)),
            fake("backup", &backup, None),
        ],
        "secret",
    );
    let (status, _) = call(app, Some("secret"), r#"{"model":"demo"}"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(backup.load(Ordering::SeqCst), 0);

    let backup = Arc::new(AtomicUsize::new(0));
    let app = gateway(
        vec![
            fake("primary", &Arc::new(AtomicUsize::new(0)), Some(503)),
            fake("backup", &backup, None),
        ],
        "secret",
    );
    let (status, body) = call(app, Some("secret"), r#"{"model":"demo"}"#).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("ok"));
    assert_eq!(backup.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_header_secret_or_query_secret_is_accepted() {
    let hits = Arc::new(AtomicUsize::new(0));
    let app = gateway(vec![fake("primary", &hits, None)], "secret");
    let (status, _) = call_with(
        app.clone(),
        "x-goog-api-key",
        "nope",
        "/v1/plain",
        r#"{"model":"demo"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, body) = call_with(
        app.clone(),
        "x-goog-api-key",
        "secret",
        "/v1/plain",
        r#"{"model":"demo"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("ok"));

    let queried = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/plain?key=secret")
                .body(Body::from(r#"{"model":"demo"}"#))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(queried.status(), StatusCode::OK);
}

#[tokio::test]
async fn generative_routes_inject_the_model_and_list_models() {
    let hits = Arc::new(AtomicUsize::new(0));
    let app = gateway_with(vec![fake("primary", &hits, None)], "secret", true);
    let (status, body) = call_with(
        app.clone(),
        "x-goog-api-key",
        "secret",
        "/v1beta/models/demo:generateContent",
        r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("ok"), "{body}");
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    let (status, body) = call_with(
        app.clone(),
        "authorization",
        "Bearer secret",
        "/v1beta/models/demo:streamGenerateContent?alt=sse",
        r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("hi"), "{body}");

    let listed = app
        .oneshot(
            Request::builder()
                .uri("/v1beta/models")
                .header("x-goog-api-key", "secret")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("models");
    assert_eq!(listed.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(listed.into_body(), usize::MAX)
        .await
        .expect("body");
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("models/demo"), "{text}");
    assert!(text.contains("streamGenerateContent"), "{text}");
}

async fn call_with(
    app: axum::Router,
    header: &str,
    value: &str,
    uri: &str,
    body: &'static str,
) -> (StatusCode, String) {
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header(header, value)
                .body(Body::from(body))
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}
