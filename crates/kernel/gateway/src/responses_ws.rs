use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::ws::{CloseFrame, Message, Utf8Bytes, WebSocket, close_code};
use axum::extract::{FromRequestParts, State, WebSocketUpgrade};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use hofund_plugin_api::responses_ws as wire;
use serde_json::{Map, Value, json};
use tokio::sync::{Mutex as AsyncMutex, watch};
use tower::ServiceExt;

use super::{
    BODY_LIMIT, Inner, current_token, next_request_id, reject, request_router, text_response,
};

const KEEP_TURNS: usize = 2;
const ERROR_BODY_LIMIT: usize = 1024 * 1024;
const RESPONSES_PATH: &str = "/v1/responses";
const RESPONSES_ALIAS: &str = "/responses";
const CUT_REASON: &str = "the reply ended before response.completed";
const RESTARTING: &str = "hofund is restarting";
const SOCKET_AWAY: u8 = 1;
const SOCKET_CUT: u8 = 2;

pub(super) struct SocketRegistry {
    state: Mutex<RegistryState>,
    count: watch::Sender<usize>,
}

#[derive(Default)]
struct RegistryState {
    closing: bool,
    next_id: u64,
    sessions: HashMap<u64, watch::Sender<u8>>,
}

impl Default for SocketRegistry {
    fn default() -> Self {
        Self {
            state: Mutex::new(RegistryState::default()),
            count: watch::channel(0).0,
        }
    }
}

impl SocketRegistry {
    pub(super) fn ending(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .closing
    }

    fn register(self: &Arc<Self>) -> Option<SocketRegistration> {
        let (lifecycle, receiver) = watch::channel(0);
        let id = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if state.closing {
                return None;
            }
            state.next_id += 1;
            let id = state.next_id;
            state.sessions.insert(id, lifecycle);
            self.count.send_replace(state.sessions.len());
            id
        };
        Some(SocketRegistration {
            id,
            registry: Arc::clone(self),
            lifecycle: receiver,
        })
    }

    pub(super) fn drain(&self) {
        let sessions = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.closing = true;
            state
                .sessions
                .values()
                .cloned()
                .collect::<Vec<watch::Sender<u8>>>()
        };
        for session in sessions {
            session.send_replace(SOCKET_AWAY);
        }
    }

    pub(super) fn cut(&self) {
        let sessions = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .sessions
            .values()
            .cloned()
            .collect::<Vec<watch::Sender<u8>>>();
        for session in sessions {
            session.send_replace(SOCKET_CUT);
        }
    }

    pub(super) async fn wait(&self) {
        let mut count = self.count.subscribe();
        let _ = count.wait_for(|count| *count == 0).await;
    }
}

struct SocketRegistration {
    id: u64,
    registry: Arc<SocketRegistry>,
    lifecycle: watch::Receiver<u8>,
}

impl Drop for SocketRegistration {
    fn drop(&mut self) {
        let mut state = self
            .registry
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.sessions.remove(&self.id);
        self.registry.count.send_replace(state.sessions.len());
    }
}

pub(super) fn is_responses_path(path: &str) -> bool {
    path == RESPONSES_PATH || path == RESPONSES_ALIAS
}

pub(super) fn wants_socket(method: &Method, headers: &HeaderMap) -> bool {
    method == Method::GET
        && headers
            .get(header::UPGRADE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
}

pub(super) async fn path_guard(
    State(app): State<Arc<Inner>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if wants_socket(request.method(), request.headers()) {
        let path = request.uri().path();
        let admitted = is_responses_path(path)
            || app
                .surfaces
                .iter()
                .any(|surface| surface.responses_websocket_paths().contains(&path));
        if !admitted {
            return upgrade_required();
        }
    }
    next.run(request).await
}

pub(super) fn upgrade_required() -> Response {
    text_response(
        StatusCode::UPGRADE_REQUIRED,
        "use POST, or open a Responses WebSocket on a Responses route",
    )
}

pub(super) async fn get(State(app): State<Arc<Inner>>, request: Request<Body>) -> Response {
    if !wants_socket(request.method(), request.headers()) {
        return Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .header(header::ALLOW, "POST")
            .body(Body::empty())
            .unwrap_or_else(|_| text_response(StatusCode::METHOD_NOT_ALLOWED, "use POST"));
    }
    if app.sockets.ending() {
        return text_response(StatusCode::SERVICE_UNAVAILABLE, RESTARTING);
    }
    if !origin_matches_host(request.headers()) {
        return text_response(
            StatusCode::FORBIDDEN,
            "the request Origin is not authorized for this Host",
        );
    }
    let codec = app.codecs.get(RESPONSES_PATH).cloned();
    if let Some(response) = reject(
        &current_token(&app),
        request.headers(),
        request.uri(),
        codec.as_deref(),
    ) {
        return response;
    }
    let (mut parts, _) = request.into_parts();
    let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
        Ok(upgrade) => upgrade,
        Err(error) => return error.into_response(),
    };
    upgrade
        .max_message_size(BODY_LIMIT)
        .on_upgrade(move |socket| serve_socket(app, parts, socket))
}

