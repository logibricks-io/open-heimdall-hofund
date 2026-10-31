use std::collections::{BTreeMap, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt, stream};
use hofund_codec::{Codec, OpenAiResponses};
use hofund_ir::{ContentPart, Request as ModelRequest, Role};
use hofund_plugin_api::{BodyStream, MemberId, MemberKey, PluginError, PresetId, ProtocolId};
use hofund_router::{Completion, FailoverPolicy, Member, Router as ModelRouter, Snapshot};
use hofund_usage::Ledger;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{self, protocol::CloseFrame as ClientCloseFrame};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::subscriber::{Interest, with_default};
use tracing::{Event, Metadata, Subscriber};

use crate::{Gateway, RequestEnd, RequestEvents, RequestStart};

use super::*;

#[derive(Clone, Default)]
struct Reply {
    fail: Option<u16>,
    chunks: Vec<Vec<u8>>,
    delay: Duration,
}

type Calls = Arc<Mutex<Vec<ModelRequest>>>;
type ReplyQueue = Arc<Mutex<VecDeque<Reply>>>;

impl Reply {
    fn failure(status: u16) -> Self {
        Self {
            fail: Some(status),
            ..Self::default()
        }
    }

    fn chunks(chunks: Vec<Vec<u8>>) -> Self {
        Self {
            chunks,
            ..Self::default()
        }
    }

    fn delayed(self, delay: Duration) -> Self {
        Self { delay, ..self }
    }

    fn stream(id: &str, answer: &str) -> Self {
        let item = json!({
            "type": "message",
            "id": format!("msg_{id}"),
            "role": "assistant",
            "status": "completed",
            "content": [{"type": "output_text", "text": answer}],
        });
        Self::chunks(vec![
            sse(&[
                json!({"type": "response.created", "response": {
                    "id": id, "object": "response", "status": "in_progress", "output": [],
                }}),
                json!({"type": "response.output_item.done", "output_index": 0, "item": item}),
                json!({"type": "response.completed", "response": {
                    "id": id,
                    "object": "response",
                    "status": "completed",
                    "output": [item],
                    "usage": {"input_tokens": 11, "output_tokens": 7, "total_tokens": 18},
                }}),
            ])
            .into_bytes(),
        ])
    }
}

fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect()
}

struct ActiveGuard {
    active: Arc<AtomicUsize>,
}

impl ActiveGuard {
    fn new(active: Arc<AtomicUsize>, maximum: Arc<AtomicUsize>) -> Self {
        let now = active.fetch_add(1, Ordering::SeqCst) + 1;
        maximum.fetch_max(now, Ordering::SeqCst);
        Self { active }
    }
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
    }
}

struct ReplyStream {
    chunks: VecDeque<Vec<u8>>,
    delay: Duration,
    _guard: Option<ActiveGuard>,
}

struct Fake {
    member_id: MemberId,
    route_name: &'static str,
    calls: Arc<Mutex<Vec<ModelRequest>>>,
    replies: Arc<Mutex<VecDeque<Reply>>>,
    active: Arc<AtomicUsize>,
    maximum: Arc<AtomicUsize>,
}

impl Fake {
    fn new(id: &'static str) -> (Arc<Self>, Calls, ReplyQueue) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let replies = Arc::new(Mutex::new(VecDeque::new()));
        let fake = Arc::new(Self {
            member_id: member_id(id),
            route_name: id,
            calls: Arc::clone(&calls),
            replies: Arc::clone(&replies),
            active: Arc::new(AtomicUsize::new(0)),
            maximum: Arc::new(AtomicUsize::new(0)),
        });
        (fake, calls, replies)
    }

    fn member(self: &Arc<Self>) -> Arc<dyn Member> {
        Arc::clone(self) as Arc<dyn Member>
    }
}

#[async_trait]
impl Member for Fake {
    fn id(&self) -> &MemberId {
        &self.member_id
    }

    fn route_id(&self) -> &str {
        self.route_name
    }

    fn protocol(&self) -> &str {
        ProtocolId::OPENAI_RESPONSES.as_str()
    }

    async fn execute(
        &self,
        request: ModelRequest,
        _inbound_protocol: &str,
    ) -> Result<Completion, PluginError> {
        let number = {
            let mut calls = self.calls.lock().expect("calls");
            calls.push(request);
            calls.len()
        };
        let reply = self
            .replies
            .lock()
            .expect("replies")
            .pop_front()
            .unwrap_or_else(|| {
                Reply::stream(&format!("resp_up{number}"), &format!("answer {number}"))
            });
        if let Some(status) = reply.fail {
            return Err(PluginError::status_error(
                status,
                "upstream refused the turn",
            ));
        }
        let guard = (!reply.delay.is_zero())
            .then(|| ActiveGuard::new(Arc::clone(&self.active), Arc::clone(&self.maximum)));
        let state = ReplyStream {
            chunks: reply.chunks.into(),
            delay: reply.delay,
            _guard: guard,
        };
        Ok(Completion::Raw(BodyStream::from_stream(stream::unfold(
            state,
            |mut state| async move {
                if !state.delay.is_zero() {
                    tokio::time::sleep(state.delay).await;
                    state.delay = Duration::ZERO;
                }
                let chunk = state.chunks.pop_front()?;
                Some((Ok(Bytes::from(chunk)), state))
            },
        ))))
    }
}

