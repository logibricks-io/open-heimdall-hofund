//! Named-key authentication, attribution, and model/account scopes.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use hofund_codec::{Codec, CodecError, StreamDecoder, StreamEncoder};
use hofund_gateway::Gateway;
use hofund_ir::{ContentPart, Message, Request as ModelRequest, Response, Role, StreamEvent};
use hofund_plugin_api::{
    CountOutcome, Forward, MemberId, MemberKey, PluginError, PresetId, RawAnswer, RawBody,
};
use hofund_router::{Completion, FailoverPolicy, Hop, Member, Router, Snapshot};
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
    member_id: MemberId,
    account: &'static str,
    hits: Arc<AtomicUsize>,
}

#[async_trait]
impl Member for Fake {
    fn id(&self) -> &MemberId {
        &self.member_id
    }

    fn route_id(&self) -> &str {
        self.id
    }

    fn protocol(&self) -> &str {
        "plain"
    }

    fn account_refs(&self) -> Vec<String> {
        vec![self.account.to_owned()]
    }

    async fn execute(
        &self,
        _request: ModelRequest,
        _inbound: &str,
    ) -> Result<Completion, PluginError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        Ok(Completion::Unary(Box::new(Response::new(Message::new(
            Role::Assistant,
        )))))
    }

    async fn count_input(
        &self,
        _request: ModelRequest,
        _inbound: &str,
    ) -> Result<CountOutcome, PluginError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        Ok(CountOutcome::Counted(4))
    }

    async fn forward(&self, _path: &str, _body: RawBody) -> Result<RawAnswer, PluginError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        Ok(RawAnswer::new(200, None, b"{}".to_vec()))
    }
}

struct Fixture {
    app: axum::Router,
    snapshot: Snapshot,
    store: Arc<KeyStore>,
    usage: Arc<Ledger>,
    hits: Arc<AtomicUsize>,
    _dir: tempfile::TempDir,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Arc::new(KeyStore::beside_token(dir.path().join("hofund.token")));
    store.list().expect("migration");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let hits = Arc::new(AtomicUsize::new(0));
    let member = |id: &'static str, account: &'static str| {
        Arc::new(Fake {
            id,
            member_id: MemberId::from_preset(
                PresetId::try_from(id).expect("preset"),
                MemberKey::from_static("row"),
            ),
            account,
            hits: Arc::clone(&hits),
        }) as Arc<dyn Member>
    };
    let hop = |id, account| Hop {
        member: member(id, account),
        wire_model: Some("wire".to_owned()),
        max_output_tokens: None,
        context_window: None,
        patience: None,
        weight: 1,
        free: false,
    };
    let routes = BTreeMap::from([
        ("demo".to_owned(), vec![hop("primary", "account-a")]),
        ("other".to_owned(), vec![hop("spare", "account-b")]),
    ]);
    let snapshot = Snapshot::new(Router::from_hops(routes, FailoverPolicy::default()));
    let codecs = HashMap::from([("/v1/plain".to_owned(), Arc::new(Plain) as Arc<dyn Codec>)]);
    let token = store.token_file().read().expect("token");
    let app = Gateway::new(snapshot.clone(), token, codecs, Arc::clone(&usage))
        .with_key_store(Arc::clone(&store))
        .into_router();
    Fixture {
        app,
        snapshot,
        store,
        usage,
        hits,
        _dir: dir,
    }
}