/// A browser page from another site may not open a socket: when the upgrade
/// carries an `Origin`, its host (with port) must be the request's `Host`.
/// A sandboxed frame sends `Origin: null`, which has no host and is refused.
fn origin_matches_host(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    origin
        .to_str()
        .ok()
        .and_then(|origin| origin.parse::<axum::http::Uri>().ok())
        .and_then(|origin| {
            origin
                .authority()
                .map(|authority| authority.as_str().to_owned())
        })
        .is_some_and(|authority| authority.eq_ignore_ascii_case(host))
}

async fn serve_socket(app: Arc<Inner>, origin: Parts, mut socket: WebSocket) {
    let Some(registration) = app.sockets.register() else {
        let _ = socket
            .send(Message::Close(Some(CloseFrame {
                code: close_code::AWAY,
                reason: Utf8Bytes::from(RESTARTING),
            })))
            .await;
        return;
    };
    let lifecycle = registration.lifecycle.clone();
    let _registration = registration;
    SocketSession {
        app,
        origin,
        held: VecDeque::new(),
        stats: SocketStats::default(),
    }
    .run(socket, lifecycle)
    .await;
}

#[derive(Default)]
struct SocketStats {
    turns: u64,
    continued: u64,
    bytes_received: u64,
    bytes_sent: u64,
}

impl SocketStats {
    /// A socket that served no turn ends without a line.
    fn log(&self) {
        if self.turns == 0 {
            return;
        }
        tracing::info!(
            turns = self.turns,
            continued = self.continued,
            bytes_received = self.bytes_received,
            bytes_sent_as_whole_requests = self.bytes_sent,
            "responses WebSocket ended"
        );
    }
}

struct HeldTurn {
    id: String,
    items: Vec<Value>,
}

type SocketSink = Arc<AsyncMutex<SplitSink<WebSocket, Message>>>;
type SocketMessages = VecDeque<Result<Message, axum::Error>>;

#[derive(Clone)]
struct SocketWriter {
    sink: SocketSink,
    cancelled: Option<Arc<AtomicBool>>,
}

impl SocketWriter {
    fn new(sink: SplitSink<WebSocket, Message>) -> Self {
        Self {
            sink: Arc::new(AsyncMutex::new(sink)),
            cancelled: None,
        }
    }

    fn for_turn(&self, run: &Arc<TurnRun>) -> Self {
        Self {
            sink: Arc::clone(&self.sink),
            cancelled: Some(Arc::clone(&run.cancelled)),
        }
    }

    fn cancelled(&self) -> bool {
        self.cancelled
            .as_ref()
            .is_some_and(|cancelled| cancelled.load(Ordering::SeqCst))
    }
}

struct TurnRun {
    ids: Mutex<Vec<String>>,
    cancelled: Arc<AtomicBool>,
    cancel: watch::Sender<bool>,
}

impl TurnRun {
    fn new() -> Self {
        Self {
            ids: Mutex::new(Vec::new()),
            cancelled: Arc::new(AtomicBool::new(false)),
            cancel: watch::channel(false).0,
        }
    }

    fn subscribe(&self) -> watch::Receiver<bool> {
        self.cancel.subscribe()
    }

    fn note(&self, id: &str) {
        if id.is_empty() {
            return;
        }
        let mut ids = self
            .ids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !ids.iter().any(|known| known == id) {
            ids.push(id.to_owned());
        }
    }

    fn interrupt(&self, id: &str) -> bool {
        let matched = self
            .ids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .any(|known| known == id);
        if matched {
            self.cancel();
        }
        matched
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.cancel.send_replace(true);
    }
}

struct SocketSession {
    app: Arc<Inner>,
    origin: Parts,
    held: VecDeque<HeldTurn>,
    stats: SocketStats,
}

impl SocketSession {
    fn keep(&mut self, turn: HeldTurn) {
        self.held.push_back(turn);
        while self.held.len() > KEEP_TURNS {
            self.held.pop_front();
        }
    }

    fn prior(&self, id: &str) -> Option<&HeldTurn> {
        self.held.iter().rev().find(|turn| turn.id == id)
    }

