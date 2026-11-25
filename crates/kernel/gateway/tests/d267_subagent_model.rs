//! A configured subagent model replaces the model used for routing and counts.
//!
//! The hook is neutral. Prompt openings and settings files are covered beside
//! the agent that owns them.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use hofund_codec::{Codec, CodecError, SUBAGENT_HEADER, StreamDecoder, StreamEncoder};
use hofund_gateway::{Gateway, SubagentModelChoice, SubagentModelHook};
use hofund_ir::{Effort, Message, Request as ModelRequest, Response, Role, StreamEvent};
use hofund_model_catalog::{CatalogFetcher, CatalogSettings, FetchResponse, ModelCatalog};
use hofund_plugin_api::{CountOutcome, MemberId, MemberKey, PluginError, PresetId};
use hofund_router::{Completion, FailoverPolicy, Member, RouteTraceBook, Router, Snapshot};
use hofund_secrets::Identity;
use hofund_usage::Ledger;
use serde_json::{Value, json};
use tower::ServiceExt;

const MARKER: &[u8] = b"INHERIT-TURN";

struct Plain;

impl Codec for Plain {
    fn id(&self) -> &'static str {
        "plain"
    }

    fn decode_request(&self, bytes: &[u8]) -> Result<ModelRequest, CodecError> {
        let value: Value = serde_json::from_slice(bytes)?;
        let model = value
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if model.is_empty() {
            return Err(CodecError::MissingField("model"));
        }
        let mut request = ModelRequest::new(model);
        if let Some(config) = value.get("output_config") {
            request.params.effort = config
                .get("effort")
                .and_then(Value::as_str)
                .and_then(Effort::from_label);
            request.extensions.insert("output_config", config.clone());
        }
        if let Some(thinking) = value.get("thinking") {
            request.extensions.insert("thinking", thinking.clone());
        }
        Ok(request)
    }

    fn encode_request(&self, request: &ModelRequest) -> Result<Vec<u8>, CodecError> {
        serde_json::to_vec(&json!({"model": request.model})).map_err(CodecError::from)
    }

    fn decode_response(&self, _bytes: &[u8]) -> Result<Response, CodecError> {
        Err(CodecError::Unsupported {
            message: "unused".to_owned(),
        })
    }

    fn encode_response(&self, _response: &Response) -> Result<Vec<u8>, CodecError> {
        Ok(br#"{"text":"ok"}"#.to_vec())
    }

    fn encode_stream(&self, _events: &[StreamEvent]) -> Result<Vec<u8>, CodecError> {
        Ok(Vec::new())
    }

    fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(NoDecode)
    }

    fn stream_encoder(&self) -> Box<dyn StreamEncoder> {
        Box::new(PlainEncoder)
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
    fn push(&mut self, _event: &StreamEvent) -> Result<Vec<u8>, CodecError> {
        Ok(Vec::new())
    }
}

struct Offline;

impl CatalogFetcher for Offline {
    fn fetch(&self, _url: &str, _if_none_match: Option<&str>) -> std::io::Result<FetchResponse> {
        Err(std::io::Error::other("offline"))
    }
}

struct Seen {
    model: String,
    effort: Option<Effort>,
    output_effort: Option<String>,
    thinking_disabled: bool,
    bridge_effort: Option<String>,
    think_off: Option<bool>,
}

