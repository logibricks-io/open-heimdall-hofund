//! The HTTP edge, with a stand-in codec and provider. No vendor names.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use bytes::Bytes;
use futures_util::stream;
use hofund_codec::{
    AnthropicMessages, Codec, CodecError, GenerativeLanguage, OpenAiChat, OpenAiResponses,
    StreamDecoder, StreamEncoder,
};
use hofund_gateway::Gateway;
use hofund_gateway::{GENERATIVE_STREAM, GENERATIVE_UNARY};
use hofund_ir::{
    ContentPart, Extensions, Message, Request as ModelRequest, Response, Role, StreamEvent,
    TextDelta,
};
use hofund_plugin_api::{BodyStream, MemberId, MemberKey, PluginError, PresetId};
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
        if value.get("broken").and_then(serde_json::Value::as_bool) == Some(true) {
            return Err(CodecError::Unsupported {
                message: "broken".to_owned(),
            });
        }
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
    route_id: &'static str,
    member_id: MemberId,
    hits: Arc<AtomicUsize>,
    fail: Option<u16>,
    stream: bool,
}

#[async_trait]
impl Member for Fake {
    fn id(&self) -> &MemberId {
        &self.member_id
    }

    fn route_id(&self) -> &str {
        self.route_id
    }

    fn protocol(&self) -> &str {
        "plain"
    }

    async fn execute(
        &self,
        request: ModelRequest,
        _inbound_protocol: &str,
    ) -> Result<Completion, PluginError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        if let Some(status) = self.fail {
            return Err(PluginError::status_error(status, "nope"));
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
        Ok(Completion::Unary(Box::new(Response::new(message))))
    }
}

fn member_id(id: &'static str) -> MemberId {
    MemberId::from_preset(
        PresetId::try_from(id).expect("preset"),
        MemberKey::from_static("row"),
    )
}

fn gateway(members: Vec<Arc<dyn Member>>, token: &str) -> (axum::Router, tempfile::TempDir) {
    gateway_with(members, token, false)
}

fn gateway_with(
    members: Vec<Arc<dyn Member>>,
    token: &str,
    generative: bool,
) -> (axum::Router, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temp dir");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let mut routes = BTreeMap::new();
    routes.insert("demo".to_owned(), members);
    let mut codecs = HashMap::new();
    codecs.insert("/v1/plain".to_owned(), Arc::new(Plain) as Arc<dyn Codec>);
    if generative {
        codecs.insert(
            GENERATIVE_UNARY.to_owned(),
            Arc::new(GenerativeLanguage) as Arc<dyn Codec>,
        );
        codecs.insert(GENERATIVE_STREAM.to_owned(), Arc::new(GenerativeLanguage));
    }
    let app = Gateway::new(
        Snapshot::new(Router::new(routes, FailoverPolicy::default())),
        token.to_owned(),
        codecs,
        usage,
    )
    .into_router();
    (app, dir)
}