fn member_id(id: &'static str) -> MemberId {
    MemberId::from_preset(
        PresetId::try_from(id).expect("preset"),
        MemberKey::from_static("row"),
    )
}

fn insert_named(headers: &mut HeaderMap, name: String, value: &'static str) {
    let name = axum::http::HeaderName::from_bytes(name.as_bytes()).expect("header name");
    headers.insert(name, HeaderValue::from_static(value));
}

#[derive(Default)]
struct EventSpy {
    starts: Mutex<Vec<RequestStart>>,
    ends: Mutex<Vec<RequestEnd>>,
}

impl RequestEvents for EventSpy {
    fn started(&self, start: &RequestStart) {
        self.starts.lock().expect("starts").push(start.clone());
    }

    fn finished(&self, end: &RequestEnd) {
        self.ends.lock().expect("ends").push(end.clone());
    }
}

fn gateway_app(
    members: Vec<Arc<dyn Member>>,
    token_file: Option<PathBuf>,
    usage: Arc<Ledger>,
    events: Option<Arc<EventSpy>>,
) -> Router {
    let mut routes = BTreeMap::new();
    routes.insert("demo".to_owned(), members);
    let snapshot = Snapshot::new(ModelRouter::new(routes, FailoverPolicy::default()));
    let mut codecs = std::collections::HashMap::new();
    codecs.insert(
        "/v1/responses".to_owned(),
        Arc::new(OpenAiResponses) as Arc<dyn Codec>,
    );
    let gateway = match token_file {
        Some(path) => Gateway::with_token_file(snapshot, "first".to_owned(), path, codecs, usage),
        None => Gateway::new(snapshot, "secret".to_owned(), codecs, usage),
    };
    match events {
        Some(events) => gateway
            .with_request_events(events, Arc::new(AtomicU64::new(0)))
            .into_router(),
        None => gateway.into_router(),
    }
}

struct TestServer {
    addr: SocketAddr,
    task: JoinHandle<()>,
}

impl TestServer {
    async fn start(app: Router) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("address");
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self { addr, task }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Client {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl Client {
    async fn connect(server: &TestServer, path: &str) -> Self {
        Self::connect_with(server, path, &[("authorization", "Bearer secret")]).await
    }

    async fn connect_with(server: &TestServer, path: &str, headers: &[(&str, &str)]) -> Self {
        let mut request = format!("ws://{}{path}", server.addr)
            .into_client_request()
            .expect("client request");
        for (name, value) in headers {
            request.headers_mut().insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).expect("client header name"),
                value.parse().expect("client header"),
            );
        }
        let (socket, response) = connect_async(request).await.expect("connect");
        assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
        Self { socket }
    }

    async fn send(&mut self, value: &Value) {
        self.send_text(&value.to_string()).await;
    }

    async fn send_text(&mut self, text: &str) {
        self.socket
            .send(tungstenite::Message::Text(text.to_owned().into()))
            .await
            .expect("send text");
    }

    async fn send_binary(&mut self, bytes: &[u8]) {
        self.socket
            .send(tungstenite::Message::Binary(Bytes::copy_from_slice(bytes)))
            .await
            .expect("send binary");
    }

    async fn json(&mut self) -> Value {
        loop {
            let message = self.message().await;
            match message {
                Some(tungstenite::Message::Text(text)) => {
                    return serde_json::from_str(text.as_str()).expect("JSON frame");
                }
                Some(tungstenite::Message::Ping(_) | tungstenite::Message::Pong(_)) => {}
                other => panic!("expected a text frame, got {other:?}"),
            }
        }
    }

    async fn until_terminal(&mut self) -> Vec<Value> {
        let mut frames = Vec::new();
        loop {
            let frame = self.json().await;
            let terminal = matches!(
                frame.get("type").and_then(Value::as_str),
                Some(
                    "error"
                        | "response.completed"
                        | "response.incomplete"
                        | "response.failed"
                        | "response.cancelled"
                )
            );
            frames.push(frame);
            if terminal {
                return frames;
            }
        }
    }

    async fn turn(&mut self, value: &Value) -> Vec<Value> {
        self.send(value).await;
        self.until_terminal().await
    }

