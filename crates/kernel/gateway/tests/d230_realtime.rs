//! Realtime WebSocket passthrough against a local fake upstream.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::IntoResponse;
use axum::routing::get;
use futures_util::{FutureExt, SinkExt, StreamExt};
use hofund_gateway::{Gateway, NetworkAccess};
use hofund_plugin_api::{MemberId, MemberKey, PluginError, PresetId};
use hofund_router::{FailoverPolicy, Hop, Member, RealtimeDial, Router, Snapshot};
use hofund_secrets::{KeyLimit, KeyStore};
use hofund_usage::{Ledger, Row};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{Error as SocketError, Message as ClientMessage};
use tower::ServiceExt;

struct Voice {
    member_id: MemberId,
    route_id: &'static str,
    serves: bool,
    dials: Arc<AtomicUsize>,
    upstream: SocketAddr,
    proxy: Option<String>,
    secure: bool,
}

#[async_trait]
impl Member for Voice {
    fn id(&self) -> &MemberId {
        &self.member_id
    }

    fn route_id(&self) -> &str {
        self.route_id
    }

    fn protocol(&self) -> &str {
        "plain"
    }

    fn realtime(&self, model: &str) -> Option<RealtimeDial> {
        if !self.serves {
            return None;
        }
        self.dials.fetch_add(1, Ordering::SeqCst);
        let scheme = if self.secure { "wss" } else { "ws" };
        Some(RealtimeDial {
            url: format!("{scheme}://{}/realtime?model={model}", self.upstream),
            headers: vec![("authorization".to_owned(), "Bearer plan-key".to_owned())],
            proxy: self.proxy.clone(),
        })
    }

    async fn execute(
        &self,
        _request: hofund_ir::Request,
        _inbound: &str,
    ) -> Result<hofund_router::Completion, PluginError> {
        Err(PluginError::unsupported("not used"))
    }
}

struct Seen {
    hits: usize,
    authorization: String,
    protocols: String,
    beta: String,
    uri: String,
    closed: bool,
}

struct Upstream {
    seen: Mutex<Seen>,
    closed: Notify,
}

impl Upstream {
    fn new() -> Self {
        Self {
            seen: Mutex::new(Seen {
                hits: 0,
                authorization: String::new(),
                protocols: String::new(),
                beta: String::new(),
                uri: String::new(),
                closed: false,
            }),
            closed: Notify::new(),
        }
    }

    fn note(&self, headers: &HeaderMap, uri: &Uri) {
        let mut seen = self.seen.lock().expect("seen");
        seen.hits += 1;
        seen.authorization = header_text(headers, "authorization");
        seen.protocols = header_text(headers, "sec-websocket-protocol");
        seen.beta = header_text(headers, hofund_plugin_api::responses_ws::BETA_HEADER);
        seen.uri = uri.to_string();
    }

    fn mark_closed(&self) {
        self.seen.lock().expect("seen").closed = true;
        self.closed.notify_waiters();
    }
}

fn header_text(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_owned()
}

async fn upstream_route(
    State(upstream): State<Arc<Upstream>>,
    headers: HeaderMap,
    uri: Uri,
    upgrade: WebSocketUpgrade,
) -> impl IntoResponse {
    upstream.note(&headers, &uri);
    let upstream = Arc::clone(&upstream);
    // The dial offers only the forwarded protocol. The handshake fails closed
    // unless this server selects it.
    upgrade
        .protocols(["chat"])
        .on_upgrade(move |socket| echo(socket, upstream))
}

async fn echo(socket: WebSocket, upstream: Arc<Upstream>) {
    let (mut write, mut read) = socket.split();
    while let Some(message) = read.next().await {
        match message {
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(Message::Ping(data)) => {
                if write.send(Message::Pong(data)).await.is_err() {
                    break;
                }
            }
            Ok(Message::Pong(_)) => {}
            Ok(message) => {
                if write.send(message).await.is_err() {
                    break;
                }
            }
        }
    }
    upstream.mark_closed();
}