fn fake(id: &'static str, hits: &Arc<AtomicUsize>, fail: Option<u16>) -> Arc<dyn Member> {
    Arc::new(Fake {
        route_id: id,
        member_id: member_id(id),
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
    let (app, _dir) = gateway(vec![fake("primary", &hits, None)], "secret");
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
    let (app, _dir) = gateway(
        vec![
            Arc::new(Fake {
                route_id: "primary",
                member_id: member_id("primary"),
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
    assert!(!body.contains("event: error"));
    assert_eq!(second.load(Ordering::SeqCst), 0);

    let backup = Arc::new(AtomicUsize::new(0));
    let (app, _dir) = gateway(
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
    let (app, _dir) = gateway(
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
    let (app, _dir) = gateway(vec![fake("primary", &hits, None)], "secret");
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
        .clone()
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

    let (status, body) = call_with(
        app.clone(),
        "x-api-key",
        "secret",
        "/v1/plain",
        r#"{"model":"demo"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = call_with(app, "x-api-key", "nope", "/v1/plain", r#"{"model":"demo"}"#).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn generative_routes_inject_the_model_and_list_models() {
    let hits = Arc::new(AtomicUsize::new(0));
    let (app, _dir) = gateway_with(vec![fake("primary", &hits, None)], "secret", true);
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

#[tokio::test]
async fn health_is_open_and_names_no_secret() {
    let (app, _dir) = gateway(
        vec![fake("primary", &Arc::new(AtomicUsize::new(0)), None)],
        "secret",
    );
    let response = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("health");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let text = String::from_utf8_lossy(&bytes);
    assert_eq!(text, "{\"status\":\"ok\",\"service\":\"hofund\"}");
    assert!(!text.contains("secret"));
}

#[tokio::test]
async fn a_replaced_token_file_is_read_on_the_next_request() {
    let dir = tempfile::tempdir().expect("temp dir");
    let token_path = dir.path().join("token");
    std::fs::write(&token_path, "first\n").expect("token");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let mut routes = BTreeMap::new();
    routes.insert(
        "demo".to_owned(),
        vec![fake("primary", &Arc::new(AtomicUsize::new(0)), None)],
    );
    let mut codecs = HashMap::new();
    codecs.insert("/v1/plain".to_owned(), Arc::new(Plain) as Arc<dyn Codec>);
    let app = Gateway::with_token_file(
        Snapshot::new(Router::new(routes, FailoverPolicy::default())),
        "first".to_owned(),
        token_path.clone(),
        codecs,
        usage,
    )
    .into_router();
    let (status, _) = call(app.clone(), Some("first"), r#"{"model":"demo"}"#).await;
    assert_eq!(status, StatusCode::OK);
    std::fs::write(&token_path, "second\n").expect("rotate");
    let (status, _) = call(app.clone(), Some("first"), r#"{"model":"demo"}"#).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = call(app.clone(), Some("second"), r#"{"model":"demo"}"#).await;
    assert_eq!(status, StatusCode::OK);
    std::fs::write(&token_path, "\n").expect("empty");
    let (status, _) = call(app, Some("second"), r#"{"model":"demo"}"#).await;
    assert_eq!(status, StatusCode::OK);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_rejected_body_records_the_model_it_named() {
    let dir = tempfile::tempdir().expect("temp dir");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let mut routes = BTreeMap::new();
    routes.insert(
        "gpt-demo".to_owned(),
        vec![fake("primary", &Arc::new(AtomicUsize::new(0)), None)],
    );
    let mut codecs = HashMap::new();
    codecs.insert("/v1/plain".to_owned(), Arc::new(Plain) as Arc<dyn Codec>);
    let app = Gateway::new(
        Snapshot::new(Router::new(routes, FailoverPolicy::default())),
        "secret".to_owned(),
        codecs,
        Arc::clone(&usage),
    )
    .into_router();
    let (status, _) = call(app, Some("secret"), r#"{"model":"gpt-demo","broken":true}"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let rows = usage.rows().expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].model, "gpt-demo");
    assert_eq!(rows[0].status, "400");
    let _ = std::fs::remove_dir_all(&dir);
}

struct ThenFail {
    member_id: MemberId,
}

#[async_trait]
impl Member for ThenFail {
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
        _inbound_protocol: &str,
    ) -> Result<Completion, PluginError> {
        let event = StreamEvent::TextDelta(TextDelta {
            index: 0,
            text: "hi".to_owned(),
            extensions: Extensions::new(),
        });
        Ok(Completion::Stream(Box::pin(stream::iter(vec![
            Ok(event),
            Err(PluginError::status_error(502, "mid-stream")),
        ]))))
    }
}

#[tokio::test]
async fn a_stream_error_after_an_event_is_an_error_frame() {
    let (app, _dir) = gateway(
        vec![Arc::new(ThenFail {
            member_id: member_id("primary"),
        })],
        "secret",
    );
    let (status, body) = call(app, Some("secret"), r#"{"model":"demo","stream":true}"#).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("data: hi"), "{body}");
    assert!(body.contains("event: error"), "{body}");
    assert!(body.contains("mid-stream"), "{body}");
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

struct RawReply {
    member_id: MemberId,
    parts: Vec<Vec<u8>>,
    then_error: bool,
}

#[async_trait]
impl Member for RawReply {
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
        _inbound_protocol: &str,
    ) -> Result<Completion, PluginError> {
        let mut items: Vec<Result<Bytes, PluginError>> = self
            .parts
            .iter()
            .map(|part| Ok(Bytes::copy_from_slice(part)))
            .collect();
        if self.then_error {
            items.push(Err(PluginError::status_error(502, "mid-stream")));
        }
        Ok(Completion::Raw(BodyStream::from_stream(stream::iter(
            items,
        ))))
    }
}

fn raw_app(
    parts: Vec<Vec<u8>>,
    then_error: bool,
) -> (axum::Router, Arc<Ledger>, tempfile::TempDir) {
    raw_app_as(parts, then_error, "upstream")
}

fn raw_app_as(
    parts: Vec<Vec<u8>>,
    then_error: bool,
    wire: &str,
) -> (axum::Router, Arc<Ledger>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temp dir");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let mut routes = BTreeMap::new();
    routes.insert(
        "demo".to_owned(),
        vec![Hop {
            member: Arc::new(RawReply {
                member_id: member_id("primary"),
                parts,
                then_error,
            }) as Arc<dyn Member>,
            wire_model: Some(wire.to_owned()),
            max_output_tokens: None,
            context_window: None,
            weight: 1,
            free: false,
        }],
    );
    let mut codecs = HashMap::new();
    codecs.insert(
        "/v1/chat/completions".to_owned(),
        Arc::new(OpenAiChat) as Arc<dyn Codec>,
    );
    codecs.insert("/v1/responses".to_owned(), Arc::new(OpenAiResponses));
    codecs.insert("/v1/messages".to_owned(), Arc::new(AnthropicMessages));
    codecs.insert(GENERATIVE_UNARY.to_owned(), Arc::new(GenerativeLanguage));
    codecs.insert(GENERATIVE_STREAM.to_owned(), Arc::new(GenerativeLanguage));
    let app = Gateway::new(
        Snapshot::new(Router::from_hops(routes, FailoverPolicy::default())),
        "secret".to_owned(),
        codecs,
        Arc::clone(&usage),
    )
    .into_router();
    (app, usage, dir)
}

async fn post_raw(app: axum::Router, uri: &str, body: &str) -> (StatusCode, String, String) {
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("authorization", "Bearer secret")
                .body(Body::from(body.to_owned()))
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    (
        status,
        content_type,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

fn split_at(bytes: &'static [u8], marker: &str) -> Vec<Vec<u8>> {
    let index = bytes
        .windows(marker.len())
        .position(|window| window == marker.as_bytes())
        .unwrap_or_else(|| panic!("missing {marker}"));
    vec![bytes[..index].to_vec(), bytes[index..].to_vec()]
}

#[tokio::test]
async fn raw_passthrough_counts_usage_and_keeps_the_requested_model() {
    let chat_stream = concat!(
        "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"model\":\"upstream\",",
        "\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":7}}\n\n",
    );
    let chat_unary = br#"{"id":"c","object":"chat.completion","model":"upstream","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],"usage":{"prompt_tokens":11,"completion_tokens":7,"total_tokens":18}}"#;
    let messages_body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"type\":\"message\",",
        "\"role\":\"assistant\",\"model\":\"upstream\",\"content\":[],",
        "\"usage\":{\"input_tokens\":11,\"output_tokens\":1}}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},",
        "\"usage\":{\"output_tokens\":7}}\n\n",
    );
    let responses = concat!(
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"model\":\"upstream\",",
        "\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":11,\"output_tokens\":7}}}\n\n",
    );
    let generative = concat!(
        "data: {\"responseId\":\"g\",\"modelVersion\":\"upstream\",",
        "\"usageMetadata\":{\"promptTokenCount\":11,\"candidatesTokenCount\":7}}\n\n",
    );
    let generative_unary = br#"{"responseId":"g","modelVersion":"upstream","candidates":[{"content":{"role":"model","parts":[{"text":"hi"}]}}],"usageMetadata":{"promptTokenCount":11,"candidatesTokenCount":7}}"#;
    let chat_request =
        r#"{"model":"demo","messages":[{"role":"user","content":"hi"}],"stream":true}"#;
    let cases = [
        (
            "chat-stream",
            "/v1/chat/completions",
            chat_request,
            vec![chat_stream.as_bytes().to_vec()],
            "text/event-stream",
        ),
        (
            "chat-split",
            "/v1/chat/completions",
            chat_request,
            split_at(chat_stream.as_bytes(), "prompt_tokens"),
            "text/event-stream",
        ),
        (
            "chat-unary",
            "/v1/chat/completions",
            r#"{"model":"demo","messages":[{"role":"user","content":"hi"}]}"#,
            split_at(chat_unary, "prompt_tokens"),
            "application/json",
        ),
        (
            "messages",
            "/v1/messages",
            r#"{"model":"demo","max_tokens":16,"messages":[{"role":"user","content":"hi"}],"stream":true}"#,
            split_at(messages_body.as_bytes(), "input_tokens"),
            "text/event-stream",
        ),
        (
            "responses",
            "/v1/responses",
            r#"{"model":"demo","input":"hi","stream":true}"#,
            split_at(responses.as_bytes(), "input_tokens"),
            "text/event-stream",
        ),
        (
            "generative-stream",
            "/v1beta/models/demo:streamGenerateContent",
            r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#,
            split_at(generative.as_bytes(), "promptTokenCount"),
            "text/event-stream",
        ),
        (
            "generative-unary",
            "/v1beta/models/demo:generateContent",
            r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#,
            split_at(generative_unary, "promptTokenCount"),
            "application/json",
        ),
    ];
    for (name, uri, request, parts, content_type) in cases {
        let (app, usage, dir) = raw_app(parts, false);
        let (status, got_type, body) = post_raw(app, uri, request).await;
        assert_eq!(status, StatusCode::OK, "{name}: {body}");
        assert_eq!(got_type, content_type, "{name}");
        assert!(body.contains("demo"), "{name} model: {body}");
        assert!(
            !body.contains("upstream"),
            "{name} wire model leaked: {body}"
        );
        let rows = usage.rows().expect("rows");
        assert_eq!(rows.len(), 1, "{name}");
        assert_eq!(rows[0].model, "demo", "{name}");
        assert_eq!(rows[0].upstream, "primary", "{name}");
        assert_eq!(rows[0].input_tokens, Some(11), "{name}: {body}");
        assert_eq!(rows[0].output_tokens, Some(7), "{name}: {body}");
        let _ = std::fs::remove_dir_all(dir);
    }
}

const STREAM_CUT: &str = "data: {\"a\":1}\n\ndata: {\"b\":";

const CHAT_STREAM: &str =
    r#"{"model":"demo","messages":[{"role":"user","content":"hi"}],"stream":true}"#;

fn chat_error_frame(body: &str) -> &str {
    let start = body.find("data: {\"error\":").expect("error frame");
    &body[start..]
}

#[tokio::test]
async fn d21_a_severed_chat_stream_drops_the_half_event_and_ends_done() {
    let (app, usage, dir) = raw_app(vec![br#"data: {"n":1"#.to_vec()], true);
    let (status, content_type, body) = post_raw(app, "/v1/chat/completions", CHAT_STREAM).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(content_type, "text/event-stream");
    assert!(body.starts_with("data: {\"error\":"), "{body}");
    assert!(!body.contains("\"n\":1"), "{body}");
    assert!(body.contains("mid-stream"), "{body}");
    assert!(body.ends_with("}\n\ndata: [DONE]\n\n"), "{body}");
    assert_eq!(usage.rows().expect("rows")[0].status, "502");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn d21_a_severed_whole_chat_reply_gets_no_done() {
    let unary = r#"{"model":"demo","messages":[{"role":"user","content":"hi"}]}"#;
    let (app, usage, dir) = raw_app(vec![br#"{"id":"x","#.to_vec()], true);
    let (_, content_type, body) = post_raw(app, "/v1/chat/completions", unary).await;
    assert_ne!(content_type, "text/event-stream", "{body}");
    assert!(!body.contains("[DONE]"), "{body}");
    assert_eq!(usage.rows().expect("rows")[0].status, "502");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn d21_severed_mid_line_keeps_the_events_before_it() {
    for wire in ["upstream", "demo"] {
        let parts = vec![
            b"data: {\"a\":1}\n\ndata: {\"b\"".to_vec(),
            b":2,\"c\":".to_vec(),
        ];
        let (app, _usage, dir) = raw_app_as(parts, true, wire);
        let (status, _, body) = post_raw(app, "/v1/chat/completions", CHAT_STREAM).await;
        assert_eq!(status, StatusCode::OK, "{wire}: {body}");
        assert!(
            body.starts_with("data: {\"a\":1}\n\ndata: {\"error\":"),
            "{wire}: {body}"
        );
        assert!(!body.contains("\"b\""), "{wire}: {body}");
        assert!(body.ends_with("data: [DONE]\n\n"), "{wire}: {body}");
        assert_eq!(body.matches("[DONE]").count(), 1, "{wire}: {body}");
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[tokio::test]
async fn d21_severed_at_line_end_closes_the_event_before_the_error() {
    for wire in ["upstream", "demo"] {
        let (app, _usage, dir) = raw_app_as(vec![b"data: {\"a\":1}\n".to_vec()], true, wire);
        let (_, _, body) = post_raw(app, "/v1/chat/completions", CHAT_STREAM).await;
        assert!(
            body.starts_with("data: {\"a\":1}\n\ndata: {\"error\":"),
            "{wire}: {body}"
        );
        assert!(
            chat_error_frame(&body).ends_with("}\n\ndata: [DONE]\n\n"),
            "{body}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[tokio::test]
async fn d21_a_whole_data_line_short_of_its_end_still_goes_out() {
    for wire in ["upstream", "demo"] {
        let (app, _usage, dir) = raw_app_as(vec![b"data: {\"a\":1}".to_vec()], true, wire);
        let (_, _, body) = post_raw(app, "/v1/chat/completions", CHAT_STREAM).await;
        assert!(
            body.starts_with("data: {\"a\":1}\n\ndata: {\"error\":"),
            "{wire}: {body}"
        );
        assert!(body.ends_with("data: [DONE]\n\n"), "{wire}: {body}");
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[tokio::test]
async fn d21_a_severed_messages_stream_ends_on_its_error_event() {
    let parts = vec![
        concat!(
            "event: ping\ndata: {\"type\":\"ping\"}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_bl"
        )
        .as_bytes()
        .to_vec(),
    ];
    let (app, _usage, dir) = raw_app(parts, true);
    let (status, _, body) = post_raw(
        app,
        "/v1/messages",
        r#"{"model":"demo","max_tokens":16,"messages":[{"role":"user","content":"hi"}],"stream":true}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.starts_with(concat!(
            "event: ping\ndata: {\"type\":\"ping\"}\n\n",
            "event: content_block_delta\n\nevent: error\ndata: "
        )),
        "{body}"
    );
    assert!(!body.contains("content_bl\""), "{body}");
    assert!(!body.contains("content_bl\n"), "{body}");
    assert!(!body.contains("[DONE]"), "{body}");
    assert!(body.ends_with("}\n\n"), "{body}");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn d21_a_clean_end_sends_the_unended_line_as_it_came() {
    let stream = STREAM_CUT;
    let (app, usage, dir) = raw_app_as(split_at(STREAM_CUT.as_bytes(), "1}"), false, "demo");
    let (status, _, body) = post_raw(app, "/v1/chat/completions", CHAT_STREAM).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, stream);
    assert_eq!(usage.rows().expect("rows")[0].status, "200");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn a_comment_quote_does_not_leak_the_upstream_model() {
    let body = concat!(
        ": keep-alive \"\n\n",
        "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"model\":\"upstream-x\",",
        "\"choices\":[]}\n\n",
    );
    let (app, usage, dir) = raw_app(vec![body.as_bytes().to_vec()], false);
    let (status, _, got) = post_raw(
        app,
        "/v1/chat/completions",
        r#"{"model":"demo","messages":[{"role":"user","content":"hi"}],"stream":true}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{got}");
    assert!(got.contains(": keep-alive \""), "{got}");
    assert!(got.contains("\"model\":\"demo\""), "{got}");
    assert!(!got.contains("upstream-x"), "{got}");
    assert_eq!(usage.rows().expect("rows")[0].model, "demo");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn tool_arguments_keep_a_nested_model_field() {
    let messages = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"type\":\"message\",",
        "\"role\":\"assistant\",\"model\":\"upstream-x\",\"content\":[",
        "{\"type\":\"tool_use\",\"id\":\"t\",\"name\":\"car\",\"input\":{\"model\":\"tesla-3\"}}",
        "],\"usage\":{\"input_tokens\":11,\"output_tokens\":1}}}\n\n",
    );
    let (app, _, dir) = raw_app(vec![messages.as_bytes().to_vec()], false);
    let (status, _, got) = post_raw(
        app,
        "/v1/messages",
        r#"{"model":"demo","max_tokens":16,"messages":[{"role":"user","content":"hi"}],"stream":true}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{got}");
    assert!(got.contains("\"model\":\"demo\""), "{got}");
    assert!(got.contains("\"model\":\"tesla-3\""), "{got}");
    assert!(!got.contains("upstream-x"), "{got}");
    let _ = std::fs::remove_dir_all(dir);

    let language = br#"{"responseId":"g","modelVersion":"upstream-x","candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"car","args":{"model":"tesla-3"}}}]}}]}"#;
    let (app, _, dir) = raw_app(vec![language.to_vec()], false);
    let (status, content_type, got) = post_raw(
        app,
        "/v1beta/models/demo:generateContent",
        r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{got}");
    assert_eq!(content_type, "application/json");
    assert!(got.contains("\"modelVersion\":\"demo\""), "{got}");
    assert!(got.contains("\"model\":\"tesla-3\""), "{got}");
    assert!(!got.contains("upstream-x"), "{got}");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn a_crlf_frame_does_not_gain_an_extra_blank_event() {
    let chunk = concat!(
        "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"model\":\"upstream\",",
        "\"choices\":[]}\r\n\r\n",
    );
    let (app, _, dir) = raw_app(vec![chunk.as_bytes().to_vec()], true);
    let (status, _, body) = post_raw(
        app,
        "/v1/chat/completions",
        r#"{"model":"demo","messages":[{"role":"user","content":"hi"}],"stream":true}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("\"model\":\"demo\""), "{body}");
    assert!(!body.contains("upstream"), "{body}");
    assert!(!body.contains("\r\n\r\n\n\n"), "{body:?}");
    assert!(!body.contains("\n\n\n\n"), "{body:?}");
    assert!(body.contains("mid-stream"), "{body}");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn d21_a_held_rewriter_half_line_is_dropped_before_the_error_frame() {
    let (app, _, dir) = raw_app(vec![br#"data: {"model":"upstream"#.to_vec()], true);
    let (status, _, body) = post_raw(
        app,
        "/v1/chat/completions",
        r#"{"model":"demo","messages":[{"role":"user","content":"hi"}],"stream":true}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.starts_with("data: {\"error\":"), "{body}");
    assert!(!body.contains("upstream"), "{body}");
    assert!(body.ends_with("data: [DONE]\n\n"), "{body}");
    let _ = std::fs::remove_dir_all(dir);
}

enum EdgeFailure {
    Status(u16, &'static str),
    Transport,
}

struct Fails {
    route_id: &'static str,
    member_id: MemberId,
    hits: Arc<AtomicUsize>,
    failure: EdgeFailure,
}

#[async_trait]
impl Member for Fails {
    fn id(&self) -> &MemberId {
        &self.member_id
    }

    fn route_id(&self) -> &str {
        self.route_id
    }

    fn protocol(&self) -> &str {
        "plain"
    }

    async fn execute(
        &self,
        _request: ModelRequest,
        _inbound_protocol: &str,
    ) -> Result<Completion, PluginError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        Err(match self.failure {
            EdgeFailure::Status(status, message) => PluginError::status_error(status, message),
            EdgeFailure::Transport => PluginError::transport("reset after the request body"),
        })
    }
}

fn failure(id: &'static str, hits: &Arc<AtomicUsize>, failure: EdgeFailure) -> Arc<dyn Member> {
    Arc::new(Fails {
        route_id: id,
        member_id: member_id(id),
        hits: Arc::clone(hits),
        failure,
    })
}

fn protocol_app(members: Vec<Arc<dyn Member>>) -> (axum::Router, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temp dir");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let hops = members
        .into_iter()
        .map(|member| Hop {
            member,
            wire_model: None,
            max_output_tokens: None,
            context_window: None,
            weight: 1,
            free: false,
        })
        .collect();
    let routes = BTreeMap::from([("demo".to_owned(), hops)]);
    let codecs = HashMap::from([(
        "/v1/chat/completions".to_owned(),
        Arc::new(OpenAiChat) as Arc<dyn Codec>,
    )]);
    let app = Gateway::new(
        Snapshot::new(Router::from_hops(routes, FailoverPolicy::default())),
        "secret".to_owned(),
        codecs,
        usage,
    )
    .into_router();
    (app, dir)
}

#[tokio::test]
async fn c8_overflow_is_returned_in_the_client_protocol() {
    let second = Arc::new(AtomicUsize::new(0));
    let (app, dir) = protocol_app(vec![
        failure(
            "first",
            &Arc::new(AtomicUsize::new(0)),
            EdgeFailure::Status(400, "context_length_exceeded"),
        ),
        Arc::new(Fake {
            // Another account of the same provider and model holds no more.
            route_id: "first",
            member_id: member_id("second"),
            hits: Arc::clone(&second),
            fail: None,
            stream: false,
        }),
    ]);
    let (status, content_type, body) = post_raw(
        app,
        "/v1/chat/completions",
        r#"{"model":"demo","messages":[{"role":"user","content":"hi"}]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(content_type, "application/json");
    let body: serde_json::Value = serde_json::from_str(&body).expect("JSON error");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("context_length_exceeded")),
        "{body}"
    );
    assert!(body["error"]["type"].is_string(), "{body}");
    assert_eq!(second.load(Ordering::SeqCst), 0);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn c13_transport_error_before_any_byte_is_a_502() {
    let (app, dir) = protocol_app(vec![failure(
        "first",
        &Arc::new(AtomicUsize::new(0)),
        EdgeFailure::Transport,
    )]);
    let (status, _, body) = post_raw(
        app,
        "/v1/chat/completions",
        r#"{"model":"demo","messages":[{"role":"user","content":"hi"}]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert!(body.contains("reset after the request body"), "{body}");
    let _ = std::fs::remove_dir_all(dir);
}