    async fn close(&mut self) -> Option<ClientCloseFrame> {
        loop {
            match self.message().await {
                Some(tungstenite::Message::Close(frame)) => return frame,
                Some(tungstenite::Message::Ping(_) | tungstenite::Message::Pong(_)) => {}
                Some(_) => {}
                None => return None,
            }
        }
    }

    async fn message(&mut self) -> Option<tungstenite::Message> {
        tokio::time::timeout(Duration::from_secs(10), self.socket.next())
            .await
            .expect("socket read timed out")
            .and_then(|message| message.ok())
    }
}

fn create(previous: Option<&str>, input: Value) -> Value {
    let mut value = json!({
        "type": "response.create",
        "model": "demo",
        "stream": true,
        "store": false,
        "input": input,
    });
    if let Some(previous) = previous {
        value["previous_response_id"] = Value::String(previous.to_owned());
    }
    value
}

fn user(text: &str) -> Value {
    json!({
        "type": "message",
        "role": "user",
        "content": [{"type": "input_text", "text": text}],
    })
}

fn terminal_id(frames: &[Value]) -> &str {
    frames
        .last()
        .and_then(|frame| frame.pointer("/response/id"))
        .and_then(Value::as_str)
        .expect("response id")
}

fn message_texts(request: &ModelRequest) -> Vec<(Role, String)> {
    request
        .messages
        .iter()
        .map(|message| {
            let text = message
                .content
                .iter()
                .map(|part| match part {
                    ContentPart::Text(text) => text.text.clone(),
                    _ => String::new(),
                })
                .collect::<Vec<_>>()
                .join("");
            (message.role, text)
        })
        .collect()
}

#[tokio::test]
async fn b1_routes_accept_sockets_plain_get_and_other_paths() {
    let (fake, _calls, _replies) = Fake::new("primary");
    let dir = tempfile::tempdir().expect("temp dir");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let app = gateway_app(vec![fake.member()], None, usage, None);
    for path in ["/v1/responses", "/responses"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(path)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("plain GET");
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(
            response.headers().get(header::ALLOW).expect("allow"),
            "POST"
        );
    }
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/models")
                .header(header::UPGRADE, "websocket")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("other path");
    assert_eq!(response.status(), StatusCode::UPGRADE_REQUIRED);

    let server = TestServer::start(app).await;
    for path in ["/v1/responses", "/responses"] {
        let client = Client::connect(&server, path).await;
        drop(client);
    }
}

#[tokio::test]
async fn b1_a_foreign_origin_cannot_open_a_socket() {
    let (fake, calls, _replies) = Fake::new("primary");
    let dir = tempfile::tempdir().expect("temp dir");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let server = TestServer::start(gateway_app(vec![fake.member()], None, usage, None)).await;
    let own = format!("http://{}", server.addr);
    let same = Client::connect_with(
        &server,
        "/v1/responses",
        &[("authorization", "Bearer secret"), ("origin", own.as_str())],
    )
    .await;
    drop(same);
    let port = server.addr.port();
    let subdomain = format!("http://evil.localhost:{port}");
    for origin in ["https://evil.example", "null", subdomain.as_str()] {
        let mut request = format!("ws://{}/v1/responses", server.addr)
            .into_client_request()
            .expect("client request");
        request
            .headers_mut()
            .insert("authorization", "Bearer secret".parse().expect("auth"));
        request
            .headers_mut()
            .insert("origin", origin.parse().expect("origin"));
        match connect_async(request).await {
            Err(tungstenite::Error::Http(response)) => {
                assert_eq!(response.status(), StatusCode::FORBIDDEN, "{origin}");
            }
            other => panic!("{origin}: expected 403, got {other:?}"),
        }
    }
    assert!(calls.lock().expect("calls").is_empty());
}