    async fn run(mut self, socket: WebSocket, mut lifecycle: watch::Receiver<u8>) {
        let (sink, mut incoming) = socket.split();
        let writer = SocketWriter::new(sink);
        let mut queued = SocketMessages::new();
        loop {
            // going away or cut, a socket between turns closes 1001 now
            if *lifecycle.borrow_and_update() != 0 {
                let _ = close(&writer, close_code::AWAY, RESTARTING).await;
                break;
            }
            let message = match queued.pop_front() {
                Some(message) => Some(message),
                None => {
                    tokio::select! {
                        message = incoming.next() => message,
                        // the loop-top check closes or cuts the socket
                        _ = lifecycle.changed() => continue,
                    }
                }
            };
            let Some(message) = message else {
                break;
            };
            let text = match message {
                Ok(Message::Text(text)) => text,
                Ok(Message::Binary(_)) => {
                    let _ = close(&writer, close_code::UNSUPPORTED, "text frames only").await;
                    break;
                }
                Ok(Message::Close(_)) | Err(_) => break,
                Ok(Message::Ping(_) | Message::Pong(_)) => continue,
            };
            let parsed = serde_json::from_slice::<Value>(text.as_bytes())
                .ok()
                .filter(Value::is_object);
            let kind = match &parsed {
                Some(value) => value.get("type").and_then(Value::as_str).map(str::to_owned),
                None => names_create(text.as_str()).then(|| "response.create".to_owned()),
            };
            if kind.as_deref() == Some("response.interrupt") {
                let id = parsed
                    .as_ref()
                    .and_then(|value| value.get("response_id"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !send_json(&writer, &interrupt_reply("response.interrupt.failed", id)).await {
                    break;
                }
                continue;
            }
            if kind.as_deref() != Some("response.create") {
                if !send_local_error(
                    &writer,
                    "unsupported_message",
                    "only response.create is supported on this socket",
                    Some("type"),
                )
                .await
                {
                    break;
                }
                continue;
            }
            self.stats.turns += 1;
            self.stats.bytes_received += text.len() as u64;
            let Some(value) = parsed else {
                if !send_local_error(
                    &writer,
                    "invalid_json",
                    "response.create is not a JSON object",
                    None,
                )
                .await
                {
                    break;
                }
                continue;
            };
            let prepared = match self.prepare(value) {
                Ok(prepared) => prepared,
                Err(error) => {
                    if !send_local_error(&writer, error.code, &error.message, error.param).await {
                        break;
                    }
                    continue;
                }
            };
            match self
                .drive_turn(
                    prepared,
                    &writer,
                    &mut incoming,
                    &mut queued,
                    &mut lifecycle,
                )
                .await
            {
                DriveResult::Disconnected => break,
                DriveResult::Interrupted => {}
                DriveResult::Finished(mapped) => {
                    if !mapped.connected {
                        break;
                    }
                    if mapped.cut {
                        let _ = close(&writer, close_code::ERROR, CUT_REASON).await;
                        break;
                    }
                    if let Some(held) = mapped.held {
                        self.keep(held);
                    }
                }
            }
        }
        self.stats.log();
    }

    async fn drive_turn(
        &mut self,
        prepared: Prepared,
        writer: &SocketWriter,
        incoming: &mut SplitStream<WebSocket>,
        queued: &mut SocketMessages,
        lifecycle: &mut watch::Receiver<u8>,
    ) -> DriveResult {
        if let Prepared::Replay { body, .. } = &prepared {
            self.stats.bytes_sent += body.len() as u64;
        }
        let run = Arc::new(TurnRun::new());
        let turn_writer = writer.for_turn(&run);
        let cancelled = run.subscribe();
        let mut task = tokio::spawn(run_prepared(
            Arc::clone(&self.app),
            self.origin.clone(),
            prepared,
            turn_writer,
            Arc::clone(&run),
            cancelled,
        ));
        loop {
            tokio::select! {
                outcome = &mut task => {
                    return match outcome {
                        Ok(mapped) => DriveResult::Finished(mapped),
                        Err(_) => DriveResult::Disconnected,
                    };
                }
                message = incoming.next() => {
                    let Some(message) = message.filter(|message| {
                        !matches!(message, Ok(Message::Close(_)) | Err(_))
                    }) else {
                        // the client went: the turn in flight ends with it
                        run.cancel();
                        let _ = task.await;
                        return DriveResult::Disconnected;
                    };
                    if let Some(id) = interrupt_id(&message) {
                        if run.interrupt(&id) {
                            let _ = task.await;
                            if !send_json(writer, &interrupt_reply("response.interrupt.accepted", &id)).await
                                || !send_json(writer, &interrupted_reply(&id)).await
                            {
                                return DriveResult::Disconnected;
                            }
                            return DriveResult::Interrupted;
                        }
                        if !send_json(writer, &interrupt_reply("response.interrupt.failed", &id)).await {
                            return DriveResult::Disconnected;
                        }
                    } else {
                        queued.push_back(message);
                    }
                }
                // going away waits for the turn; the loop-top check closes 1001
                _ = lifecycle.changed() => {
                    if *lifecycle.borrow() == SOCKET_CUT {
                        run.cancel();
                        task.abort();
                        let _ = task.await;
                        return DriveResult::Disconnected;
                    }
                }
            }
        }
    }

    fn prepare(&mut self, mut value: Value) -> Result<Prepared, LocalError> {
        let fresh = input_items(value.get("input"))?;
        let mut input = Vec::new();
        if let Some(previous) = value.get("previous_response_id")
            && !previous.is_null()
        {
            let id = previous.as_str().unwrap_or_default();
            let Some(prior) = self.prior(id) else {
                return Err(LocalError {
                    code: "previous_response_not_found",
                    message: format!("Previous response with id '{id}' not found."),
                    param: Some("previous_response_id"),
                });
            };
            input.extend(prior.items.iter().cloned());
            self.stats.continued += 1;
        }
        input.extend(fresh);
        if value.get("generate").and_then(Value::as_bool) == Some(false) {
            return Ok(Prepared::Warmup {
                input,
                model: value.get("model").cloned(),
            });
        }
        if let Some(object) = value.as_object_mut() {
            object.remove("type");
            object.remove("previous_response_id");
            object.remove("generate");
            object.insert("input".to_owned(), Value::Array(input.clone()));
        }
        let body = serde_json::to_vec(&value).map_err(|error| LocalError {
            code: "invalid_json",
            message: error.to_string(),
            param: None,
        })?;
        Ok(Prepared::Replay { body, input })
    }
}

enum DriveResult {
    Disconnected,
    Interrupted,
    Finished(MappedTurn),
}

fn interrupt_id(message: &Result<Message, axum::Error>) -> Option<String> {
    let Message::Text(text) = message.as_ref().ok()? else {
        return None;
    };
    let value = serde_json::from_slice::<Value>(text.as_bytes()).ok()?;
    (value.get("type").and_then(Value::as_str) == Some("response.interrupt")).then(|| {
        value
            .get("response_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    })
}

fn interrupt_reply(kind: &str, id: &str) -> Value {
    json!({"type": kind, "response_id": id})
}

fn interrupted_reply(id: &str) -> Value {
    json!({
        "type": "response.incomplete",
        "response": {
            "id": id,
            "object": "response",
            "status": "incomplete",
            "incomplete_details": {"reason": "interrupted"},
            "output": [],
        },
    })
}

async fn run_prepared(
    app: Arc<Inner>,
    origin: Parts,
    prepared: Prepared,
    writer: SocketWriter,
    run: Arc<TurnRun>,
    mut cancelled: watch::Receiver<bool>,
) -> MappedTurn {
    let work = async move {
        match prepared {
            Prepared::Warmup { input, model } => warmup(&writer, &run, input, model).await,
            Prepared::Replay { body, input } => {
                let request = replay_request(&origin, &body);
                let response = match request_router(&app).oneshot(request).await {
                    Ok(response) => response,
                    Err(never) => match never {},
                };
                map_response(&writer, response, input, &run).await
            }
        }
    };
    tokio::pin!(work);
    tokio::select! {
        biased;
        _ = cancelled.wait_for(|cancelled| *cancelled) => MappedTurn {
            held: None,
            cut: false,
            connected: true,
        },
        mapped = &mut work => mapped,
    }
}

async fn warmup(
    writer: &SocketWriter,
    run: &Arc<TurnRun>,
    input: Vec<Value>,
    model: Option<Value>,
) -> MappedTurn {
    let id = format!("resp_{}", next_request_id());
    run.note(&id);
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    let mut response = Map::new();
    response.insert("id".to_owned(), Value::String(id.clone()));
    response.insert("object".to_owned(), Value::String("response".to_owned()));
    response.insert("created_at".to_owned(), json!(created_at));
    response.insert("status".to_owned(), Value::String("in_progress".to_owned()));
    response.insert("output".to_owned(), Value::Array(Vec::new()));
    if let Some(model) = model {
        response.insert("model".to_owned(), model);
    }
    if !send_json(
        writer,
        &json!({"type": "response.created", "response": response}),
    )
    .await
    {
        return MappedTurn {
            held: None,
            cut: false,
            connected: false,
        };
    }
    response.insert("status".to_owned(), Value::String("completed".to_owned()));
    response.insert(
        "usage".to_owned(),
        json!({"input_tokens": 0, "output_tokens": 0, "total_tokens": 0}),
    );
    let connected = send_json(
        writer,
        &json!({"type": "response.completed", "response": response}),
    )
    .await;
    MappedTurn {
        held: connected.then_some(HeldTurn { id, items: input }),
        cut: false,
        connected,
    }
}

/// Whether a frame that is not a JSON object still names `response.create`
/// as its `type`. The frame type is judged before the body, so a broken
/// create is answered `invalid_json` while any other unreadable frame is an
/// unsupported message.
fn names_create(text: &str) -> bool {
    let compact: String = text.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    compact.starts_with('{') && compact.contains(r#""type":"response.create""#)
}

enum Prepared {
    Warmup {
        input: Vec<Value>,
        model: Option<Value>,
    },
    Replay {
        body: Vec<u8>,
        input: Vec<Value>,
    },
}

struct LocalError {
    code: &'static str,
    message: String,
    param: Option<&'static str>,
}

fn input_items(input: Option<&Value>) -> Result<Vec<Value>, LocalError> {
    match input {
        Some(Value::String(text)) => Ok(vec![json!({
            "type": "message",
            "role": "user",
            "content": text,
        })]),
        Some(Value::Array(items)) => Ok(items.clone()),
        _ => Err(LocalError {
            code: "invalid_value",
            message: "input is neither text nor a list of items".to_owned(),
            param: Some("input"),
        }),
    }
}

fn replay_request(origin: &Parts, body: &[u8]) -> Request<Body> {
    let mut parts = origin.clone();
    parts.method = Method::POST;
    let nominated: Vec<String> = origin
        .headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|name| name.trim().to_ascii_lowercase())
        .filter(|name| !name.is_empty())
        .collect();
    let mut headers = HeaderMap::new();
    for (name, value) in &origin.headers {
        let key = name.as_str();
        if nominated.iter().any(|nominated| nominated == key)
            || matches!(
                key,
                "connection"
                    | "keep-alive"
                    | "proxy-authenticate"
                    | "proxy-authorization"
                    | "te"
                    | "trailer"
                    | "transfer-encoding"
                    | "upgrade"
                    | "content-length"
                    | "content-type"
                    | "content-encoding"
                    | "accept"
                    | "accept-encoding"
            )
            || key.starts_with("sec-websocket-")
        {
            continue;
        }
        if key == wire::BETA_HEADER
            && value
                .to_str()
                .ok()
                .is_some_and(|value| value.contains(wire::SOCKET_BETA))
        {
            continue;
        }
        headers.append(name.clone(), value.clone());
    }
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(
        header::ACCEPT,
        HeaderValue::from_static("text/event-stream"),
    );
    if let Ok(length) = HeaderValue::try_from(body.len().to_string()) {
        headers.insert(header::CONTENT_LENGTH, length);
    }
    if let Ok(value) = serde_json::from_slice::<Value>(body) {
        if let Some(turn_state) = value
            .get("client_metadata")
            .and_then(|metadata| metadata.get(wire::TURN_STATE))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            && let Ok(turn_state) = HeaderValue::try_from(turn_state)
        {
            headers.insert(wire::TURN_STATE, turn_state);
        }
        if value
            .get("client_metadata")
            .and_then(|metadata| metadata.get(wire::LITE_METADATA))
            .and_then(Value::as_str)
            .is_some_and(|value| value.eq_ignore_ascii_case("true"))
        {
            headers.insert(wire::LITE_HEADER, HeaderValue::from_static("true"));
        }
    }
    parts.headers = headers;
    Request::from_parts(parts, Body::from(body.to_vec()))
}

struct MappedTurn {
    held: Option<HeldTurn>,
    cut: bool,
    connected: bool,
}

struct TurnEvents {
    id: String,
    items: Vec<Value>,
    ended: bool,
    completed: bool,
    run: Arc<TurnRun>,
}

impl TurnEvents {
    fn new(run: &Arc<TurnRun>) -> Self {
        Self {
            id: String::new(),
            items: Vec::new(),
            ended: false,
            completed: false,
            run: Arc::clone(run),
        }
    }
}

async fn map_response(
    writer: &SocketWriter,
    response: Response<Body>,
    input: Vec<Value>,
    run: &Arc<TurnRun>,
) -> MappedTurn {
    let status = response.status();
    let headers = response.headers().clone();
    let stream = status.is_success()
        && headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.to_ascii_lowercase().contains("event-stream"));
    if !status.is_success() {
        let frame = http_error_reply(response).await;
        return MappedTurn {
            held: None,
            cut: false,
            connected: send_json(writer, &frame).await,
        };
    }
    if stream {
        return map_stream(writer, response.into_body(), &headers, input, run).await;
    }
    let body = read_body(response.into_body(), None).await;
    let Ok(response) = serde_json::from_slice::<Value>(&body) else {
        let frame = server_error_frame(
            StatusCode::BAD_GATEWAY,
            &headers,
            "the reply was neither a stream nor a response",
        );
        return MappedTurn {
            held: None,
            cut: false,
            connected: send_json(writer, &frame).await,
        };
    };
    if response.get("object").and_then(Value::as_str) != Some("response")
        || response
            .get("id")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
    {
        let frame = server_error_frame(
            StatusCode::BAD_GATEWAY,
            &headers,
            "the reply was neither a stream nor a response",
        );
        return MappedTurn {
            held: None,
            cut: false,
            connected: send_json(writer, &frame).await,
        };
    }
    if !send_header_frames(writer, &headers).await {
        return MappedTurn {
            held: None,
            cut: false,
            connected: false,
        };
    }
    let mut events = TurnEvents::new(run);
    let mut connected = send_event(
        writer,
        &headers,
        &mut events,
        &json!({"type": "response.created", "response": response}),
    )
    .await;
    if connected {
        for (index, item) in response
            .get("output")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            connected = send_event(
                writer,
                &headers,
                &mut events,
                &json!({"type": "response.output_item.done", "output_index": index, "item": item}),
            )
            .await;
            if !connected {
                break;
            }
        }
    }
    if connected {
        let kind = match response.get("status").and_then(Value::as_str) {
            Some("incomplete") => "response.incomplete",
            Some("failed") => "response.failed",
            _ => "response.completed",
        };
        connected = send_event(
            writer,
            &headers,
            &mut events,
            &json!({"type": kind, "response": response}),
        )
        .await;
    }
    finish_mapping(events, input, connected)
}

async fn map_stream(
    writer: &SocketWriter,
    body: Body,
    headers: &HeaderMap,
    input: Vec<Value>,
    run: &Arc<TurnRun>,
) -> MappedTurn {
    if !send_header_frames(writer, headers).await {
        return MappedTurn {
            held: None,
            cut: false,
            connected: false,
        };
    }
    let mut stream = body.into_data_stream();
    let mut parser = SseParser::default();
    let mut events = TurnEvents::new(run);
    let mut connected = true;
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            break;
        };
        parser.push(&chunk);
        while let Some(data) = parser.next() {
            if !send_event_bytes(writer, headers, &mut events, &data).await {
                connected = false;
                break;
            }
        }
        if !connected {
            break;
        }
    }
    if connected && let Some(data) = parser.finish() {
        connected = send_event_bytes(writer, headers, &mut events, &data).await;
    }
    let cut = connected && !events.ended;
    let mut mapped = finish_mapping(events, input, connected);
    mapped.cut = cut;
    mapped
}

fn finish_mapping(events: TurnEvents, mut input: Vec<Value>, connected: bool) -> MappedTurn {
    let held = if connected && events.completed && !events.id.is_empty() {
        input.extend(events.items);
        Some(HeldTurn {
            id: events.id,
            items: input,
        })
    } else {
        None
    };
    MappedTurn {
        held,
        cut: false,
        connected,
    }
}

async fn send_event_bytes(
    writer: &SocketWriter,
    headers: &HeaderMap,
    events: &mut TurnEvents,
    data: &[u8],
) -> bool {
    let trimmed = trim_ascii(data);
    if trimmed.is_empty() || trimmed == b"[DONE]" {
        return true;
    }
    let Ok(value) = serde_json::from_slice::<Value>(trimmed) else {
        return true;
    };
    send_event(writer, headers, events, &value).await
}

async fn send_event(
    writer: &SocketWriter,
    headers: &HeaderMap,
    events: &mut TurnEvents,
    value: &Value,
) -> bool {
    match value.get("type").and_then(Value::as_str) {
        Some("response.created" | "response.in_progress") => note_id(events, value),
        Some("response.output_item.done") => {
            if let Some(item) = value.get("item") {
                events.items.push(item.clone());
            }
        }
        Some("response.completed") => {
            note_id(events, value);
            events.ended = true;
            events.completed = true;
        }
        Some("response.incomplete" | "response.failed" | "response.cancelled" | "error") => {
            events.ended = true
        }
        _ => {}
    }
    let value = with_gateway_etag(value.clone(), headers);
    send_json(writer, &value).await
}

fn with_gateway_etag(mut value: Value, headers: &HeaderMap) -> Value {
    if value.get("type").and_then(Value::as_str) == Some(wire::RESPONSE_METADATA_EVENT)
        && let Some(etag) = headers
            .get(wire::MODELS_ETAG_HEADER)
            .and_then(|value| value.to_str().ok())
        && let Some(event_headers) = value.get_mut("headers").and_then(Value::as_object_mut)
        && event_headers.contains_key(wire::MODELS_ETAG_HEADER)
    {
        event_headers.insert(
            wire::MODELS_ETAG_HEADER.to_owned(),
            Value::String(etag.to_owned()),
        );
    }
    value
}

fn note_id(events: &mut TurnEvents, value: &Value) {
    if let Some(id) = value
        .get("response")
        .and_then(|response| response.get("id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
    {
        events.id = id.to_owned();
        events.run.note(id);
    }
}

async fn send_header_frames(writer: &SocketWriter, headers: &HeaderMap) -> bool {
    for frame in header_frames(headers) {
        if !send_json(writer, &frame).await {
            return false;
        }
    }
    true
}

fn header_frames(headers: &HeaderMap) -> Vec<Value> {
    let mut frames = Vec::new();
    let mut metadata = Map::new();
    for name in [wire::TURN_STATE, wire::MODEL_HEADER, wire::REASONING_HEADER] {
        if let Some(value) = headers.get(name).and_then(|value| value.to_str().ok())
            && !value.is_empty()
        {
            metadata.insert(name.to_owned(), Value::String(value.to_owned()));
        }
    }
    if !metadata.is_empty() {
        frames.push(json!({"type": "response.metadata", "headers": metadata}));
    }
    if let Some(etag) = headers
        .get(wire::MODELS_ETAG_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
    {
        frames.push(json!({
            "type": wire::RESPONSE_METADATA_EVENT,
            "headers": {wire::MODELS_ETAG_HEADER: etag},
        }));
    }
    if let Some(limits) = rate_limits_frame(headers) {
        frames.push(limits);
    }
    frames
}

fn rate_limits_frame(headers: &HeaderMap) -> Option<Value> {
    let mut windows = Map::new();
    for which in ["primary", "secondary"] {
        let used_name = format!("{}{which}-used-percent", wire::CLIENT_HEADER_PREFIX);
        let Some(used) = headers
            .get(used_name.as_str())
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<f64>().ok())
            .and_then(serde_json::Number::from_f64)
        else {
            continue;
        };
        let mut window = Map::new();
        window.insert("used_percent".to_owned(), Value::Number(used));
        for (field, suffix) in [
            ("window_minutes", "window-minutes"),
            ("reset_at", "reset-at"),
        ] {
            let name = format!("{}{which}-{suffix}", wire::CLIENT_HEADER_PREFIX);
            if let Some(value) = headers
                .get(name.as_str())
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<i64>().ok())
            {
                window.insert(field.to_owned(), json!(value));
            }
        }
        windows.insert(which.to_owned(), Value::Object(window));
    }
    if windows.is_empty() {
        return None;
    }
    let mut event = Map::new();
    event.insert(
        "type".to_owned(),
        Value::String(wire::RATE_LIMITS_EVENT.to_owned()),
    );
    event.insert("rate_limits".to_owned(), Value::Object(windows));
    if let Some(plan) = headers
        .get(wire::PLAN_TYPE_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
    {
        event.insert("plan_type".to_owned(), Value::String(plan.to_owned()));
    }
    if let (Some(has), Some(unlimited)) = (
        headers
            .get(wire::CREDITS_HAS_HEADER)
            .and_then(|value| value.to_str().ok()),
        headers
            .get(wire::CREDITS_UNLIMITED_HEADER)
            .and_then(|value| value.to_str().ok()),
    ) {
        let mut credits = Map::new();
        credits.insert(
            "has_credits".to_owned(),
            json!(has.eq_ignore_ascii_case("true")),
        );
        credits.insert(
            "unlimited".to_owned(),
            json!(unlimited.eq_ignore_ascii_case("true")),
        );
        if let Some(balance) = headers
            .get(wire::CREDITS_BALANCE_HEADER)
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.is_empty())
        {
            credits.insert("balance".to_owned(), Value::String(balance.to_owned()));
        }
        event.insert("credits".to_owned(), Value::Object(credits));
    }
    Some(Value::Object(event))
}

/// The error frame for a reply that failed, its body read up to
/// [`ERROR_BODY_LIMIT`].
async fn http_error_reply(response: Response<Body>) -> Value {
    let status = response.status();
    let headers = response.headers().clone();
    let body = read_body(response.into_body(), Some(ERROR_BODY_LIMIT)).await;
    http_error_frame(status, &headers, &body)
}

fn http_error_frame(status: StatusCode, headers: &HeaderMap, body: &[u8]) -> Value {
    let error = serde_json::from_slice::<Value>(trim_ascii(body))
        .ok()
        .and_then(|value| value.get("error").cloned())
        .filter(Value::is_object)
        .unwrap_or_else(|| {
            let text = std::str::from_utf8(trim_ascii(body))
                .ok()
                .filter(|text| !text.is_empty());
            json!({
                "type": "server_error",
                "message": text.unwrap_or_else(|| status.canonical_reason().unwrap_or("upstream error")),
            })
        });
    error_frame(status, headers, error)
}

fn server_error_frame(status: StatusCode, headers: &HeaderMap, message: &str) -> Value {
    error_frame(
        status,
        headers,
        json!({"type": "server_error", "message": message}),
    )
}

fn error_frame(status: StatusCode, headers: &HeaderMap, error: Value) -> Value {
    let mut frame = Map::new();
    frame.insert("type".to_owned(), Value::String("error".to_owned()));
    frame.insert("status".to_owned(), json!(status.as_u16()));
    frame.insert("error".to_owned(), error);
    let mut copied = Map::new();
    for (name, value) in headers {
        let name = name.as_str();
        if (matches!(
            name,
            "retry-after" | "retry-after-ms" | "x-request-id" | "x-error-json"
        ) || name.starts_with(wire::CLIENT_HEADER_PREFIX)
            || name.starts_with(wire::RATE_LIMIT_HEADER_PREFIX))
            && let Ok(value) = value.to_str()
        {
            copied.insert(name.to_owned(), Value::String(value.to_owned()));
        }
    }
    if !copied.is_empty() {
        frame.insert("headers".to_owned(), Value::Object(copied));
    }
    Value::Object(frame)
}

async fn send_local_error(
    writer: &SocketWriter,
    code: &str,
    message: &str,
    param: Option<&str>,
) -> bool {
    let mut error = Map::new();
    error.insert(
        "type".to_owned(),
        Value::String("invalid_request_error".to_owned()),
    );
    error.insert("code".to_owned(), Value::String(code.to_owned()));
    error.insert("message".to_owned(), Value::String(message.to_owned()));
    if let Some(param) = param {
        error.insert("param".to_owned(), Value::String(param.to_owned()));
    }
    send_json(
        writer,
        &json!({"type": "error", "status": 400, "error": error}),
    )
    .await
}

async fn send_json(writer: &SocketWriter, value: &Value) -> bool {
    let Ok(text) = serde_json::to_string(value) else {
        return false;
    };
    let mut sink = writer.sink.lock().await;
    if writer.cancelled() {
        return false;
    }
    sink.send(Message::Text(Utf8Bytes::from(text)))
        .await
        .is_ok()
}

async fn close(writer: &SocketWriter, code: u16, reason: &'static str) -> Result<(), axum::Error> {
    writer
        .sink
        .lock()
        .await
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: Utf8Bytes::from(reason),
        })))
        .await
}