async fn serve_upstream() -> (SocketAddr, Arc<Upstream>) {
    let upstream = Arc::new(Upstream::new());
    let app = axum::Router::new()
        .route("/realtime", get(upstream_route))
        .with_state(Arc::clone(&upstream));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("upstream bind");
    let address = listener.local_addr().expect("upstream address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (address, upstream)
}

struct Rig {
    app: axum::Router,
    store: Arc<KeyStore>,
    secret: String,
    limited: String,
    usage: Arc<Ledger>,
    dials: Arc<AtomicUsize>,
    upstream: Arc<Upstream>,
    _dir: tempfile::TempDir,
}

#[derive(Default)]
struct Extra {
    proxy: Option<String>,
    secure: bool,
    wire: Option<String>,
    address: Option<SocketAddr>,
    pricer: Option<Arc<hofund_model_catalog::EffectivePricer>>,
}

async fn rig(serves: bool, group: bool) -> Rig {
    rig_cap(serves, group, 1_000_000).await
}

async fn rig_cap(serves: bool, group: bool, tokens: u64) -> Rig {
    rig_extra(serves, group, tokens, Extra::default()).await
}

async fn rig_extra(serves: bool, group: bool, tokens: u64, extra: Extra) -> Rig {
    let (address, upstream) = if let Some(address) = extra.address {
        (address, Arc::new(Upstream::new()))
    } else {
        serve_upstream().await
    };
    let dir = tempfile::tempdir().expect("dir");
    let store = Arc::new(KeyStore::beside_token(dir.path().join("hofund.token")));
    store.list().expect("migration");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let dials = Arc::new(AtomicUsize::new(0));
    let proxy = extra.proxy.clone();
    let secure = extra.secure;
    let wire = extra.wire.clone();
    let pricer = extra.pricer;
    let member = |route_id: &'static str| {
        Arc::new(Voice {
            member_id: MemberId::from_preset(
                PresetId::try_from(route_id).expect("preset"),
                MemberKey::from_static("row"),
            ),
            route_id,
            serves,
            dials: Arc::clone(&dials),
            upstream: address,
            proxy: proxy.clone(),
            secure,
        }) as Arc<dyn Member>
    };
    let hops = if group {
        vec![Hop::new(member("alpha")), Hop::new(member("beta"))]
    } else {
        let mut hop = Hop::new(member("plan"));
        hop.wire_model = wire;
        vec![hop]
    };
    let routes = std::collections::BTreeMap::from([("voice".to_owned(), hops)]);
    let secret = store.token_file().read().expect("token");
    let added = store
        .add("Capped", Some("limited-secret-123"))
        .expect("add");
    store
        .set_limit(
            &added.key.id,
            Some(KeyLimit {
                period: "day".to_owned(),
                tokens,
                ..KeyLimit::default()
            }),
            None,
            100,
        )
        .expect("limit");
    let mut gateway = Gateway::new(
        Snapshot::new(Router::from_hops(routes, FailoverPolicy::default())),
        secret.clone(),
        std::collections::HashMap::new(),
        Arc::clone(&usage),
    )
    .with_key_store(Arc::clone(&store));
    if let Some(pricer) = pricer {
        gateway = gateway.with_effective_pricer(pricer);
    }
    Rig {
        app: gateway.into_router(),
        store,
        secret,
        limited: added.secret,
        usage,
        dials,
        upstream,
        _dir: dir,
    }
}

fn refused(
    result: Result<
        tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
        SocketError,
    >,
) -> u16 {
    match result {
        Ok(_) => panic!("the upgrade was accepted"),
        Err(SocketError::Http(response)) => response.status().as_u16(),
        Err(_) => panic!("the upgrade failed before an HTTP status"),
    }
}

async fn connect(
    address: SocketAddr,
    path: &str,
    secret: Option<&str>,
    origin: Option<&str>,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
    SocketError,
> {
    let mut request = format!("ws://{address}{path}")
        .into_client_request()
        .expect("request");
    if let Some(secret) = secret {
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {secret}").parse().expect("authorization"),
        );
    }
    if let Some(origin) = origin {
        request
            .headers_mut()
            .insert("origin", origin.parse().expect("origin"));
    }
    request.headers_mut().insert(
        "sec-websocket-protocol",
        "insecure-api-key.sample, chat".parse().expect("protocol"),
    );
    request.headers_mut().insert(
        hofund_plugin_api::responses_ws::BETA_HEADER,
        "session".parse().expect("beta"),
    );
    connect_async(request).await.map(|(socket, _)| socket)
}