#[tokio::test]
async fn b2_replay_rechecks_auth_and_runs_the_whole_request_path() {
    let request = Request::builder()
        .method("GET")
        .uri("/v1/responses?key=first")
        .header("authorization", "Bearer first")
        .header("host", "local.test")
        .header("x-kept", "yes")
        .header("x-nominated", "drop")
        .header("connection", "Upgrade, x-nominated")
        .header("upgrade", "websocket")
        .header("sec-websocket-key", "drop")
        .header("content-type", "text/plain")
        .header("content-encoding", "gzip")
        .header("accept", "application/json")
        .header("accept-encoding", "gzip")
        .header(wire::BETA_HEADER, wire::SOCKET_BETA)
        .header("content-length", "99")
        .body(Body::empty())
        .expect("origin");
    let (parts, _) = request.into_parts();
    let replay = replay_request(&parts, br#"{"model":"demo","input":[]}"#);
    assert_eq!(replay.method(), Method::POST);
    assert_eq!(replay.uri(), "/v1/responses?key=first");
    assert_eq!(replay.headers().get("host").expect("host"), "local.test");
    assert_eq!(replay.headers().get("x-kept").expect("kept"), "yes");
    for name in [
        "x-nominated",
        "connection",
        "upgrade",
        "sec-websocket-key",
        "content-encoding",
        "accept-encoding",
        wire::BETA_HEADER,
    ] {
        assert!(!replay.headers().contains_key(name), "{name}");
    }
    assert_eq!(
        replay.headers().get(header::CONTENT_TYPE).expect("type"),
        "application/json"
    );
    assert_eq!(
        replay.headers().get(header::ACCEPT).expect("accept"),
        "text/event-stream"
    );

    let (primary, primary_calls, primary_replies) = Fake::new("primary");
    let (backup, backup_calls, _backup_replies) = Fake::new("backup");
    primary_replies
        .lock()
        .expect("reply")
        .push_back(Reply::failure(503));
    let dir = tempfile::tempdir().expect("temp dir");
    let token = dir.path().join("token");
    std::fs::write(&token, "first\n").expect("token");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let events = Arc::new(EventSpy::default());
    let app = gateway_app(
        vec![primary.member(), backup.member()],
        Some(token.clone()),
        Arc::clone(&usage),
        Some(Arc::clone(&events)),
    );
    let server = TestServer::start(app).await;
    let mut client = Client::connect_with(
        &server,
        "/v1/responses",
        &[("authorization", "Bearer first")],
    )
    .await;
    let first = client.turn(&create(None, json!([user("hi")]))).await;
    assert_eq!(
        first.last().expect("terminal")["type"],
        "response.completed"
    );
    std::fs::write(&token, "second\n").expect("rotate");
    let refused = client
        .turn(&create(Some(terminal_id(&first)), json!([user("again")])))
        .await;
    assert_eq!(refused.last().expect("error")["type"], "error");
    assert_eq!(refused.last().expect("error")["status"], 401);
    assert_eq!(primary_calls.lock().expect("calls").len(), 1);
    assert_eq!(backup_calls.lock().expect("calls").len(), 1);
    assert_eq!(usage.rows().expect("rows").len(), 1);
    assert_eq!(usage.rows().expect("rows")[0].upstream, "backup");
    assert_eq!(events.starts.lock().expect("starts").len(), 1);
    assert_eq!(events.ends.lock().expect("ends").len(), 1);
}

#[test]
fn b3_client_metadata_becomes_turn_headers() {
    let request = Request::builder()
        .method("GET")
        .uri("/v1/responses")
        .body(Body::empty())
        .expect("origin");
    let (parts, _) = request.into_parts();
    let body = json!({
        "model": "demo",
        "input": [],
        "client_metadata": {
            wire::TURN_STATE: "turn-state",
            wire::LITE_METADATA: "TRUE",
        },
    });
    let replay = replay_request(&parts, &serde_json::to_vec(&body).expect("body"));
    assert_eq!(
        replay.headers().get(wire::TURN_STATE).expect("turn state"),
        "turn-state"
    );
    assert_eq!(
        replay.headers().get(wire::LITE_HEADER).expect("lite"),
        "true"
    );
}

#[tokio::test]
async fn b4_create_body_drops_socket_fields_and_expands_input() {
    let (fake, calls, _replies) = Fake::new("primary");
    let dir = tempfile::tempdir().expect("temp dir");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let server = TestServer::start(gateway_app(vec![fake.member()], None, usage, None)).await;
    let mut client = Client::connect(&server, "/v1/responses").await;
    let first = client.turn(&create(None, json!([user("first")]))).await;
    let mut second = create(Some(terminal_id(&first)), json!([user("second")]));
    second["generate"] = Value::Bool(true);
    second["instructions"] = Value::String("be brief".to_owned());
    let completed = client.turn(&second).await;
    assert_eq!(
        completed.last().expect("terminal")["type"],
        "response.completed"
    );
    let calls = calls.lock().expect("calls");
    assert_eq!(calls.len(), 2);
    assert_eq!(
        message_texts(&calls[1])[1..],
        vec![
            (Role::User, "first".to_owned()),
            (Role::Assistant, "answer 1".to_owned()),
            (Role::User, "second".to_owned()),
        ]
    );
    assert_eq!(
        message_texts(&calls[1])[0],
        (Role::System, "be brief".to_owned())
    );
    for name in ["type", "previous_response_id", "generate"] {
        assert!(calls[1].extensions.get(name).is_none(), "{name}");
    }
}

#[tokio::test]
async fn b5_history_is_socket_local_unknown_and_limited_to_two() {
    let (fake, calls, _replies) = Fake::new("primary");
    let dir = tempfile::tempdir().expect("temp dir");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let server = TestServer::start(gateway_app(vec![fake.member()], None, usage, None)).await;
    let mut first = Client::connect(&server, "/v1/responses").await;
    let one = first.turn(&create(None, json!([user("one")]))).await;
    let one_id = terminal_id(&one).to_owned();
    let mut other = Client::connect(&server, "/v1/responses").await;
    let refused = other
        .turn(&create(Some(&one_id), json!([user("other")])))
        .await;
    assert_eq!(
        refused.last().expect("error")["error"]["code"],
        "previous_response_not_found"
    );
    assert_eq!(
        refused.last().expect("error")["error"]["param"],
        "previous_response_id"
    );
    let two = first
        .turn(&create(Some(&one_id), json!([user("two")])))
        .await;
    let two_id = terminal_id(&two).to_owned();
    let three = first
        .turn(&create(Some(&two_id), json!([user("three")])))
        .await;
    let three_id = terminal_id(&three).to_owned();
    let evicted = first
        .turn(&create(Some(&one_id), json!([user("evicted")])))
        .await;
    assert_eq!(
        evicted.last().expect("error")["error"]["code"],
        "previous_response_not_found"
    );
    let unknown = first
        .turn(&create(Some("resp_missing"), json!([user("missing")])))
        .await;
    assert_eq!(unknown.last().expect("error")["status"], 400);
    assert_eq!(
        unknown.last().expect("error")["error"]["code"],
        "previous_response_not_found"
    );
    assert_eq!(calls.lock().expect("calls").len(), 3);
    let continued = first
        .turn(&create(Some(&three_id), json!([user("four")])))
        .await;
    assert_eq!(
        continued.last().expect("terminal")["type"],
        "response.completed"
    );
}

#[tokio::test]
async fn b6_warmup_is_local_and_holds_its_input() {
    let (fake, calls, _replies) = Fake::new("primary");
    let dir = tempfile::tempdir().expect("temp dir");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let server = TestServer::start(gateway_app(vec![fake.member()], None, usage, None)).await;
    let mut client = Client::connect(&server, "/v1/responses").await;
    let mut warmup = create(None, json!([user("held")]));
    warmup["generate"] = Value::Bool(false);
    let frames = client.turn(&warmup).await;
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0]["type"], "response.created");
    assert_eq!(frames[0]["response"]["status"], "in_progress");
    assert_eq!(frames[0]["response"]["model"], "demo");
    assert_eq!(frames[1]["response"]["status"], "completed");
    assert_eq!(frames[1]["response"]["usage"]["total_tokens"], 0);
    assert!(terminal_id(&frames).starts_with("resp_"));
    assert!(calls.lock().expect("calls").is_empty());
    let next = client
        .turn(&create(Some(terminal_id(&frames)), json!([user("new")])))
        .await;
    assert_eq!(next.last().expect("terminal")["type"], "response.completed");
    let calls = calls.lock().expect("calls");
    assert_eq!(calls.len(), 1);
    assert_eq!(message_texts(&calls[0]).len(), 2);
}