async fn request(
    app: axum::Router,
    method: &str,
    uri: &str,
    header: Option<(&str, &str)>,
    body: &str,
) -> (StatusCode, String) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some((name, value)) = header {
        builder = builder.header(name, value);
    }
    let response = app
        .oneshot(builder.body(Body::from(body.to_owned())).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn chat(model: &str) -> String {
    format!(r#"{{"model":"{model}","messages":[{{"role":"user","content":"hi"}}]}}"#)
}

#[tokio::test]
async fn k4_authentication_accepts_every_header_form_and_refuses_off_or_unknown_keys() {
    let fixture = fixture();
    let added = fixture
        .store
        .add("Named", Some("named-secret-123"))
        .expect("add");
    for (uri, header) in [
        (
            "/v1/models",
            Some(("authorization", "Bearer named-secret-123")),
        ),
        ("/v1/models", Some(("x-api-key", "named-secret-123"))),
        ("/v1/models", Some(("x-goog-api-key", "named-secret-123"))),
        ("/v1/models?key=named-secret-123", None),
    ] {
        let (status, _) = request(fixture.app.clone(), "GET", uri, header, "").await;
        assert_eq!(status, StatusCode::OK, "{uri}");
    }
    let (status, _) = request(fixture.app.clone(), "GET", "/v1/models", None, "").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    fixture
        .store
        .set_enabled(&added.key.id, false)
        .expect("off");
    let (status, _) = request(
        fixture.app.clone(),
        "GET",
        "/v1/models",
        Some(("authorization", "Bearer named-secret-123")),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = request(
        fixture.app,
        "GET",
        "/v1/models",
        Some(("authorization", "Bearer unknown-secret")),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn k5_identity_is_carried_into_usage_rows() {
    let fixture = fixture();
    let added = fixture
        .store
        .add("Usage client", Some("usage-secret-123"))
        .expect("add");
    let (status, body) = request(
        fixture.app,
        "POST",
        "/v1/plain",
        Some(("authorization", "Bearer usage-secret-123")),
        &chat("demo"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rows = fixture.usage.rows().expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].caller_key_id, added.key.id);
    assert_eq!(rows[0].caller_key_name, "Usage client");
}

#[tokio::test]
async fn k8_accounts_only_key_holds_models_on_every_route_and_the_list() {
    let fixture = fixture();
    let added = fixture
        .store
        .add("Accounts", Some("account-secret-123"))
        .expect("add");
    fixture
        .store
        .set_accounts(
            &added.key.id,
            &["primary/account-a".to_owned()],
            &["primary/account-a".to_owned(), "spare/account-b".to_owned()],
        )
        .expect("accounts");
    let (status, body) = request(
        fixture.app.clone(),
        "GET",
        "/v1/models",
        Some(("authorization", "Bearer account-secret-123")),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("demo"));
    assert!(!body.contains("other"));
    let identity = fixture
        .store
        .authenticate("account-secret-123")
        .expect("auth")
        .expect("identity");
    let scope = hofund_router::RouteScope {
        models: identity.models,
        accounts: identity.accounts,
    };
    let router = fixture.snapshot.load();
    assert!(router.models_scoped(Some(&scope)) == ["demo"]);
    let other = ModelRequest::new("other");
    assert!(
        router
            .dispatch_scoped(&other, "plain", Some(&scope), None)
            .await
            .is_err()
    );
    assert!(
        router
            .count_scoped(&other, "plain", Some(&scope))
            .await
            .is_err()
    );
    let forward = Forward::new("other", "/v1/embeddings", |model| {
        Ok(RawBody::new(
            "application/json",
            format!("{{\"model\":\"{model}\"}}"),
        ))
    })
    .expect("forward");
    assert!(router.forward_scoped(&forward, Some(&scope)).await.is_err());
    assert_eq!(fixture.hits.load(Ordering::SeqCst), 0);
    let demo = ModelRequest::new("demo");
    assert!(
        router
            .dispatch_scoped(&demo, "plain", Some(&scope), None)
            .await
            .is_ok()
    );
    assert!(
        router
            .count_scoped(&demo, "plain", Some(&scope))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn k9_model_outside_scope_is_403_with_wording_and_the_list_is_filtered() {
    let fixture = fixture();
    let added = fixture
        .store
        .add("Scoped", Some("model-secret-123"))
        .expect("add");
    fixture
        .store
        .set_models(&added.key.id, &["primary/*".to_owned()])
        .expect("models");
    let (status, body) = request(
        fixture.app.clone(),
        "POST",
        "/v1/plain",
        Some(("authorization", "Bearer model-secret-123")),
        &chat("other"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body.contains("Scoped"), "{body}");
    assert!(body.contains("primary/*"), "{body}");
    assert_eq!(fixture.hits.load(Ordering::SeqCst), 0);
    let (status, body) = request(
        fixture.app,
        "GET",
        "/v1/models",
        Some(("authorization", "Bearer model-secret-123")),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("demo"));
    assert!(!body.contains("other"));
}