struct Lane {
    member_id: MemberId,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Lane {
    fn new(key: &'static str) -> Arc<Self> {
        Arc::new(Self {
            member_id: MemberId::from_preset(
                PresetId::try_from("prov").expect("preset"),
                MemberKey::from_static(key),
            ),
            seen: Arc::new(Mutex::new(Vec::new())),
        })
    }

    fn note(&self, request: &ModelRequest) {
        let output_effort = request
            .extensions
            .get("output_config")
            .and_then(Value::as_object)
            .and_then(|config| config.get("effort"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let thinking_disabled = request
            .extensions
            .get("thinking")
            .and_then(Value::as_object)
            .and_then(|thinking| thinking.get("type"))
            .and_then(Value::as_str)
            == Some("disabled");
        let bridge_effort = request
            .extensions
            .get("hofund_bridge_effort")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let think_off = request
            .extensions
            .get("hofund_think_off")
            .and_then(Value::as_bool);
        self.seen.lock().expect("seen").push(Seen {
            model: request.model.clone(),
            effort: request.params.effort,
            output_effort,
            thinking_disabled,
            bridge_effort,
            think_off,
        });
    }
}

#[async_trait]
impl Member for Lane {
    fn id(&self) -> &MemberId {
        &self.member_id
    }

    fn route_id(&self) -> &str {
        "prov"
    }

    fn protocol(&self) -> &str {
        "plain"
    }

    async fn execute(
        &self,
        request: ModelRequest,
        _inbound: &str,
    ) -> Result<Completion, PluginError> {
        self.note(&request);
        let mut message = Message::new(Role::Assistant);
        message.content.push(hofund_ir::ContentPart::text("ok"));
        Ok(Completion::Unary(Box::new(Response::new(message))))
    }

    async fn count_input(
        &self,
        request: ModelRequest,
        _inbound: &str,
    ) -> Result<CountOutcome, PluginError> {
        self.note(&request);
        Ok(CountOutcome::Counted(4))
    }
}

struct Harness {
    app: axum::Router,
    usage: Arc<Ledger>,
    book: Arc<RouteTraceBook>,
    lead: Arc<Lane>,
    sub: Arc<Lane>,
    calls: Arc<AtomicUsize>,
    _dir: tempfile::TempDir,
}

fn harness(choice: Option<SubagentModelChoice>, disabled: &[&str]) -> Harness {
    let dir = tempfile::tempdir().expect("temp dir");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let lead = Lane::new("lead");
    let sub = Lane::new("sub");
    let routes = BTreeMap::from([
        (
            "lead".to_owned(),
            vec![Arc::clone(&lead) as Arc<dyn Member>],
        ),
        ("sub".to_owned(), vec![Arc::clone(&sub) as Arc<dyn Member>]),
    ]);
    let codecs: HashMap<String, Arc<dyn Codec>> =
        HashMap::from([("/v1/messages".to_owned(), Arc::new(Plain) as Arc<dyn Codec>)]);
    let calls = Arc::new(AtomicUsize::new(0));
    let hook_calls = Arc::clone(&calls);
    let hook: SubagentModelHook = Arc::new(move |query| {
        hook_calls.fetch_add(1, Ordering::SeqCst);
        let choice = choice.clone()?;
        query
            .body
            .windows(MARKER.len())
            .any(|window| window == MARKER)
            .then_some(choice)
    });
    let book = RouteTraceBook::new();
    let mut gateway = Gateway::new(
        Snapshot::new(Router::new(routes, FailoverPolicy::default())),
        "secret".to_owned(),
        codecs,
        Arc::clone(&usage),
    )
    .with_route_trace(Arc::clone(&book))
    .with_subagent_model(hook);
    if !disabled.is_empty() {
        let settings = CatalogSettings {
            disabled: disabled.iter().copied().map(str::to_owned).collect(),
            ..CatalogSettings::default()
        };
        let catalog = ModelCatalog::open(dir.path(), settings, Arc::new(Offline)).expect("catalog");
        gateway = gateway.with_model_catalog(catalog);
    }
    Harness {
        app: gateway.into_router(),
        usage,
        book,
        lead,
        sub,
        calls,
        _dir: dir,
    }
}

fn payload(model: &str, marker: bool, effort: Option<&str>) -> String {
    let mut value = json!({
        "model": model,
        "max_tokens": 8,
        "messages": [{
            "role": "user",
            "content": if marker { "INHERIT-TURN" } else { "hello" }
        }]
    });
    if let Some(effort) = effort {
        value["output_config"] = json!({ "effort": effort });
    }
    value.to_string()
}

async fn send(
    app: axum::Router,
    uri: &str,
    body: &str,
    header: bool,
    identity: Option<Identity>,
) -> (StatusCode, String) {
    let mut request = Request::builder()
        .method("POST")
        .uri(uri)
        .header("authorization", "Bearer secret")
        .header("content-type", "application/json");
    if header {
        request = request.header(SUBAGENT_HEADER, "worker");
    }
    let mut request = request.body(Body::from(body.to_owned())).expect("request");
    if let Some(identity) = identity {
        request.extensions_mut().insert(identity);
    }
    let response = app.oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn choice(model: &str, effort: Option<&str>) -> SubagentModelChoice {
    SubagentModelChoice {
        model: model.to_owned(),
        effort_label: effort.map(str::to_owned),
    }
}

fn only(lane: &Lane) -> Seen {
    let seen = lane.seen.lock().expect("seen");
    assert_eq!(seen.len(), 1);
    Seen {
        model: seen[0].model.clone(),
        effort: seen[0].effort,
        output_effort: seen[0].output_effort.clone(),
        thinking_disabled: seen[0].thinking_disabled,
        bridge_effort: seen[0].bridge_effort.clone(),
        think_off: seen[0].think_off,
    }
}

#[tokio::test]
async fn d267_inheriting_turn_uses_the_set_model() {
    let harness = harness(Some(choice("sub", None)), &[]);
    let (status, body) = send(
        harness.app,
        "/v1/messages",
        &payload("lead", true, None),
        true,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(harness.lead.seen.lock().expect("lead").is_empty());
    assert_eq!(only(&harness.sub).model, "sub");
    let rows = harness.usage.rows().expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].model, "lead");
    let decision = &harness.book.recent()[0];
    assert_eq!(decision.asked, "lead");
    assert_eq!(decision.tries[0].model, "sub");
    let note = decision.subagent.clone().expect("note");
    assert_eq!(note.asked, "lead");
    assert_eq!(note.to, "sub");
    assert!(note.kept.is_empty());
    assert_eq!(harness.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn d267_effort_replaces_the_body_effort() {
    let harness = harness(Some(choice("sub", Some("high"))), &[]);
    let (status, body) = send(
        harness.app,
        "/v1/messages",
        &payload("lead", true, Some("low")),
        true,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let seen = only(&harness.sub);
    assert_eq!(seen.effort, Some(Effort::High));
    assert_eq!(seen.output_effort.as_deref(), Some("high"));
    assert_eq!(seen.bridge_effort.as_deref(), Some("high"));
    assert_eq!(seen.think_off, Some(false));
    assert!(!seen.thinking_disabled);
}

#[tokio::test]
async fn d267_model_without_effort_clears_the_lead_effort() {
    let harness = harness(Some(choice("sub", None)), &[]);
    let (status, body) = send(
        harness.app,
        "/v1/messages",
        &payload("lead", true, Some("high")),
        true,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let seen = only(&harness.sub);
    assert_eq!(seen.effort, None);
    assert_eq!(seen.output_effort, None);
    assert_eq!(seen.bridge_effort.as_deref(), Some(""));
    assert_eq!(seen.think_off, Some(false));
}

#[tokio::test]
async fn d267_none_effort_disables_thinking() {
    let harness = harness(Some(choice("sub", Some("none"))), &[]);
    let (status, body) = send(
        harness.app,
        "/v1/messages",
        &payload("lead", true, Some("high")),
        true,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let seen = only(&harness.sub);
    assert_eq!(seen.effort, Some(Effort::None));
    assert!(seen.output_effort.is_none());
    assert!(seen.thinking_disabled);
    assert_eq!(seen.bridge_effort.as_deref(), Some(""));
    assert_eq!(seen.think_off, Some(true));
}

#[tokio::test]
async fn d267_other_turns_and_the_same_model_stay() {
    let plain = harness(Some(choice("sub", None)), &[]);
    let (status, body) = send(
        plain.app,
        "/v1/messages",
        &payload("lead", false, None),
        true,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(only(&plain.lead).model, "lead");
    assert!(plain.sub.seen.lock().expect("sub").is_empty());
    assert!(plain.book.recent()[0].subagent.is_none());
    assert_eq!(plain.calls.load(Ordering::SeqCst), 1);

    let lead = harness(Some(choice("sub", None)), &[]);
    let (status, body) = send(
        lead.app,
        "/v1/messages",
        &payload("lead", true, None),
        false,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(only(&lead.lead).model, "lead");
    assert!(lead.book.recent()[0].subagent.is_none());
    assert_eq!(lead.calls.load(Ordering::SeqCst), 0);

    let same = harness(Some(choice("lead", None)), &[]);
    let (status, body) = send(
        same.app,
        "/v1/messages",
        &payload("lead", true, None),
        true,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(only(&same.lead).model, "lead");
    assert!(same.book.recent()[0].subagent.is_none());
}

#[tokio::test]
async fn d267_unserved_disabled_and_key_bar_keep_the_asked_model() {
    let unserved = harness(Some(choice("missing", None)), &[]);
    let (status, body) = send(
        unserved.app,
        "/v1/messages",
        &payload("lead", true, None),
        true,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(only(&unserved.lead).model, "lead");
    let note = unserved.book.recent()[0]
        .subagent
        .clone()
        .expect("unserved");
    assert_eq!(note.to, "missing");
    assert_eq!(note.kept, "the gateway does not serve it now");
    assert_eq!(unserved.book.recent()[0].asked, "lead");

    let disabled = harness(Some(choice("sub", None)), &["sub"]);
    let (status, body) = send(
        disabled.app,
        "/v1/messages",
        &payload("lead", true, None),
        true,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(only(&disabled.lead).model, "lead");
    let note = disabled.book.recent()[0]
        .subagent
        .clone()
        .expect("disabled");
    assert_eq!(note.kept, "it is disabled");

    let barred = harness(Some(choice("sub", None)), &[]);
    let (status, body) = send(
        barred.app,
        "/v1/messages",
        &payload("lead", true, None),
        true,
        Some(Identity {
            models: vec!["prov/lead".to_owned()],
            ..Identity::default()
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(only(&barred.lead).model, "lead");
    let note = barred.book.recent()[0].subagent.clone().expect("barred");
    assert_eq!(note.kept, "the gateway key may not use it");
    assert_eq!(barred.usage.rows().expect("rows")[0].model, "lead");
}

#[tokio::test]
async fn d267_count_tokens_uses_the_pick() {
    let harness = harness(Some(choice("sub", Some("high"))), &[]);
    let (status, body) = send(
        harness.app,
        "/v1/messages/count_tokens",
        &payload("lead", true, Some("low")),
        true,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(harness.lead.seen.lock().expect("lead").is_empty());
    let seen = only(&harness.sub);
    assert_eq!(seen.model, "sub");
    assert_eq!(seen.effort, Some(Effort::High));
    assert_eq!(seen.bridge_effort.as_deref(), Some("high"));
}

#[tokio::test]
async fn d267_account_only_key_bar_keeps_the_asked_model() {
    let harness = harness(Some(choice("sub", None)), &[]);
    let (status, body) = send(
        harness.app,
        "/v1/messages",
        &payload("lead", true, None),
        true,
        Some(Identity {
            accounts: vec!["prov/preset:prov:lead".to_owned()],
            ..Identity::default()
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(only(&harness.lead).model, "lead");
    assert!(harness.sub.seen.lock().expect("sub").is_empty());
    let note = harness.book.recent()[0]
        .subagent
        .clone()
        .expect("account bar");
    assert_eq!(note.kept, "the gateway key may not use it");
}

#[tokio::test]
async fn d267_qualified_target_resolves_and_follows_key_scope() {
    let harness = harness(Some(choice("prov/sub", None)), &[]);
    let (status, body) = send(
        harness.app,
        "/v1/messages",
        &payload("lead", true, None),
        true,
        Some(Identity {
            models: vec!["prov/sub".to_owned()],
            ..Identity::default()
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(harness.lead.seen.lock().expect("lead").is_empty());
    assert_eq!(only(&harness.sub).model, "sub");
    let decision = &harness.book.recent()[0];
    assert_eq!(decision.tries[0].model, "sub");
    let note = decision.subagent.clone().expect("qualified target");
    assert_eq!(note.to, "prov/sub");
    assert!(note.kept.is_empty());
}