#[tokio::test]
async fn b7_streams_unary_replies_and_bad_success_bodies_map_to_frames() {
    let (fake, _calls, replies) = Fake::new("primary");
    let stream = concat!(
        ": comment\n\n",
        "data:\n\n",
        "data: [DONE]\n\n",
        "data: not-json\n\n",
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_stream\"}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_stream\",\"status\":\"completed\",\"output\":[]}}\n\n",
    );
    replies.lock().expect("replies").extend([
        Reply::chunks(vec![
            stream.as_bytes()[..17].to_vec(),
            stream.as_bytes()[17..].to_vec(),
        ]),
        Reply::chunks(vec![
            serde_json::to_vec(&json!({
                "id": "resp_unary",
                "object": "response",
                "status": "incomplete",
                "output": [
                    {"type": "message", "role": "assistant", "content": []},
                    {"type": "message", "role": "assistant", "content": []},
                ],
            }))
            .expect("unary"),
        ]),
        Reply::chunks(vec![b"not a response".to_vec()]),
    ]);
    let dir = tempfile::tempdir().expect("temp dir");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let server = TestServer::start(gateway_app(vec![fake.member()], None, usage, None)).await;
    let mut client = Client::connect(&server, "/v1/responses").await;
    let streamed = client.turn(&create(None, json!([user("stream")]))).await;
    assert_eq!(streamed.len(), 2);
    assert_eq!(streamed[0]["type"], "response.created");
    assert_eq!(streamed[1]["type"], "response.completed");
    let mut unary_request = create(None, json!([user("unary")]));
    unary_request["stream"] = Value::Bool(false);
    let unary = client.turn(&unary_request).await;
    assert_eq!(unary.len(), 4);
    assert_eq!(unary[0]["type"], "response.created");
    assert_eq!(unary[1]["type"], "response.output_item.done");
    assert_eq!(unary[2]["output_index"], 1);
    assert_eq!(unary[3]["type"], "response.incomplete");
    unary_request["input"] = json!([user("invalid")]);
    let invalid = client.turn(&unary_request).await;
    assert_eq!(invalid.last().expect("error")["type"], "error");
    assert_eq!(invalid.last().expect("error")["status"], 502);
    assert_eq!(
        invalid.last().expect("error")["error"]["type"],
        "server_error"
    );
}

