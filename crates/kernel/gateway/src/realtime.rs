//! Realtime session passthrough.
//!
//! `GET /v1/realtime` and `GET /realtime` upgrade to a WebSocket and relay
//! frames to the one provider the router selects. Only a member that returns
//! dial material is eligible. A gateway-key reservation is held until the
//! socket closes. Frames are bounded, an idle session closes, and shutdown
//! closes the session.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::ws::{CloseFrame, Message, Utf8Bytes, WebSocket, close_code};
use axum::extract::{FromRequestParts, State, WebSocketUpgrade};
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use hofund_plugin_api::responses_ws::BETA_HEADER;
use hofund_router::{Candidates, RealtimeDial, RouterError};
use hofund_secrets::Identity;
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message as UpstreamMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{connect_async_with_config, tungstenite};

use super::responses_ws::{origin_matches_host, wants_socket};
use super::{Inner, authenticate_request, text_response};

const REALTIME_PATH: &str = "/v1/realtime";
const REALTIME_ALIAS: &str = "/realtime";
/// Largest accepted frame. Audio sessions need more room than a chat body.
const FRAME_LIMIT: usize = 32 << 20;
/// How long a session may sit without a frame.
const IDLE: Duration = Duration::from_secs(10 * 60);
/// How often a live session rechecks the gateway key.
const KEY_TICK: Duration = Duration::from_secs(5);
const DIAL_TIMEOUT: Duration = Duration::from_secs(30);
const MODEL_LIMIT: usize = 512;
/// Skip usage parsing on a frame larger than this. A done event is small.
const USAGE_PARSE_LIMIT: usize = 64 * 1024;

type UpstreamSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

pub(super) struct Registry {
    inner: Mutex<RegistryState>,
    count: watch::Sender<usize>,
}

struct RegistryState {
    ending: bool,
    next: u64,
    sessions: HashMap<u64, watch::Sender<bool>>,
}

impl Registry {
    pub(super) fn new() -> Self {
        let (count, _) = watch::channel(0);
        Self {
            inner: Mutex::new(RegistryState {
                ending: false,
                next: 0,
                sessions: HashMap::new(),
            }),
            count,
        }
    }

    pub(super) fn ending(&self) -> bool {
        self.lock().ending
    }

    fn register(&self) -> Option<(u64, watch::Receiver<bool>)> {
        let mut state = self.lock();
        if state.ending {
            return None;
        }
        let id = state.next;
        state.next = state.next.wrapping_add(1);
        let (sender, receiver) = watch::channel(false);
        state.sessions.insert(id, sender);
        let _ = self.count.send(state.sessions.len());
        Some((id, receiver))
    }

    fn finish(&self, id: u64) {
        let mut state = self.lock();
        state.sessions.remove(&id);
        let _ = self.count.send(state.sessions.len());
    }

    pub(super) fn drain(&self) {
        let mut state = self.lock();
        state.ending = true;
        for sender in state.sessions.values() {
            let _ = sender.send(true);
        }
    }

    pub(super) fn cut(&self) {
        self.drain();
    }