async fn http(address: SocketAddr, path: &str, secret: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(address).await.expect("connect");
    let request = format!(
        "GET {path} HTTP/1.1\r\nhost: {address}\r\nauthorization: Bearer {secret}\r\nconnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).await.expect("read");
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .unwrap_or("0")
        .parse()
        .unwrap_or(0);
    (status, text)
}

async fn oneshot(
    app: axum::Router,
    path: &str,
    secret: Option<&str>,
) -> (StatusCode, HeaderMap, String) {
    let mut builder = Request::builder().uri(path);
    if let Some(secret) = secret {
        builder = builder.header("authorization", format!("Bearer {secret}"));
    }
    let response = app
        .oneshot(builder.body(Body::empty()).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    (status, headers, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test]
async fn d230_upgrade_requires_a_gateway_key() {
    let rig = rig(true, false).await;
    let (status, _, body) = oneshot(rig.app.clone(), "/v1/realtime?model=voice", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(rig.dials.load(Ordering::SeqCst), 0);
    assert_eq!(rig.upstream.seen.lock().expect("seen").hits, 0);
}

#[tokio::test]
async fn d230_plain_get_requires_an_upgrade() {
    let rig = rig(true, false).await;
    let (status, headers, body) = oneshot(
        rig.app.clone(),
        "/v1/realtime?model=voice",
        Some(&rig.secret),
    )
    .await;
    assert_eq!(status, StatusCode::UPGRADE_REQUIRED, "{body}");
    assert_eq!(
        headers.get("upgrade").and_then(|value| value.to_str().ok()),
        Some("websocket")
    );
    assert_eq!(rig.upstream.seen.lock().expect("seen").hits, 0);
}

#[tokio::test]
async fn d230_frames_relay_both_ways() {
    let rig = rig(true, false).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    let app = rig.app.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let mut socket = connect(address, "/v1/realtime?model=voice", Some(&rig.secret), None)
        .await
        .expect("upgrade");
    socket
        .send(ClientMessage::Text("hello".into()))
        .await
        .expect("text");
    let echoed = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("text arrived")
        .expect("text frame")
        .expect("text ok");
    assert_eq!(echoed, ClientMessage::Text("hello".into()));
    socket
        .send(ClientMessage::Binary(vec![1, 2, 3].into()))
        .await
        .expect("binary");
    let echoed = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("binary arrived")
        .expect("binary frame")
        .expect("binary ok");
    assert_eq!(echoed, ClientMessage::Binary(vec![1, 2, 3].into()));
    let done = r#"{"type":"response.done","response":{"usage":{"input_tokens":10,"output_tokens":2,"input_token_details":{"cached_tokens":4}}}}"#;
    socket
        .send(ClientMessage::Text(done.into()))
        .await
        .expect("done");
    let echoed = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("done arrived")
        .expect("done frame")
        .expect("done ok");
    assert!(matches!(echoed, ClientMessage::Text(_)));
    let seen = {
        let seen = rig.upstream.seen.lock().expect("seen");
        (
            seen.hits,
            seen.authorization.clone(),
            seen.protocols.clone(),
            seen.beta.clone(),
            seen.uri.clone(),
        )
    };
    assert_eq!(seen.0, 1);
    assert_eq!(seen.1, "Bearer plan-key");
    assert_eq!(seen.2, "chat");
    assert!(!seen.2.contains("sample"));
    assert_eq!(seen.3, "session");
    assert!(seen.4.contains("model=voice"));
    assert!(!seen.4.contains("model=voice&model"));
    drop(socket);

    let mut alias = connect(address, "/realtime?model=voice", Some(&rig.secret), None)
        .await
        .expect("alias");
    alias
        .send(ClientMessage::Text("alias".into()))
        .await
        .expect("alias text");
    let echoed = tokio::time::timeout(Duration::from_secs(5), alias.next())
        .await
        .expect("alias arrived")
        .expect("alias frame")
        .expect("alias ok");
    assert_eq!(echoed, ClientMessage::Text("alias".into()));
    drop(alias);

    let rows = rig.usage.rows().expect("rows");
    let row = rows
        .iter()
        .find(|row| row.kind == "realtime")
        .expect("usage");
    assert_eq!(row.model, "voice");
    assert_eq!(row.input_tokens, Some(6));
    assert_eq!(row.output_tokens, Some(2));
    assert_eq!(row.cache_read_tokens, Some(4));
    assert_eq!(row.endpoint, "/v1/realtime");
}

#[tokio::test]
async fn d230_slot_held_then_released() {
    let rig = rig(true, false).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    let app = rig.app.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let mut socket = connect(
        address,
        "/v1/realtime?model=voice",
        Some(&rig.limited),
        None,
    )
    .await
    .expect("upgrade");
    socket
        .send(ClientMessage::Text("held".into()))
        .await
        .expect("text");
    let _ = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("echo");
    let (status, body) = http(address, "/v1/hofund/limit", &rig.limited).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"inFlight\":1"), "{body}");
    socket
        .send(ClientMessage::Close(None))
        .await
        .expect("close");
    let notified = rig.upstream.closed.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    let closed = rig.upstream.seen.lock().expect("seen").closed;
    if !closed {
        tokio::time::timeout(Duration::from_secs(5), notified)
            .await
            .expect("upstream closed");
    }
    let (status, body) = http(address, "/v1/hofund/limit", &rig.limited).await;
    assert_eq!(status, 200, "{body}");
    assert!(!body.contains("\"inFlight\""), "{body}");
}

#[tokio::test]
async fn d230_unknown_model_is_a_clean_error() {
    let rig = rig(true, false).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    let app = rig.app.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let status = refused(
        connect(
            address,
            "/v1/realtime?model=missing",
            Some(&rig.secret),
            None,
        )
        .await,
    );
    assert_eq!(status, 404);
    assert_eq!(rig.dials.load(Ordering::SeqCst), 0);
    assert_eq!(rig.upstream.seen.lock().expect("seen").hits, 0);
    let status = refused(connect(address, "/v1/realtime", Some(&rig.secret), None).await);
    assert_eq!(status, 400);
}

#[tokio::test]
async fn d230_provider_without_realtime_endpoint_is_refused() {
    let rig = rig(false, false).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    let app = rig.app.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let status =
        refused(connect(address, "/v1/realtime?model=voice", Some(&rig.secret), None).await);
    assert_eq!(status, 400);
    assert_eq!(rig.dials.load(Ordering::SeqCst), 0);
    assert_eq!(rig.upstream.seen.lock().expect("seen").hits, 0);
}

#[tokio::test]
async fn d230_foreign_origin_is_refused() {
    let rig = rig(true, false).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    let app = rig.app.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let status = refused(
        connect(
            address,
            "/v1/realtime?model=voice",
            Some(&rig.secret),
            Some("https://other.example"),
        )
        .await,
    );
    assert_eq!(status, 403);
    assert_eq!(rig.upstream.seen.lock().expect("seen").hits, 0);
}

#[tokio::test]
async fn d230_a_routing_group_is_refused() {
    let rig = rig(true, true).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    let app = rig.app.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let status =
        refused(connect(address, "/v1/realtime?model=voice", Some(&rig.secret), None).await);
    assert_eq!(status, 400);
    assert_eq!(rig.dials.load(Ordering::SeqCst), 0);
    assert_eq!(rig.upstream.seen.lock().expect("seen").hits, 0);
}

#[tokio::test]
async fn d230_spent_key_is_refused_before_dial() {
    let rig = rig_cap(true, false, 1).await;
    rig.usage
        .record(&Row {
            created_unix: chrono::Local::now().timestamp(),
            model: "voice".to_owned(),
            upstream: "plan".to_owned(),
            input_tokens: Some(20),
            status: "200".to_owned(),
            caller_key_id: key_id(&rig),
            ..Row::default()
        })
        .expect("record");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    let app = rig.app.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let status = refused(
        connect(
            address,
            "/v1/realtime?model=voice",
            Some(&rig.limited),
            None,
        )
        .await,
    );
    assert_eq!(status, 429);
    assert_eq!(rig.dials.load(Ordering::SeqCst), 0);
    assert_eq!(rig.upstream.seen.lock().expect("seen").hits, 0);
}

fn key_id(rig: &Rig) -> String {
    rig.store
        .authenticate(&rig.limited)
        .expect("auth")
        .expect("identity")
        .key_id
}

#[tokio::test]
async fn d230_allow_list_refuses_a_missing_peer() {
    let (address, upstream) = serve_upstream().await;
    let dir = tempfile::tempdir().expect("dir");
    let store = Arc::new(KeyStore::beside_token(dir.path().join("hofund.token")));
    store.list().expect("migration");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let dials = Arc::new(AtomicUsize::new(0));
    let member = Arc::new(Voice {
        member_id: MemberId::from_preset(
            PresetId::try_from("plan").expect("preset"),
            MemberKey::from_static("row"),
        ),
        route_id: "plan",
        serves: true,
        dials: Arc::clone(&dials),
        upstream: address,
        proxy: None,
        secure: false,
    }) as Arc<dyn Member>;
    let routes = std::collections::BTreeMap::from([("voice".to_owned(), vec![Hop::new(member)])]);
    let secret = store.token_file().read().expect("token");
    let gateway = Gateway::new(
        Snapshot::new(Router::from_hops(routes, FailoverPolicy::default())),
        secret.clone(),
        std::collections::HashMap::new(),
        usage,
    )
    .with_key_store(store)
    .with_network_access(NetworkAccess::new(hofund_config::NetworkSettings {
        allow_ips: vec!["203.0.113.5".to_owned()],
        trust_proxy: false,
        invalid_allow_ips: false,
    }));
    let (status, _, _) = oneshot(
        gateway.into_router(),
        "/v1/realtime?model=voice",
        Some(&secret),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(dials.load(Ordering::SeqCst), 0);
    assert_eq!(upstream.seen.lock().expect("seen").hits, 0);
}

#[allow(clippy::result_large_err)]
fn select_protocol(
    request: &tokio_tungstenite::tungstenite::handshake::server::Request,
    mut response: tokio_tungstenite::tungstenite::handshake::server::Response,
) -> Result<
    tokio_tungstenite::tungstenite::handshake::server::Response,
    tokio_tungstenite::tungstenite::handshake::server::ErrorResponse,
> {
    if let Some(protocol) = request.headers().get("sec-websocket-protocol") {
        response.headers_mut().insert(
            tokio_tungstenite::tungstenite::http::header::SEC_WEBSOCKET_PROTOCOL,
            protocol.clone(),
        );
    }
    Ok(response)
}

async fn echo_socket<S>(mut socket: tokio_tungstenite::WebSocketStream<S>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    while let Some(message) = socket.next().await {
        match message {
            Ok(ClientMessage::Text(text)) => {
                if socket.send(ClientMessage::Text(text)).await.is_err() {
                    break;
                }
            }
            Ok(ClientMessage::Binary(data)) => {
                if socket.send(ClientMessage::Binary(data)).await.is_err() {
                    break;
                }
            }
            Ok(ClientMessage::Ping(data)) => {
                if socket.send(ClientMessage::Pong(data)).await.is_err() {
                    break;
                }
            }
            Ok(ClientMessage::Close(_)) | Err(_) => break,
            Ok(ClientMessage::Pong(_) | ClientMessage::Frame(_)) => {}
        }
    }
}

async fn serve_proxy(listener: tokio::net::TcpListener, seen: Arc<Mutex<String>>) {
    let Ok((mut stream, _)) = listener.accept().await else {
        return;
    };
    let mut buf = Vec::new();
    let mut tmp = [0_u8; 256];
    loop {
        let Ok(read) = stream.read(&mut tmp).await else {
            return;
        };
        if read == 0 {
            return;
        }
        buf.extend_from_slice(&tmp[..read]);
        if buf.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    *seen.lock().expect("seen") = String::from_utf8_lossy(&buf).into_owned();
    if stream
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await
        .is_err()
    {
        return;
    }
    let Ok(socket) = tokio_tungstenite::accept_hdr_async(stream, select_protocol).await else {
        return;
    };
    echo_socket(socket).await;
}

fn trap_accepted(listener: &tokio::net::TcpListener) -> bool {
    listener.accept().now_or_never().is_some()
}

#[tokio::test]
async fn d230_a_proxied_member_does_not_dial_the_upstream_directly() {
    let trap = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("trap");
    let trap_address = trap.local_addr().expect("trap address");
    let proxy = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("proxy");
    let proxy_address = proxy.local_addr().expect("proxy address");
    let seen = Arc::new(Mutex::new(String::new()));
    let recorded = Arc::clone(&seen);
    tokio::spawn(async move { serve_proxy(proxy, recorded).await });
    let rig = rig_extra(
        true,
        false,
        1_000_000,
        Extra {
            proxy: Some(format!("http://{proxy_address}")),
            address: Some(trap_address),
            ..Extra::default()
        },
    )
    .await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let gateway = listener.local_addr().expect("address");
    let app = rig.app.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let mut socket = connect(gateway, "/v1/realtime?model=voice", Some(&rig.secret), None)
        .await
        .expect("proxied upgrade");
    socket
        .send(ClientMessage::Text("via-proxy".into()))
        .await
        .expect("text");
    let echoed = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("echo")
        .expect("frame")
        .expect("message");
    assert!(matches!(echoed, ClientMessage::Text(text) if text == "via-proxy"));
    let request = seen.lock().expect("seen").clone();
    assert!(
        request.starts_with(&format!("CONNECT {trap_address} ")),
        "{request}"
    );
    assert!(
        !trap_accepted(&trap),
        "the upstream accepted a direct socket"
    );
}

#[tokio::test]
async fn d230_an_unusable_proxy_opens_no_direct_socket() {
    let trap = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("trap");
    let trap_address = trap.local_addr().expect("trap address");
    let rig = rig_extra(
        true,
        false,
        1_000_000,
        Extra {
            proxy: Some("http://127.0.0.1:1".to_owned()),
            address: Some(trap_address),
            ..Extra::default()
        },
    )
    .await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let gateway = listener.local_addr().expect("address");
    let app = rig.app.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    match connect(gateway, "/v1/realtime?model=voice", Some(&rig.secret), None).await {
        Err(SocketError::Http(response)) => {
            assert_eq!(response.status(), 502);
            let body = String::from_utf8_lossy(response.body().as_deref().unwrap_or_default());
            assert!(body.contains("proxy"), "{body}");
        }
        other => panic!("expected a proxy refusal, got {other:?}"),
    }
    assert!(
        !trap_accepted(&trap),
        "the upstream accepted a direct socket"
    );
}

struct OfflineCatalog;

impl hofund_model_catalog::CatalogFetcher for OfflineCatalog {
    fn fetch(
        &self,
        _url: &str,
        _if_none_match: Option<&str>,
    ) -> Result<hofund_model_catalog::FetchResponse, std::io::Error> {
        Err(std::io::Error::other("offline"))
    }
}

struct WirePrice;

impl hofund_model_catalog::ListPriceLookup for WirePrice {
    fn list_price(&self, provider: &str, model: &str) -> Option<hofund_model_catalog::Price> {
        (provider == "plan" && model == "billed-voice").then_some(hofund_model_catalog::Price {
            input: 1.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
            cache_write_1h: 0.0,
        })
    }
}

#[tokio::test]
async fn d230_usage_is_priced_on_the_hop_wire_id() {
    let catalog_dir = tempfile::tempdir().expect("catalog");
    let catalog = hofund_model_catalog::ModelCatalog::open(
        catalog_dir.path(),
        hofund_config::CatalogSettings::default(),
        Arc::new(OfflineCatalog),
    )
    .expect("catalog");
    let pricer = Arc::new(hofund_model_catalog::EffectivePricer::new(
        hofund_config::UsageSettings::default(),
        catalog,
        Arc::new(WirePrice),
    ));
    let rig = rig_extra(
        true,
        false,
        1_000_000,
        Extra {
            wire: Some("billed-voice".to_owned()),
            pricer: Some(pricer),
            ..Extra::default()
        },
    )
    .await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let gateway = listener.local_addr().expect("address");
    let app = rig.app.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let mut socket = connect(gateway, "/v1/realtime?model=voice", Some(&rig.secret), None)
        .await
        .expect("upgrade");
    let done = r#"{"type":"response.done","response":{"status":"completed","usage":{"input_tokens":1000000,"output_tokens":0}}}"#;
    socket
        .send(ClientMessage::Text(done.into()))
        .await
        .expect("done");
    let _ = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("echo");
    let row = rig.usage.rows().expect("rows").pop().expect("row");
    assert_eq!(row.model, "voice");
    assert!(row.priced, "the wire id was not priced");
    assert!((row.cost_usd.expect("cost") - 1.0).abs() < 1e-9);
}

#[tokio::test]
async fn d230_cached_tokens_are_clamped_and_a_failed_done_is_not_ok() {
    let rig = rig(true, false).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let gateway = listener.local_addr().expect("address");
    let app = rig.app.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let mut socket = connect(gateway, "/v1/realtime?model=voice", Some(&rig.secret), None)
        .await
        .expect("upgrade");
    let done = r#"{"type":"response.done","response":{"status":"failed","usage":{"input_tokens":10,"output_tokens":1,"input_token_details":{"cached_tokens":40}}}}"#;
    socket
        .send(ClientMessage::Text(done.into()))
        .await
        .expect("done");
    let _ = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("echo");
    let row = rig.usage.rows().expect("rows").pop().expect("row");
    assert_eq!(row.status, "500");
    assert_eq!(row.input_tokens, Some(0));
    assert_eq!(row.cache_read_tokens, Some(10));
    assert_eq!(row.output_tokens, Some(1));
}