#[test]
fn b8_response_headers_become_metadata_and_rate_limit_frames() {
    let mut headers = HeaderMap::new();
    headers.insert(wire::TURN_STATE, HeaderValue::from_static("state"));
    headers.insert(wire::MODEL_HEADER, HeaderValue::from_static("model"));
    headers.insert(wire::REASONING_HEADER, HeaderValue::from_static("yes"));
    headers.insert(
        wire::MODELS_ETAG_HEADER,
        HeaderValue::from_static("W/\"gateway\""),
    );
    insert_named(
        &mut headers,
        format!("{}primary-used-percent", wire::CLIENT_HEADER_PREFIX),
        "12.5",
    );
    insert_named(
        &mut headers,
        format!("{}primary-window-minutes", wire::CLIENT_HEADER_PREFIX),
        "300",
    );
    insert_named(
        &mut headers,
        format!("{}primary-reset-at", wire::CLIENT_HEADER_PREFIX),
        "1234",
    );
    insert_named(
        &mut headers,
        format!("{}secondary-used-percent", wire::CLIENT_HEADER_PREFIX),
        "25",
    );
    headers.insert(wire::PLAN_TYPE_HEADER, HeaderValue::from_static("plan"));
    headers.insert(wire::CREDITS_HAS_HEADER, HeaderValue::from_static("true"));
    headers.insert(
        wire::CREDITS_UNLIMITED_HEADER,
        HeaderValue::from_static("false"),
    );
    headers.insert(
        wire::CREDITS_BALANCE_HEADER,
        HeaderValue::from_static("9.5"),
    );
    let frames = header_frames(&headers);
    assert_eq!(frames.len(), 3);
    assert_eq!(frames[0]["type"], "response.metadata");
    assert_eq!(frames[0]["headers"][wire::TURN_STATE], "state");
    assert_eq!(frames[0]["headers"][wire::MODEL_HEADER], "model");
    assert_eq!(frames[0]["headers"][wire::REASONING_HEADER], "yes");
    assert_eq!(frames[1]["type"], wire::RESPONSE_METADATA_EVENT);
    assert_eq!(
        frames[1]["headers"][wire::MODELS_ETAG_HEADER],
        "W/\"gateway\""
    );
    assert_eq!(frames[2]["type"], wire::RATE_LIMITS_EVENT);
    assert_eq!(frames[2]["rate_limits"]["primary"]["used_percent"], 12.5);
    assert_eq!(frames[2]["rate_limits"]["primary"]["window_minutes"], 300);
    assert_eq!(frames[2]["rate_limits"]["primary"]["reset_at"], 1234);
    assert_eq!(frames[2]["rate_limits"]["secondary"]["used_percent"], 25.0);
    assert_eq!(frames[2]["plan_type"], "plan");
    assert_eq!(frames[2]["credits"]["has_credits"], true);
    assert_eq!(frames[2]["credits"]["unlimited"], false);
    assert_eq!(frames[2]["credits"]["balance"], "9.5");
    let event = with_gateway_etag(
        json!({
            "type": wire::RESPONSE_METADATA_EVENT,
            "headers": {wire::MODELS_ETAG_HEADER: "stale", "x-other": "kept"},
        }),
        &headers,
    );
    assert_eq!(event["headers"][wire::MODELS_ETAG_HEADER], "W/\"gateway\"");
    assert_eq!(event["headers"]["x-other"], "kept");
}