    pub(super) async fn wait(&self) {
        let mut seen = self.count.subscribe();
        while *seen.borrow() > 0 {
            if seen.changed().await.is_err() {
                break;
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RegistryState> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

pub(super) fn is_realtime_path(path: &str) -> bool {
    path == REALTIME_PATH || path == REALTIME_ALIAS
}

pub(super) async fn get(State(app): State<Arc<Inner>>, request: Request<Body>) -> Response {
    if app.realtimes.ending() {
        return text_response(StatusCode::SERVICE_UNAVAILABLE, "hofund is restarting");
    }
    let identity = match authenticate_request(&app, request.headers(), request.uri(), None, None) {
        Ok(identity) => identity,
        Err(response) => return *response,
    };
    if !wants_socket(request.method(), request.headers()) {
        return upgrade_required();
    }
    if !origin_matches_host(request.headers()) {
        return text_response(
            StatusCode::FORBIDDEN,
            "the request Origin is not authorized for this Host",
        );
    }
    let Some(model) = query_model(request.uri()) else {
        return text_response(StatusCode::BAD_REQUEST, "a realtime session needs a model");
    };
    if model.is_empty() || model.len() > MODEL_LIMIT {
        return text_response(StatusCode::BAD_REQUEST, "a realtime session needs a model");
    }
    let router = app.snapshot.load();
    let candidates = match router.candidates(&model, Some(&super::route_scope(&identity))) {
        Ok(candidates) => candidates,
        Err(error) => {
            let (status, message) = super::stream_error(&error);
            let message = match &error {
                RouterError::ModelScope { .. } | RouterError::AccountScope { .. } => {
                    super::scope_error(&error, &identity)
                }
                _ => message,
            };
            return text_response(status, &message);
        }
    };
    if several_providers(&model, &candidates) {
        return text_response(
            StatusCode::BAD_REQUEST,
            "a realtime session goes to one model",
        );
    }
    let reservation = match app.budget.reserve(&identity, 0, chrono::Local::now()) {
        Ok(reservation) => reservation,
        Err(super::limits::ReserveFailure::Refused(refusal)) => {
            return super::limits::refusal_response(
                &hofund_codec::OpenAiChat,
                &refusal,
                chrono::Local::now(),
            );
        }
        Err(super::limits::ReserveFailure::Unavailable) => {
            return text_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "gateway key limits are temporarily unavailable",
            );
        }
    };
    let protocols = forwarded_protocols(request.headers().get(header::SEC_WEBSOCKET_PROTOCOL));
    let beta = header_text(request.headers(), BETA_HEADER);
    let mut opened = None;
    let mut saw_endpoint = false;
    let mut last_error = None;
    for hop in &candidates.hops {
        let wire = hop
            .wire_model
            .clone()
            .unwrap_or_else(|| candidates.model.clone());
        let Some(dial) = hop.member.realtime(&wire) else {
            continue;
        };
        saw_endpoint = true;
        match dial_upstream(&dial, &protocols, beta.as_deref()).await {
            Ok(socket) => {
                opened = Some((hop.member.route_id().to_owned(), socket));
                break;
            }
            Err(error) => last_error = Some(error),
        }
    }
    let Some((upstream, upstream_socket)) = opened else {
        drop(reservation);
        if !saw_endpoint {
            return text_response(
                StatusCode::BAD_REQUEST,
                &format!("model `{model}` has no realtime endpoint"),
            );
        }
        let (status, message) = last_error.unwrap_or((
            StatusCode::BAD_GATEWAY,
            "could not open the realtime session",
        ));
        return text_response(status, message);
    };

    let (mut parts, _) = request.into_parts();
    let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
        Ok(upgrade) => upgrade,
        Err(error) => return error.into_response(),
    };
    let app = Arc::clone(&app);
    upgrade
        .max_message_size(FRAME_LIMIT)
        .max_frame_size(FRAME_LIMIT)
        .protocols(protocols)
        .on_upgrade(move |socket| {
            serve(
                app,
                identity,
                model,
                upstream,
                reservation,
                socket,
                upstream_socket,
            )
        })
}

fn several_providers(asked: &str, candidates: &Candidates) -> bool {
    if asked.starts_with("group/") || candidates.route.starts_with("group/") {
        return true;
    }
    let providers: BTreeSet<&str> = candidates
        .hops
        .iter()
        .map(|hop| hop.member.route_id())
        .collect();
    providers.len() > 1
}

fn upgrade_required() -> Response {
    Response::builder()
        .status(StatusCode::UPGRADE_REQUIRED)
        .header(header::UPGRADE, "websocket")
        .header(header::CONNECTION, "Upgrade")
        .body(Body::from(
            "the Realtime API is a WebSocket: open GET /v1/realtime?model=<model> with Upgrade: websocket",
        ))
        .unwrap_or_else(|_| text_response(StatusCode::UPGRADE_REQUIRED, "websocket upgrade required"))
}

async fn dial_upstream(
    dial: &RealtimeDial,
    protocols: &[String],
    beta: Option<&str>,
) -> Result<UpstreamSocket, (StatusCode, &'static str)> {
    let url = websocket_url(&dial.url).map_err(|()| {
        (
            StatusCode::BAD_REQUEST,
            "the realtime endpoint is not a WebSocket URL",
        )
    })?;
    let mut request = url.into_client_request().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            "the realtime endpoint is not a WebSocket URL",
        )
    })?;
    for (name, value) in &dial.headers {
        insert_header(request.headers_mut(), name, value);
    }
    if !protocols.is_empty() {
        insert_header(
            request.headers_mut(),
            header::SEC_WEBSOCKET_PROTOCOL.as_str(),
            &protocols.join(", "),
        );
    }
    if let Some(beta) = beta.filter(|value| !value.is_empty() && value.len() <= 256) {
        insert_header(request.headers_mut(), BETA_HEADER, beta);
    }
    let mut config = WebSocketConfig::default();
    config.max_message_size = Some(FRAME_LIMIT);
    config.max_frame_size = Some(FRAME_LIMIT);
    let dial = tokio::time::timeout(
        DIAL_TIMEOUT,
        connect_async_with_config(request, Some(config), true),
    )
    .await;
    match dial {
        Ok(Ok((socket, _))) => Ok(socket),
        Ok(Err(error)) => Err(dial_status(&error)),
        Err(_) => Err((
            StatusCode::GATEWAY_TIMEOUT,
            "the realtime session did not open in time",
        )),
    }
}

