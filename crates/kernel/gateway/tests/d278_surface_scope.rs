//! D-278: inherited surface scopes and token counts keep the agent's pin.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use hofund_codec::{Codec, CodecError, StreamDecoder, StreamEncoder};
use hofund_gateway::{AgentAccountHook, Gateway, RequestAgentHook};
use hofund_ir::{ContentPart, Message, Request as ModelRequest, Response, Role, StreamEvent};
use hofund_plugin_api::{
    BodyStream, CountOutcome, Forward, InboundSurface, MemberId, MemberKey, Method, PluginError,
    PresetId, RawAnswer, RawBody, RouteSpec, SurfaceHost, SurfaceId,
};
use hofund_router::{Completion, FailoverPolicy, Hop, Member, Router, Snapshot, affinity};
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
        Ok(ModelRequest::new(model))
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

    fn encode_stream(&self, _events: &[StreamEvent]) -> Result<Vec<u8>, CodecError> {
        Ok(Vec::new())
    }

    fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(NoStream)
    }

    fn stream_encoder(&self) -> Box<dyn StreamEncoder> {
        Box::new(NoStream)
    }

    fn encode_error(&self, _status: u16, message: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({"message": message})).unwrap_or_default()
    }
}

struct NoStream;

impl StreamDecoder for NoStream {
    fn push(&mut self, _chunk: &[u8]) -> Result<Vec<StreamEvent>, CodecError> {
        Ok(Vec::new())
    }

    fn finish(&mut self) -> Result<Vec<StreamEvent>, CodecError> {
        Ok(Vec::new())
    }
}

impl StreamEncoder for NoStream {
    fn push(&mut self, _event: &StreamEvent) -> Result<Vec<u8>, CodecError> {
        Ok(Vec::new())
    }
}

struct Fake {
    key_id: &'static str,
    member_id: MemberId,
    hits: Arc<AtomicUsize>,
}

#[async_trait]
impl Member for Fake {
    fn id(&self) -> &MemberId {
        &self.member_id
    }

    fn route_id(&self) -> &str {
        "relay"
    }

    fn provider_key_id(&self) -> &str {
        self.key_id
    }

    fn protocol(&self) -> &str {
        "plain"
    }

    fn account_refs(&self) -> Vec<String> {
        vec![self.key_id.to_owned()]
    }

    async fn execute(
        &self,
        _request: ModelRequest,
        _inbound: &str,
    ) -> Result<Completion, PluginError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        let mut message = Message::new(Role::Assistant);
        message.content.push(ContentPart::text("ok"));
        Ok(Completion::Unary(Box::new(Response::new(message))))
    }

    async fn count_input(
        &self,
        _request: ModelRequest,
        _inbound: &str,
    ) -> Result<CountOutcome, PluginError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        Ok(CountOutcome::Counted(7))
    }

    async fn forward(&self, _path: &str, _body: RawBody) -> Result<RawAnswer, PluginError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        Ok(RawAnswer::new(200, None, b"{}".to_vec()))
    }
}

static SURFACE_ID: SurfaceId = SurfaceId::from_static("scope-fixture");
static SURFACE_ROUTES: [RouteSpec; 2] = [
    RouteSpec::from_static(Method::Post, "/surface/call"),
    RouteSpec::from_static(Method::Post, "/surface/forward"),
];

struct ScopedSurface;

#[async_trait]
impl InboundSurface for ScopedSurface {
    fn id(&self) -> &SurfaceId {
        &SURFACE_ID
    }

    fn routes(&self) -> &'static [RouteSpec] {
        &SURFACE_ROUTES
    }

    async fn handle(
        &self,
        _route: &RouteSpec,
        request: Request<hofund_plugin_api::Body>,
        host: &SurfaceHost,
    ) -> Result<axum::http::Response<BodyStream>, PluginError> {
        if request.uri().path() == "/surface/call" {
            let call = Request::builder()
                .method("POST")
                .uri("/v1/plain")
                .header("content-type", "application/json")
                .body(hofund_plugin_api::Body::from_bytes(
                    br#"{"model":"demo"}"#.to_vec(),
                ))
                .expect("in-process request without user-agent");
            return host.gateway().call(call).await;
        }
        let call = Forward::new("demo", "/embeddings", |wire| {
            Ok(RawBody::new(
                "application/json",
                format!("{{\"model\":\"{wire}\"}}"),
            ))
        })?;
        let answer = host.gateway().forward(call).await?;
        let status = answer.status();
        Ok(axum::http::Response::builder()
            .status(status)
            .body(answer.into_body_stream())
            .expect("surface response"))
    }
}

struct Fixture {
    app: axum::Router,
    first: Arc<AtomicUsize>,
    second: Arc<AtomicUsize>,
    _dir: tempfile::TempDir,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("temp dir");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let first = Arc::new(AtomicUsize::new(0));
    let second = Arc::new(AtomicUsize::new(0));
    let member = |key_id: &'static str, hits: &Arc<AtomicUsize>| {
        Arc::new(Fake {
            key_id,
            member_id: MemberId::from_preset(
                PresetId::try_from("relay").expect("preset"),
                MemberKey::try_from(key_id).expect("key"),
            ),
            hits: Arc::clone(hits),
        }) as Arc<dyn Member>
    };
    let hop = |member| Hop {
        member,
        wire_model: Some("demo".to_owned()),
        max_output_tokens: None,
        catalog_output_tokens: None,
        context_window: None,
        patience: None,
        weight: 1,
        free: false,
        nearly_spent: false,
        fallback: false,
        effort: String::new(),
        cold_after: affinity::CACHE_COLD,
        service_tier: String::new(),
        units: Vec::new(),
    };
    let routes = BTreeMap::from([(
        "demo".to_owned(),
        vec![
            hop(member("aaaaaaaaaa", &first)),
            hop(member("bbbbbbbbbb", &second)),
        ],
    )]);
    let codecs = HashMap::from([
        ("/v1/plain".to_owned(), Arc::new(Plain) as Arc<dyn Codec>),
        ("/v1/messages".to_owned(), Arc::new(Plain) as Arc<dyn Codec>),
    ]);
    let agents: RequestAgentHook = Arc::new(|headers, _| {
        headers
            .get("user-agent")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    });
    let pins: AgentAccountHook = Arc::new(|agent| {
        if agent == "writer" {
            vec!["relay/bbbbbbbbbb".to_owned()]
        } else {
            Vec::new()
        }
    });
    let app = Gateway::new(
        Snapshot::new(Router::from_hops(routes, FailoverPolicy::default())),
        "secret".to_owned(),
        codecs,
        usage,
    )
    .with_request_agent(agents)
    .with_agent_accounts(pins)
    .with_surfaces(vec![Arc::new(ScopedSurface) as Arc<dyn InboundSurface>])
    .into_router();
    Fixture {
        app,
        first,
        second,
        _dir: dir,
    }
}

async fn send(app: axum::Router, path: &str) -> (StatusCode, String) {
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("authorization", "Bearer secret")
                .header("user-agent", "writer")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"model":"demo"}"#))
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

#[tokio::test]
async fn d278_surface_call_and_forward_inherit_the_agent_pin_without_headers() {
    let fixture = fixture();
    for path in ["/surface/call", "/surface/forward"] {
        let (status, body) = send(fixture.app.clone(), path).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
    }
    assert_eq!(fixture.first.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.second.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn d278_count_tokens_asks_only_the_pinned_member() {
    let fixture = fixture();
    let (status, body) = send(fixture.app, "/v1/messages/count_tokens").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(fixture.first.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.second.load(Ordering::SeqCst), 1);
}