async fn read_body(body: Body, cap: Option<usize>) -> Vec<u8> {
    let mut stream = body.into_data_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            break;
        };
        let keep = cap.map_or(chunk.len(), |cap| {
            cap.saturating_sub(bytes.len()).min(chunk.len())
        });
        bytes.extend_from_slice(&chunk[..keep]);
        if cap.is_some_and(|cap| bytes.len() >= cap) {
            break;
        }
    }
    bytes
}

#[derive(Default)]
struct SseParser {
    pending: Vec<u8>,
}

impl SseParser {
    fn push(&mut self, chunk: &[u8]) {
        self.pending.extend_from_slice(chunk);
    }

    fn next(&mut self) -> Option<Vec<u8>> {
        let (index, width) = event_boundary(&self.pending)?;
        let event: Vec<u8> = self.pending.drain(..index + width).collect();
        Some(event_data(&event[..index]))
    }

    fn finish(&mut self) -> Option<Vec<u8>> {
        if self.pending.is_empty() {
            return None;
        }
        let event = std::mem::take(&mut self.pending);
        Some(event_data(&event))
    }
}

fn event_boundary(bytes: &[u8]) -> Option<(usize, usize)> {
    for index in 0..bytes.len() {
        if bytes[index] == b'\n' {
            if index + 1 < bytes.len() && bytes[index + 1] == b'\n' {
                return Some((index, 2));
            }
            if index > 0 && bytes[index - 1] == b'\r' {
                if index + 2 < bytes.len() && bytes[index + 1..index + 3] == *b"\r\n" {
                    return Some((index - 1, 4));
                }
                if index > 1 && bytes[index - 2] == b'\r' {
                    return Some((index - 2, 3));
                }
            }
        } else if bytes[index] == b'\r' && index + 1 < bytes.len() && bytes[index + 1] == b'\r' {
            return Some((index, 2));
        }
    }
    None
}

fn event_data(event: &[u8]) -> Vec<u8> {
    let mut data = Vec::new();
    for line in event.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some(content) = line.strip_prefix(b"data:") else {
            continue;
        };
        let content = content.strip_prefix(b" ").unwrap_or(content);
        if !data.is_empty() {
            data.push(b'\n');
        }
        data.extend_from_slice(content);
    }
    data
}

fn trim_ascii(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[1..];
    }
    while bytes.last().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

#[cfg(test)]
mod tests;