#[tokio::test]
async fn b9_http_errors_keep_shape_headers_and_bound_the_body() {
    let mut headers = HeaderMap::new();
    headers.insert("retry-after", HeaderValue::from_static("2"));
    headers.insert("retry-after-ms", HeaderValue::from_static("2000"));
    headers.insert("x-request-id", HeaderValue::from_static("request"));
    headers.insert("x-error-json", HeaderValue::from_static("detail"));
    headers.insert(wire::PLAN_TYPE_HEADER, HeaderValue::from_static("plan"));
    headers.insert("x-ratelimit-reset", HeaderValue::from_static("123"));
    headers.insert("content-type", HeaderValue::from_static("text/plain"));
    let frame = http_error_frame(
        StatusCode::TOO_MANY_REQUESTS,
        &headers,
        br#"{"error":{"type":"rate_limit_error","message":"wait"}}"#,
    );
    assert_eq!(frame["status"], 429);
    assert_eq!(frame["error"]["message"], "wait");
    assert_eq!(frame["headers"]["retry-after"], "2");
    assert_eq!(frame["headers"]["retry-after-ms"], "2000");
    assert_eq!(frame["headers"]["x-request-id"], "request");
    assert_eq!(frame["headers"]["x-error-json"], "detail");
    assert_eq!(frame["headers"][wire::PLAN_TYPE_HEADER], "plan");
    assert_eq!(frame["headers"]["x-ratelimit-reset"], "123");
    assert!(frame["headers"].get("content-type").is_none());
    let text = http_error_frame(StatusCode::BAD_GATEWAY, &HeaderMap::new(), b" plain text\n");
    assert_eq!(text["error"]["type"], "server_error");
    assert_eq!(text["error"]["message"], "plain text");
    for body in [b"".as_slice(), &[0xff, 0xfe]] {
        let fallback = http_error_frame(StatusCode::BAD_GATEWAY, &HeaderMap::new(), body);
        assert_eq!(fallback["error"]["type"], "server_error");
        assert_eq!(fallback["error"]["message"], "Bad Gateway");
    }
    let capped = read_body(
        Body::from(vec![b'x'; ERROR_BODY_LIMIT + 100]),
        Some(ERROR_BODY_LIMIT),
    )
    .await;
    assert_eq!(capped.len(), ERROR_BODY_LIMIT);
    let huge = Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .body(Body::from(vec![b'x'; ERROR_BODY_LIMIT + 100]))
        .expect("reply");
    let frame = http_error_reply(huge).await;
    assert_eq!(frame["status"], 500);
    assert_eq!(
        frame["error"]["message"].as_str().expect("message").len(),
        ERROR_BODY_LIMIT
    );

    let (fake, _calls, replies) = Fake::new("primary");
    replies
        .lock()
        .expect("replies")
        .push_back(Reply::failure(429));
    let dir = tempfile::tempdir().expect("temp dir");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let server = TestServer::start(gateway_app(vec![fake.member()], None, usage, None)).await;
    let mut client = Client::connect(&server, "/v1/responses").await;
    let refused = client.turn(&create(None, json!([user("limited")]))).await;
    assert_eq!(refused.last().expect("error")["status"], 429);
    assert_eq!(refused.last().expect("error")["type"], "error");
    let next = client.turn(&create(None, json!([user("next")]))).await;
    assert_eq!(next.last().expect("terminal")["type"], "response.completed");
}

