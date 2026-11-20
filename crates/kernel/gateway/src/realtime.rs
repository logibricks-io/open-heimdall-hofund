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
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use futures_util::{SinkExt, StreamExt};
use hofund_plugin_api::responses_ws::BETA_HEADER;
use hofund_router::{Candidates, RealtimeDial, RouterError};
use hofund_secrets::Identity;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message as UpstreamMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{client_async_tls_with_config, tungstenite};

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
                opened = Some((hop.member.route_id().to_owned(), wire, socket));
                break;
            }
            Err(error) => last_error = Some(error),
        }
    }
    let Some((upstream, wire, upstream_socket)) = opened else {
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
                wire,
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
    let target = request.uri().clone();
    let dial = tokio::time::timeout(DIAL_TIMEOUT, async {
        let stream = open_stream(&target, dial.proxy.as_deref()).await?;
        let _ = stream.set_nodelay(true);
        client_async_tls_with_config(request, stream, Some(config), tls_connector())
            .await
            .map_err(|error| dial_status(&error))
    })
    .await;
    match dial {
        Ok(Ok((socket, _))) => Ok(socket),
        Ok(Err(error)) => Err(error),
        Err(_) => Err((
            StatusCode::GATEWAY_TIMEOUT,
            "the realtime session did not open in time",
        )),
    }
}

/// Opens the TCP socket for `target`. A configured proxy is the only path;
/// a proxy that cannot be used returns before any connection to `target`.
async fn open_stream(
    target: &Uri,
    proxy: Option<&str>,
) -> Result<TcpStream, (StatusCode, &'static str)> {
    let host = target.host().ok_or((
        StatusCode::BAD_REQUEST,
        "the realtime endpoint is not a WebSocket URL",
    ))?;
    let port = target
        .port_u16()
        .unwrap_or(if target.scheme_str() == Some("wss") {
            443
        } else {
            80
        });
    match proxy {
        None => TcpStream::connect((host, port)).await.map_err(|_| {
            (
                StatusCode::BAD_GATEWAY,
                "could not open the realtime session",
            )
        }),
        Some(proxy) => connect_proxy(proxy, host, port).await,
    }
}

