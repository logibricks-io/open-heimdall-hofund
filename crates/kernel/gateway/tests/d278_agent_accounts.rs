//! D-278: a gateway request uses the agent's account and still lists the model.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use hofund_codec::{Codec, CodecError, StreamDecoder, StreamEncoder};
use hofund_gateway::{AgentAccountHook, Gateway, RequestAgentHook};
use hofund_ir::{ContentPart, Message, Request as ModelRequest, Response, Role, StreamEvent};
use hofund_plugin_api::{MemberId, MemberKey, PluginError, PresetId};
use hofund_router::{Completion, FailoverPolicy, Hop, Member, Router, Snapshot, affinity};
use hofund_secrets::KeyStore;
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
        Box::new(NoDecode)
    }

    fn stream_encoder(&self) -> Box<dyn StreamEncoder> {
        Box::new(NoEncode)
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

struct NoEncode;

impl StreamEncoder for NoEncode {
    fn push(&mut self, _event: &StreamEvent) -> Result<Vec<u8>, CodecError> {
        Ok(Vec::new())
    }
}

struct Fake {
    route_id: &'static str,
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
        self.route_id
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
}

fn member(route_id: &'static str, key_id: &'static str) -> (Arc<AtomicUsize>, Arc<dyn Member>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let member = Arc::new(Fake {
        route_id,
        key_id,
        member_id: MemberId::from_preset(
            PresetId::try_from(route_id).expect("preset"),
            MemberKey::try_from(key_id).expect("key"),
        ),
        hits: Arc::clone(&hits),
    });
    (hits, member)
}

fn hop(member: Arc<dyn Member>) -> Hop {
    Hop {
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
    }
}

struct App {
    router: axum::Router,
    first: Arc<AtomicUsize>,
    second: Arc<AtomicUsize>,
    token: String,
    _dir: tempfile::TempDir,
}

fn app(pins: AgentAccountHook) -> App {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Arc::new(KeyStore::beside_token(dir.path().join("hofund.token")));
    store.list().expect("migration");
    let token = store.token_file().read().expect("token");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let (first, first_member) = member("relay", "aaaaaaaaaa");
    let (second, second_member) = member("relay", "bbbbbbbbbb");
    let routes = BTreeMap::from([(
        "demo".to_owned(),
        vec![hop(first_member), hop(second_member)],
    )]);
    let snapshot = Snapshot::new(Router::from_hops(routes, FailoverPolicy::default()));
    let codecs = HashMap::from([("/v1/plain".to_owned(), Arc::new(Plain) as Arc<dyn Codec>)]);
    let agents: RequestAgentHook = Arc::new(|headers, _| {
        headers
            .get("user-agent")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    });
    let router = Gateway::new(snapshot, token.clone(), codecs, usage)
        .with_key_store(store)
        .with_request_agent(agents)
        .with_agent_accounts(pins)
        .into_router();
    App {
        router,
        first,
        second,
        token,
        _dir: dir,
    }
}

async fn call(
    app: axum::Router,
    method: &str,
    uri: &str,
    agent: &str,
    token: &str,
    body: &str,
) -> (StatusCode, String) {
    let response = app
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("authorization", format!("Bearer {token}"))
                .header("user-agent", agent)
                .body(Body::from(body.to_owned()))
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
async fn d278_gateway_holds_each_agent_and_still_lists_the_model() {
    let pins: AgentAccountHook = Arc::new(|agent| match agent {
        "writer" => vec!["relay/bbbbbbbbbb".to_owned()],
        "missing" => vec!["relay/cccccccccc".to_owned()],
        _ => Vec::new(),
    });
    let app = app(pins);
    let (status, body) = call(
        app.router.clone(),
        "GET",
        "/v1/models",
        "writer",
        &app.token,
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("demo"), "the model list stays open: {body}");

    let (status, body) = call(
        app.router.clone(),
        "POST",
        "/v1/plain",
        "writer",
        &app.token,
        r#"{"model":"demo"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(app.first.load(Ordering::SeqCst), 0);
    assert_eq!(app.second.load(Ordering::SeqCst), 1);

    let (status, body) = call(
        app.router,
        "POST",
        "/v1/plain",
        "missing",
        &app.token,
        r#"{"model":"demo"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body.contains("missing"), "{body}");
    assert!(body.contains("Accounts by agent"), "{body}");
    assert_eq!(app.first.load(Ordering::SeqCst), 0);
    assert_eq!(app.second.load(Ordering::SeqCst), 1);
}