#[tokio::test]
async fn b10_bad_frames_get_errors_and_binary_closes_unsupported() {
    let (fake, calls, _replies) = Fake::new("primary");
    let dir = tempfile::tempdir().expect("temp dir");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let server = TestServer::start(gateway_app(vec![fake.member()], None, usage, None)).await;
    let mut client = Client::connect(&server, "/v1/responses").await;
    client
        .send_text(r#"{"type": "response.create", "input": "#)
        .await;
    assert_eq!(client.json().await["error"]["code"], "invalid_json");
    for frame in ["{", "not json", "[1]"] {
        client.send_text(frame).await;
        let refused = client.json().await;
        assert_eq!(refused["error"]["code"], "unsupported_message", "{frame}");
        assert_eq!(refused["error"]["param"], "type", "{frame}");
    }
    client.send_text(r#"{"type":"response.interrupt"}"#).await;
    assert_eq!(client.json().await["error"]["code"], "unsupported_message");
    client.send(&create(None, Value::Null)).await;
    assert_eq!(client.json().await["error"]["code"], "invalid_value");
    assert!(calls.lock().expect("calls").is_empty());
    client.send_binary(b"binary").await;
    let close = client.close().await.expect("close");
    assert_eq!(
        close.code,
        tungstenite::protocol::frame::coding::CloseCode::Unsupported
    );
}

#[tokio::test]
async fn b11_cut_replies_close_and_only_completed_replies_are_held() {
    let (fake, _calls, replies) = Fake::new("primary");
    replies.lock().expect("replies").extend([
        Reply::chunks(vec![sse(&[json!({
            "type": "response.created",
            "response": {"id": "resp_cut", "object": "response", "status": "in_progress"},
        })])
        .into_bytes()]),
        Reply::chunks(vec![sse(&[json!({
            "type": "response.incomplete",
            "response": {"id": "resp_incomplete", "object": "response", "status": "incomplete", "output": []},
        })])
        .into_bytes()]),
    ]);
    let dir = tempfile::tempdir().expect("temp dir");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let server = TestServer::start(gateway_app(vec![fake.member()], None, usage, None)).await;
    let mut cut = Client::connect(&server, "/v1/responses").await;
    cut.send(&create(None, json!([user("cut")]))).await;
    assert_eq!(cut.json().await["type"], "response.created");
    let close = cut.close().await.expect("close");
    assert_eq!(
        close.code,
        tungstenite::protocol::frame::coding::CloseCode::Error
    );
    assert_eq!(close.reason, CUT_REASON);

    let mut client = Client::connect(&server, "/v1/responses").await;
    let unknown_cut = client
        .turn(&create(Some("resp_cut"), json!([user("unknown")])))
        .await;
    assert_eq!(
        unknown_cut.last().expect("error")["error"]["code"],
        "previous_response_not_found"
    );
    let incomplete = client
        .turn(&create(None, json!([user("incomplete")])))
        .await;
    assert_eq!(
        incomplete.last().expect("terminal")["type"],
        "response.incomplete"
    );
    let unknown_incomplete = client
        .turn(&create(Some("resp_incomplete"), json!([user("unknown")])))
        .await;
    assert_eq!(
        unknown_incomplete.last().expect("error")["error"]["code"],
        "previous_response_not_found"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b12_frames_queue_and_turns_are_strictly_sequential() {
    let (fake, calls, replies) = Fake::new("primary");
    replies.lock().expect("replies").extend([
        Reply::stream("resp_first", "first").delayed(Duration::from_millis(150)),
        Reply::stream("resp_second", "second"),
    ]);
    let dir = tempfile::tempdir().expect("temp dir");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let server = TestServer::start(gateway_app(vec![fake.member()], None, usage, None)).await;
    let mut client = Client::connect(&server, "/v1/responses").await;
    client.send(&create(None, json!([user("first")]))).await;
    client.send(&create(None, json!([user("second")]))).await;
    let first = client.until_terminal().await;
    let second = client.until_terminal().await;
    assert_eq!(terminal_id(&first), "resp_first");
    assert_eq!(terminal_id(&second), "resp_second");
    assert_eq!(fake.maximum.load(Ordering::SeqCst), 1);
    let calls = calls.lock().expect("calls");
    assert_eq!(calls.len(), 2);
    assert_eq!(message_texts(&calls[0])[0].1, "first");
    assert_eq!(message_texts(&calls[1])[0].1, "second");
}

#[tokio::test]
async fn b13_message_read_limit_matches_the_http_body_limit() {
    let (fake, calls, _replies) = Fake::new("primary");
    let dir = tempfile::tempdir().expect("temp dir");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let server = TestServer::start(gateway_app(vec![fake.member()], None, usage, None)).await;
    let mut client = Client::connect(&server, "/v1/responses").await;
    let mut oversized = create(None, json!([user("large")]));
    oversized["instructions"] = Value::String("x".repeat(BODY_LIMIT));
    client.send(&oversized).await;
    if let Some(close) = client.close().await {
        assert_eq!(
            close.code,
            tungstenite::protocol::frame::coding::CloseCode::Size
        );
    }
    assert!(calls.lock().expect("calls").is_empty());
}

#[derive(Default)]
struct LogFields(Mutex<BTreeMap<String, u64>>);

struct LogSubscriber(Arc<LogFields>);

impl Subscriber for LogSubscriber {
    fn register_callsite(&self, _metadata: &'static Metadata<'static>) -> Interest {
        Interest::always()
    }

    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _span: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _span: &Id, _values: &Record<'_>) {}

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, event: &Event<'_>) {
        struct Visitor<'a>(&'a LogFields);
        impl Visit for Visitor<'_> {
            fn record_u64(&mut self, field: &Field, value: u64) {
                self.0
                    .0
                    .lock()
                    .expect("log fields")
                    .insert(field.name().to_owned(), value);
            }

            fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {}
        }
        event.record(&mut Visitor(&self.0));
    }

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}
}

#[test]
fn b14_socket_end_logs_turns_continuations_and_byte_counts() {
    let fields = Arc::new(LogFields::default());
    let stats = SocketStats {
        turns: 5,
        continued: 4,
        bytes_received: 70_185,
        bytes_sent: 255_379,
    };
    with_default(LogSubscriber(Arc::clone(&fields)), || stats.log());
    let fields = fields.0.lock().expect("log fields").clone();
    assert_eq!(fields.get("turns"), Some(&5));
    assert_eq!(fields.get("continued"), Some(&4));
    assert_eq!(fields.get("bytes_received"), Some(&70_185));
    assert_eq!(fields.get("bytes_sent_as_whole_requests"), Some(&255_379));

    let quiet = Arc::new(LogFields::default());
    let idle = SocketStats {
        bytes_received: 9,
        ..SocketStats::default()
    };
    with_default(LogSubscriber(Arc::clone(&quiet)), || idle.log());
    assert!(quiet.0.lock().expect("log fields").is_empty());
}