async fn connect_proxy(
    proxy: &str,
    host: &str,
    port: u16,
) -> Result<TcpStream, (StatusCode, &'static str)> {
    let proxy = http_proxy(proxy).map_err(|()| {
        (
            StatusCode::BAD_GATEWAY,
            "the realtime proxy could not be used",
        )
    })?;
    let mut stream = TcpStream::connect((proxy.host.as_str(), proxy.port))
        .await
        .map_err(|_| {
            (
                StatusCode::BAD_GATEWAY,
                "the realtime proxy could not be used",
            )
        })?;
    let authority = socket_authority(host, port);
    let mut request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
    if let Some(credentials) = proxy.credentials {
        let token = STANDARD.encode(credentials.as_bytes());
        request.push_str("Proxy-Authorization: Basic ");
        request.push_str(&token);
        request.push_str("\r\n");
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await.map_err(|_| {
        (
            StatusCode::BAD_GATEWAY,
            "the realtime proxy could not be used",
        )
    })?;
    if !proxy_accepted(&mut stream).await {
        return Err((
            StatusCode::BAD_GATEWAY,
            "the realtime proxy could not be used",
        ));
    }
    Ok(stream)
}

struct HttpProxy {
    host: String,
    port: u16,
    credentials: Option<String>,
}

fn http_proxy(proxy: &str) -> Result<HttpProxy, ()> {
    let proxy = proxy.trim();
    let Some((scheme, rest)) = proxy.split_once("://") else {
        return Err(());
    };
    if !scheme.eq_ignore_ascii_case("http") || rest.is_empty() {
        return Err(());
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    if authority.is_empty() {
        return Err(());
    }
    let (credentials, hostport) = match authority.rsplit_once('@') {
        Some((credentials, hostport)) if !credentials.is_empty() && !hostport.is_empty() => {
            (Some(credentials.to_owned()), hostport)
        }
        Some(_) => return Err(()),
        None => (None, authority),
    };
    let (host, port) = split_host_port(hostport, 80)?;
    Ok(HttpProxy {
        host,
        port,
        credentials,
    })
}

fn split_host_port(hostport: &str, default_port: u16) -> Result<(String, u16), ()> {
    if let Some(host) = hostport.strip_prefix('[') {
        let (host, rest) = host.split_once(']').ok_or(())?;
        if host.is_empty() {
            return Err(());
        }
        let port = if rest.is_empty() {
            default_port
        } else {
            rest.strip_prefix(':')
                .ok_or(())?
                .parse::<u16>()
                .map_err(|_| ())?
        };
        return Ok((host.to_owned(), port));
    }
    if let Some((host, port)) = hostport.rsplit_once(':') {
        if host.is_empty() || port.is_empty() {
            return Err(());
        }
        // A bare IPv6 address has more than one colon and no brackets.
        if host.contains(':') {
            return Err(());
        }
        return Ok((host.to_owned(), port.parse().map_err(|_| ())?));
    }
    if hostport.is_empty() {
        return Err(());
    }
    Ok((hostport.to_owned(), default_port))
}

fn socket_authority(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

async fn proxy_accepted(stream: &mut TcpStream) -> bool {
    let mut buf = Vec::new();
    let mut tmp = [0_u8; 256];
    let header_end = loop {
        if buf.len() > 8 * 1024 {
            return false;
        }
        let read = match stream.read(&mut tmp).await {
            Ok(0) | Err(_) => return false,
            Ok(read) => read,
        };
        buf.extend_from_slice(&tmp[..read]);
        if let Some(position) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
    };
    if header_end != buf.len() {
        return false;
    }
    let Ok(head) = std::str::from_utf8(&buf[..header_end]) else {
        return false;
    };
    head.split_whitespace().nth(1) == Some("200")
}

/// `None` uses the native root store that the `rustls-tls-native-roots` feature
/// installs. Unit tests add the local listener certificate because the process
/// trust store cannot be changed without an unsafe environment write.
fn tls_connector() -> Option<tokio_tungstenite::Connector> {
    #[cfg(not(test))]
    {
        None
    }
    #[cfg(test)]
    {
        Some(tokio_tungstenite::Connector::Rustls(test_roots()))
    }
}

#[cfg(test)]
fn test_roots() -> std::sync::Arc<rustls::ClientConfig> {
    use rustls::pki_types::CertificateDer;
    let mut roots = rustls::RootCertStore::empty();
    let cert = CertificateDer::from(include_bytes!("../testdata/realtime-ca.der").to_vec());
    roots.add(cert).expect("listener certificate");
    std::sync::Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
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
    wire_model: String,
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
                            note_done(&app, &identity, &model, &wire_model, &upstream_id, text);
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

fn note_done(
    app: &Inner,
    identity: &Identity,
    model: &str,
    wire_model: &str,
    upstream: &str,
    text: &str,
) {
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
    let cached = clamp_cached(
        input,
        json_tokens(
            usage
                .pointer("/input_token_details/cached_tokens")
                .or_else(|| usage.pointer("/input_tokens_details/cached_tokens")),
        ),
    );
    let status = if value
        .pointer("/response/status")
        .and_then(serde_json::Value::as_str)
        == Some("failed")
    {
        StatusCode::INTERNAL_SERVER_ERROR
    } else {
        StatusCode::OK
    };
    super::record_realtime_usage(
        app,
        identity,
        model,
        wire_model,
        upstream,
        super::RealtimeUsage {
            input,
            output,
            cache_read: cached,
            status,
        },
    );
}

/// Keeps a cache-read count inside the reported input so a larger cached
/// figure cannot overstate the read when that policy is on.
fn clamp_cached(input: Option<i64>, cached: Option<i64>) -> Option<i64> {
    let cached = cached?;
    let Some(input) = input else {
        return Some(cached.max(0));
    };
    Some(cached.clamp(0, input.max(0)))
}

fn json_tokens(value: Option<&serde_json::Value>) -> Option<i64> {
    let value = value?;
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|number| i64::try_from(number).ok()))
}

fn key_close(app: &Inner, identity: &Identity) -> Option<&'static str> {
    let key = if identity.key_id.is_empty() {
        KeyCheck::On
    } else if let Some(keys) = app.keys.as_ref() {
        match keys.on(&identity.key_id) {
            Ok(true) => KeyCheck::On,
            Ok(false) => KeyCheck::Off,
            Err(_) => KeyCheck::Error,
        }
    } else {
        KeyCheck::On
    };
    let caller = spend_of(app, identity);
    let issuer = if identity.issuer.is_empty() {
        SpendCheck::Open
    } else {
        let issued = Identity {
            key_id: identity.issuer.clone(),
            key_name: identity.issuer_name.clone(),
            limits: identity.issuer_limits.clone(),
            ..Identity::default()
        };
        spend_of(app, &issued)
    };
    close_reason(key, caller, issuer)
}

fn spend_of(app: &Inner, identity: &Identity) -> SpendCheck {
    match app.budget.status(identity, chrono::Local::now()) {
        Ok(statuses) if statuses.iter().any(|status| status.spent) => SpendCheck::Spent,
        Ok(_) => SpendCheck::Open,
        Err(_) => SpendCheck::Error,
    }
}

enum KeyCheck {
    On,
    Off,
    Error,
}

enum SpendCheck {
    Open,
    Spent,
    Error,
}

fn close_reason(key: KeyCheck, caller: SpendCheck, issuer: SpendCheck) -> Option<&'static str> {
    if matches!(key, KeyCheck::Off) {
        return Some("the gateway key this session was opened with is no longer on");
    }
    if matches!(caller, SpendCheck::Spent) || matches!(issuer, SpendCheck::Spent) {
        return Some("the gateway key this session was opened with can no longer spend");
    }
    if matches!(key, KeyCheck::Error)
        || matches!(caller, SpendCheck::Error)
        || matches!(issuer, SpendCheck::Error)
    {
        return Some("the gateway key this session was opened with could not be checked");
    }
    None
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures_util::{SinkExt, StreamExt};
    use rustls::ServerConfig;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use tokio_rustls::TlsAcceptor;
    use tokio_tungstenite::tungstenite::Message;

    use super::{KeyCheck, RealtimeDial, SpendCheck, close_reason, dial_upstream};

    #[tokio::test]
    async fn d230_wss_upgrade_reaches_a_local_listener() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        let cert = CertificateDer::from(include_bytes!("../testdata/realtime-leaf.der").to_vec());
        let key =
            PrivateKeyDer::try_from(include_bytes!("../testdata/realtime-leaf.key.der").to_vec())
                .expect("private key");
        let server_config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert], key)
            .expect("server config");
        let acceptor = TlsAcceptor::from(std::sync::Arc::new(server_config));
        tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(tls) = acceptor.accept(stream).await else {
                return;
            };
            let Ok(mut socket) = tokio_tungstenite::accept_async(tls).await else {
                return;
            };
            while let Some(message) = socket.next().await {
                match message {
                    Ok(Message::Text(text)) => {
                        if socket.send(Message::Text(text)).await.is_err() {
                            break;
                        }
                    }
                    Ok(Message::Close(_)) | Err(_) => break,
                    _ => {}
                }
            }
        });
        let dial = RealtimeDial {
            url: format!("wss://{address}/realtime?model=voice"),
            headers: Vec::new(),
            proxy: None,
        };
        let mut socket = dial_upstream(&dial, &[], None).await.expect("wss dial");
        socket
            .send(Message::Text("tls".into()))
            .await
            .expect("text");
        let echoed = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .expect("echo")
            .expect("frame")
            .expect("message");
        assert!(matches!(echoed, Message::Text(text) if text == "tls"));
    }

    #[test]
    fn d230_close_reason_names_a_disabled_or_spent_key() {
        assert_eq!(
            close_reason(KeyCheck::Off, SpendCheck::Open, SpendCheck::Open),
            Some("the gateway key this session was opened with is no longer on")
        );
        assert_eq!(
            close_reason(KeyCheck::On, SpendCheck::Spent, SpendCheck::Open),
            Some("the gateway key this session was opened with can no longer spend")
        );
        // The caller's own cap is still open. The issuer cap is spent.
        assert_eq!(
            close_reason(KeyCheck::On, SpendCheck::Open, SpendCheck::Spent),
            Some("the gateway key this session was opened with can no longer spend")
        );
        assert_eq!(
            close_reason(KeyCheck::Error, SpendCheck::Open, SpendCheck::Open),
            Some("the gateway key this session was opened with could not be checked")
        );
        assert_eq!(
            close_reason(KeyCheck::On, SpendCheck::Error, SpendCheck::Open),
            Some("the gateway key this session was opened with could not be checked")
        );
        assert_eq!(
            close_reason(KeyCheck::On, SpendCheck::Open, SpendCheck::Error),
            Some("the gateway key this session was opened with could not be checked")
        );
        assert_eq!(
            close_reason(KeyCheck::On, SpendCheck::Open, SpendCheck::Open),
            None
        );
    }
}