fn dial_status(error: &tungstenite::Error) -> (StatusCode, &'static str) {
    if let tungstenite::Error::Http(response) = error {
        let status =
            StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        if status == StatusCode::NOT_FOUND || status == StatusCode::METHOD_NOT_ALLOWED {
            return (status, "the provider may not serve the Realtime API");
        }
        return (status, "the provider refused the realtime session");
    }
    (
        StatusCode::BAD_GATEWAY,
        "could not open the realtime session",
    )
}

fn websocket_url(url: &str) -> Result<String, ()> {
    let url = if let Some(rest) = url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        url.to_owned()
    };
    if url.starts_with("ws://") || url.starts_with("wss://") {
        Ok(url)
    } else {
        Err(())
    }
}

fn insert_header(headers: &mut HeaderMap, name: &str, value: &str) {
    let Ok(name) = header::HeaderName::try_from(name) else {
        return;
    };
    let Ok(value) = HeaderValue::from_str(value) else {
        return;
    };
    headers.insert(name, value);
}

fn header_text(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

fn forwarded_protocols(header: Option<&HeaderValue>) -> Vec<String> {
    header
        .and_then(|value| value.to_str().ok())
        .into_iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|name| !name.is_empty() && name.len() <= 128)
        .filter(|name| {
            !name.contains("insecure-api-key")
                && !name.contains("organization.")
                && !name.contains("project.")
        })
        .take(8)
        .map(str::to_owned)
        .collect()
}

fn query_model(uri: &Uri) -> Option<String> {
    let query = uri.query()?;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key == "model" {
            return Some(percent_decode(value));
        }
    }
    None
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        if bytes[index] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[index]);
        }
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[allow(clippy::too_many_arguments)]
async fn serve(
    app: Arc<Inner>,
    identity: Identity,
    model: String,
    upstream_id: String,
    reservation: Option<super::limits::Reservation>,
    mut client: WebSocket,
    upstream: UpstreamSocket,
) {
    let Some((ticket, mut closing)) = app.realtimes.register() else {
        let _ = client
            .send(Message::Close(Some(close_frame(
                close_code::AWAY,
                "gateway is shutting down",
            ))))
            .await;
        return;
    };
    let _ticket = TicketGuard {
        registry: Arc::clone(&app.realtimes),
        id: ticket,
    };
    let mut reservation = reservation;
    let (mut client_write, mut client_read) = client.split();
    let (mut upstream_write, mut upstream_read) = upstream.split();
    let idle = tokio::time::sleep(IDLE);
    tokio::pin!(idle);
    let mut tick = tokio::time::interval(KEY_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        if *closing.borrow() {
            reservation.take();
            let _ = client_write
                .send(Message::Close(Some(close_frame(
                    close_code::AWAY,
                    "gateway is shutting down",
                ))))
                .await;
            break;
        }
        tokio::select! {
            biased;
            changed = closing.changed() => {
                if changed.is_err() || *closing.borrow() {
                    reservation.take();
                    let _ = client_write
                        .send(Message::Close(Some(close_frame(
                            close_code::AWAY,
                            "gateway is shutting down",
                        ))))
                        .await;
                    break;
                }
            }
            _ = tick.tick() => {
                if let Some(reason) = key_close(&app, &identity) {
                    reservation.take();
                    let _ = client_write
                        .send(Message::Close(Some(close_frame(close_code::POLICY, reason))))
                        .await;
                    break;
                }
            }
            message = client_read.next() => {
                idle.as_mut().reset(tokio::time::Instant::now() + IDLE);
                match message {
                    Some(Ok(Message::Close(frame))) => {
                        reservation.take();
                        let _ = upstream_write.send(close_message(frame)).await;
                        break;
                    }
                    Some(Ok(Message::Ping(data))) => {
                        if client_write.send(Message::Pong(data)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(message)) => {
                        if upstream_write.send(to_upstream(message)).await.is_err() {
                            break;
                        }
                    }
                    Some(Err(_)) | None => break,
                }
            }
            message = upstream_read.next() => {
                idle.as_mut().reset(tokio::time::Instant::now() + IDLE);
                match message {
                    Some(Ok(UpstreamMessage::Close(frame))) => {
                        reservation.take();
                        let _ = client_write.send(Message::Close(frame.map(to_client_close))).await;
                        break;
                    }
                    Some(Ok(UpstreamMessage::Ping(data))) => {
                        if upstream_write.send(UpstreamMessage::Pong(data)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(UpstreamMessage::Pong(_))) => {}
                    Some(Ok(message)) => {
                        if let UpstreamMessage::Text(text) = &message {
                            note_done(&app, &identity, &model, &upstream_id, text);
                        }
                        if client_write.send(to_client(message)).await.is_err() {
                            break;
                        }
                    }
                    Some(Err(_)) | None => break,
                }
            }
            () = &mut idle => {
                reservation.take();
                let _ = client_write
                    .send(Message::Close(Some(close_frame(close_code::NORMAL, "idle"))))
                    .await;
                break;
            }
        }
    }
    reservation.take();
}

struct TicketGuard {
    registry: Arc<Registry>,
    id: u64,
}

impl Drop for TicketGuard {
    fn drop(&mut self) {
        self.registry.finish(self.id);
    }
}

fn close_frame(code: u16, reason: &str) -> CloseFrame {
    CloseFrame {
        code,
        reason: trim_reason(reason),
    }
}

fn close_message(frame: Option<CloseFrame>) -> UpstreamMessage {
    let frame = frame.unwrap_or_else(|| close_frame(close_code::NORMAL, ""));
    UpstreamMessage::Close(Some(tokio_tungstenite::tungstenite::protocol::CloseFrame {
        code: frame.code.into(),
        reason: frame.reason.to_string().into(),
    }))
}

fn to_upstream(message: Message) -> UpstreamMessage {
    match message {
        Message::Text(text) => UpstreamMessage::Text(text.to_string().into()),
        Message::Binary(data) => UpstreamMessage::Binary(data.to_vec().into()),
        Message::Ping(data) => UpstreamMessage::Ping(data.to_vec().into()),
        Message::Pong(data) => UpstreamMessage::Pong(data.to_vec().into()),
        Message::Close(frame) => close_message(frame),
    }
}

fn to_client(message: UpstreamMessage) -> Message {
    match message {
        UpstreamMessage::Text(text) => Message::Text(text.to_string().into()),
        UpstreamMessage::Binary(data) => Message::Binary(data.to_vec().into()),
        UpstreamMessage::Ping(data) => Message::Ping(data.to_vec().into()),
        UpstreamMessage::Pong(data) => Message::Pong(data.to_vec().into()),
        UpstreamMessage::Close(frame) => Message::Close(frame.map(to_client_close)),
        UpstreamMessage::Frame(_) => Message::Close(None),
    }
}

fn to_client_close(frame: tokio_tungstenite::tungstenite::protocol::CloseFrame) -> CloseFrame {
    CloseFrame {
        code: frame.code.into(),
        reason: trim_reason(frame.reason.as_str()),
    }
}

fn trim_reason(reason: &str) -> Utf8Bytes {
    let mut end = reason.len().min(123);
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    Utf8Bytes::from(&reason[..end])
}

fn note_done(app: &Inner, identity: &Identity, model: &str, upstream: &str, text: &str) {
    if text.len() > USAGE_PARSE_LIMIT {
        return;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return;
    };
    if value.get("type").and_then(serde_json::Value::as_str) != Some("response.done") {
        return;
    }
    let Some(usage) = value.pointer("/response/usage") else {
        return;
    };
    let input = json_tokens(usage.get("input_tokens"));
    let output = json_tokens(usage.get("output_tokens"));
    let cached = json_tokens(
        usage
            .pointer("/input_token_details/cached_tokens")
            .or_else(|| usage.pointer("/input_tokens_details/cached_tokens")),
    );
    super::record_realtime_usage(app, identity, model, upstream, input, output, cached);
}

fn json_tokens(value: Option<&serde_json::Value>) -> Option<i64> {
    let value = value?;
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|number| i64::try_from(number).ok()))
}

fn key_close(app: &Inner, identity: &Identity) -> Option<&'static str> {
    let enabled = if identity.key_id.is_empty() {
        None
    } else {
        app.keys
            .as_ref()
            .and_then(|keys| keys.on(&identity.key_id).ok())
    };
    let spent = app
        .budget
        .status(identity, chrono::Local::now())
        .ok()
        .is_some_and(|statuses| statuses.iter().any(|status| status.spent));
    close_reason(enabled, spent)
}

fn close_reason(enabled: Option<bool>, spent: bool) -> Option<&'static str> {
    if enabled == Some(false) {
        return Some("the gateway key this session was opened with is no longer on");
    }
    if spent {
        return Some("the gateway key this session was opened with can no longer spend");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::close_reason;

    #[test]
    fn d230_close_reason_names_a_disabled_or_spent_key() {
        assert_eq!(
            close_reason(Some(false), false),
            Some("the gateway key this session was opened with is no longer on")
        );
        assert_eq!(
            close_reason(Some(true), true),
            Some("the gateway key this session was opened with can no longer spend")
        );
        assert_eq!(close_reason(Some(true), false), None);
        assert_eq!(close_reason(None, false), None);
    }
}
