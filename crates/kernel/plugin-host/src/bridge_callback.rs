//! Dedicated `127.0.0.1` listener for long-lived bridge callbacks.
//!
//! The kernel binds IPv4 loopback only, compares a request token against
//! every live registration in constant time, and never logs the request
//! target. Unknown, spent, or missing tokens receive a fixed 404 body.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use hofund_plugin_api::kernel::BridgeCallbackBackend;
use hofund_plugin_api::{
    BRIDGE_CALLBACK_PATH_PREFIX, BridgeCallbackHandler, PluginError, PluginKey,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::sync::oneshot;

/// How long a client gets to send its request head.
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
/// The largest request head the listener reads.
const MAX_HEAD: usize = 8 * 1024;
/// Largest accepted POST body, matching the design (16 MiB).
const MAX_BODY: usize = 16 * 1024 * 1024;
/// Fixed 404 body. It must never include the presented token.
const UNKNOWN: &[u8] = b"unknown or expired bridge run";

struct Registration {
    plugin: PluginKey,
    token: String,
    handler: Arc<dyn BridgeCallbackHandler>,
}

struct ListenerState {
    port: u16,
    shutdown: Option<oneshot::Sender<()>>,
}

struct Shared {
    listener: Mutex<Option<ListenerState>>,
    registrations: Mutex<Vec<Registration>>,
}

impl Drop for Shared {
    fn drop(&mut self) {
        if let Some(state) = self
            .listener
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            drop(state.shutdown);
        }
    }
}

/// The kernel's dedicated bridge callback listener.
///
/// Give it to a context with [`hofund_plugin_api::HostContext::with_bridge_callback`].
/// The listener starts on the first [`Self::base_url`] or register call and
/// stays up until this registry is dropped.
pub struct BridgeCallbackRegistry {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for BridgeCallbackRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self
            .shared
            .registrations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        formatter
            .debug_struct("BridgeCallbackRegistry")
            .field("registrations", &count)
            .finish()
    }
}

impl Default for BridgeCallbackRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl BridgeCallbackRegistry {
    /// An empty registry. The listener starts on first use.
    #[must_use]
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Shared {
                listener: Mutex::new(None),
                registrations: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Port of the running listener, if it has started.
    #[cfg(test)]
    fn port(&self) -> Option<u16> {
        self.shared
            .listener
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|state| state.port)
    }

    fn ensure_listening(&self) -> Result<u16, PluginError> {
        let mut slot = self
            .shared
            .listener
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(state) = slot.as_ref() {
            return Ok(state.port);
        }
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| PluginError::internal("bridge callback listener needs a tokio runtime"))?;
        let listener = {
            let _guard = runtime.enter();
            listen(0)
                .map_err(|error| PluginError::io("bridge callback listener").with_source(error))?
        };
        let port = listener
            .local_addr()
            .map_err(|error| {
                PluginError::io("bridge callback listener address").with_source(error)
            })?
            .port();
        let (shutdown, shutdown_rx) = oneshot::channel();
        let shared = Arc::clone(&self.shared);
        runtime.spawn(serve(listener, shared, shutdown_rx));
        *slot = Some(ListenerState {
            port,
            shutdown: Some(shutdown),
        });
        Ok(port)
    }
}

impl BridgeCallbackBackend for BridgeCallbackRegistry {
    fn base_url(&self) -> Result<String, PluginError> {
        let port = self.ensure_listening()?;
        Ok(format!("http://127.0.0.1:{port}"))
    }

    fn register(
        &self,
        plugin: &PluginKey,
        token: &str,
        handler: Arc<dyn BridgeCallbackHandler>,
    ) -> Result<String, PluginError> {
        let port = self.ensure_listening()?;
        let mut live = self
            .shared
            .registrations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if live
            .iter()
            .any(|entry| constant_time_eq(entry.token.as_bytes(), token.as_bytes()))
        {
            return Err(PluginError::config(
                "bridge callback token is already registered",
            ));
        }
        live.push(Registration {
            plugin: plugin.clone(),
            token: token.to_owned(),
            handler,
        });
        Ok(format!(
            "http://127.0.0.1:{port}{BRIDGE_CALLBACK_PATH_PREFIX}{token}"
        ))
    }

    fn unregister(&self, plugin: &PluginKey, token: &str) {
        let mut live = self
            .shared
            .registrations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        live.retain(|entry| {
            !(plugin == &entry.plugin && constant_time_eq(entry.token.as_bytes(), token.as_bytes()))
        });
    }
}

/// Compares `provided` against every live token. Every entry is visited;
/// the first constant-time match wins only after the rest have been compared.
fn find_handler(live: &[Registration], provided: &str) -> Option<Arc<dyn BridgeCallbackHandler>> {
    let mut found = None;
    let mut compared = 0;
    for entry in live {
        compared += 1;
        if constant_time_eq(provided.as_bytes(), entry.token.as_bytes()) {
            found = Some(Arc::clone(&entry.handler));
        }
    }
    debug_assert_eq!(compared, live.len());
    let _ = compared;
    found
}

/// Constant-time equality that also folds in a length mismatch.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut diff =
        left.len().wrapping_sub(right.len()) as u8 | right.len().wrapping_sub(left.len()) as u8;
    let n = left.len().min(right.len());
    for index in 0..n {
        diff |= left[index] ^ right[index];
    }
    diff == 0
}

/// Binds `127.0.0.1:port`. Unix sets `SO_REUSEADDR` so a port left in
/// `TIME_WAIT` binds again; Windows does not, because there it would let
/// two listeners share the port.
fn listen(port: u16) -> std::io::Result<TcpListener> {
    let socket = TcpSocket::new_v4()?;
    #[cfg(unix)]
    socket.set_reuseaddr(true)?;
    socket.bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))?;
    socket.listen(128)
}

async fn serve(listener: TcpListener, shared: Arc<Shared>, mut shutdown: oneshot::Receiver<()>) {
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => {
                if let Ok((stream, _)) = accepted {
                    let shared = Arc::clone(&shared);
                    tokio::spawn(answer(stream, shared));
                }
            }
        }
    }
}

async fn answer(mut stream: TcpStream, shared: Arc<Shared>) {
    let request = match tokio::time::timeout(HEADER_TIMEOUT, read_request(&mut stream)).await {
        Ok(Ok(request)) => request,
        Ok(Err(status)) => {
            let _ = respond(&mut stream, status, content_for(status)).await;
            return;
        }
        Err(_) => {
            let _ = respond(&mut stream, 400, UNKNOWN).await;
            return;
        }
    };
    let token = token_from_path(&request.path);
    let handler = {
        let live = shared
            .registrations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        token.and_then(|token| find_handler(&live, token))
    };
    let (status, body) = match handler {
        Some(handler) if request.method == Method::Post => {
            let reply = handler.handle(request.body).await;
            (reply.status(), Bytes::copy_from_slice(reply.body()))
        }
        _ => (404, Bytes::from_static(UNKNOWN)),
    };
    let _ = respond(&mut stream, status, &body).await;
}

struct Incoming {
    method: Method,
    path: String,
    body: Bytes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Method {
    Post,
    Other,
}

fn token_from_path(path: &str) -> Option<&str> {
    let path = path.split_once('?').map_or(path, |(path, _)| path);
    let token = path.strip_prefix(BRIDGE_CALLBACK_PATH_PREFIX)?;
    if token.is_empty() || token.contains('/') {
        return None;
    }
    Some(token)
}

fn content_for(status: u16) -> &'static [u8] {
    match status {
        404 => UNKNOWN,
        413 => b"payload too large",
        _ => b"bad request",
    }
}

async fn read_request(stream: &mut TcpStream) -> Result<Incoming, u16> {
    let mut head = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];
    let header_end;
    loop {
        if head.len() >= MAX_HEAD {
            return Err(400);
        }
        let read = stream.read(&mut chunk).await.map_err(|_| 400_u16)?;
        if read == 0 {
            return Err(400);
        }
        head.extend_from_slice(&chunk[..read]);
        if let Some(index) = find_header_end(&head) {
            header_end = index;
            break;
        }
    }
    let (title, headers, rest) = split_head(&head, header_end).ok_or(400_u16)?;
    let (method_str, path, version) = title;
    if !version.starts_with("HTTP/1.") || !path.starts_with('/') {
        return Err(400);
    }
    let method = if method_str == "POST" {
        Method::Post
    } else {
        Method::Other
    };
    if headers
        .iter()
        .any(|(name, _)| eq_ignore_ascii(name, "transfer-encoding"))
    {
        return Err(400);
    }
    let length = content_length(&headers)?;
    if length > MAX_BODY {
        return Err(413);
    }
    let mut body = rest.to_vec();
    while body.len() < length {
        let need = length - body.len();
        let take = need.min(chunk.len());
        let read = stream.read(&mut chunk[..take]).await.map_err(|_| 400_u16)?;
        if read == 0 {
            return Err(400);
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(length);
    Ok(Incoming {
        method,
        path: path.to_owned(),
        body: Bytes::from(body),
    })
}

fn find_header_end(head: &[u8]) -> Option<usize> {
    head.windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
}

#[allow(clippy::type_complexity)]
fn split_head(
    head: &[u8],
    header_end: usize,
) -> Option<((&str, &str, &str), Vec<(String, String)>, &[u8])> {
    let (header, rest) = head.split_at(header_end);
    let header = std::str::from_utf8(header).ok()?;
    let (line, header_rest) = header.split_once("\r\n")?;
    let mut parts = line.split(' ');
    let method = parts.next()?;
    let path = parts.next()?;
    let version = parts.next()?;
    let mut headers = Vec::new();
    for header_line in header_rest.split("\r\n") {
        if header_line.is_empty() {
            continue;
        }
        let (name, value) = header_line.split_once(':')?;
        headers.push((name.trim().to_owned(), value.trim().to_owned()));
    }
    Some(((method, path, version), headers, rest))
}

fn content_length(headers: &[(String, String)]) -> Result<usize, u16> {
    let mut found = None;
    for (name, value) in headers {
        if eq_ignore_ascii(name, "content-length") {
            let parsed = value.parse::<usize>().map_err(|_| 400_u16)?;
            found = Some(parsed);
        }
    }
    Ok(found.unwrap_or(0))
}

fn eq_ignore_ascii(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

async fn respond(stream: &mut TcpStream, status: u16, body: &[u8]) -> std::io::Result<()> {
    let reason = match status {
        200..=299 => "OK",
        404 => "Not Found",
        413 => "Payload Too Large",
        _ => "Bad Request",
    };
    let content_type = if status == 404 || status == 400 || status == 413 {
        "text/plain; charset=utf-8"
    } else {
        "application/json"
    };
    let reply = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\n\
         Cache-Control: no-store\r\nReferrer-Policy: no-referrer\r\n\
         X-Content-Type-Options: nosniff\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    stream.write_all(reply.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.shutdown().await
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use bytes::Bytes;
    use futures_util::future::BoxFuture;
    use hofund_plugin_api::kernel::BridgeCallbackBackend;
    use hofund_plugin_api::{
        BridgeCallbackHandler, BridgeCallbackReply, PluginErrorKind, PluginId, PluginKey,
        PluginKind,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    use tracing::Subscriber;
    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record};
    use tracing::subscriber::DefaultGuard;

    use super::Registration;
    use super::{BridgeCallbackRegistry, UNKNOWN, constant_time_eq, find_handler, token_from_path};

    const TOKEN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const OTHER: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const SHORT: &str = "short";

    struct Echo;

    impl BridgeCallbackHandler for Echo {
        fn handle(&self, body: Bytes) -> BoxFuture<'_, BridgeCallbackReply> {
            Box::pin(async move { BridgeCallbackReply::new(200, body) })
        }
    }

    struct Counting {
        hits: AtomicUsize,
        body: Mutex<Option<Bytes>>,
        reply: BridgeCallbackReply,
    }

    impl Counting {
        fn new(status: u16, body: &'static [u8]) -> Self {
            Self {
                hits: AtomicUsize::new(0),
                body: Mutex::new(None),
                reply: BridgeCallbackReply::new(status, Bytes::from_static(body)),
            }
        }
    }

    impl BridgeCallbackHandler for Counting {
        fn handle(&self, body: Bytes) -> BoxFuture<'_, BridgeCallbackReply> {
            self.hits.fetch_add(1, Ordering::SeqCst);
            *self.body.lock().expect("body") = Some(body);
            let reply = self.reply.clone();
            Box::pin(async move { reply })
        }
    }

    fn key() -> PluginKey {
        PluginKey::new(PluginKind::Subscription, PluginId::from_static("claude"))
    }

    fn other_key() -> PluginKey {
        PluginKey::new(PluginKind::Subscription, PluginId::from_static("codex"))
    }

    async fn post(url: &str, body: &[u8]) -> (u16, Vec<u8>, String) {
        request("POST", url, body, true).await
    }

    async fn request(method: &str, url: &str, body: &[u8], length: bool) -> (u16, Vec<u8>, String) {
        let parsed = url::Url::parse(url).expect("url");
        let host = parsed.host_str().expect("host");
        let port = parsed.port().expect("port");
        let path = if parsed.query().is_some() {
            format!("{}?{}", parsed.path(), parsed.query().unwrap_or_default())
        } else {
            parsed.path().to_owned()
        };
        let mut stream = TcpStream::connect((host, port)).await.expect("connect");
        let header = if length {
            format!(
                "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: {}\r\n\r\n",
                body.len()
            )
        } else {
            format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n")
        };
        stream
            .write_all(header.as_bytes())
            .await
            .expect("write head");
        stream.write_all(body).await.expect("write body");
        stream.shutdown().await.expect("shutdown");
        let mut reply = Vec::new();
        stream.read_to_end(&mut reply).await.expect("read");
        let text = String::from_utf8_lossy(&reply).into_owned();
        let status = text
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let body = reply
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| reply[index + 4..].to_vec())
            .unwrap_or_default();
        (status, body, text)
    }

    struct LogBucket(Mutex<Vec<String>>);

    impl Visit for LogBucket {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.0
                .lock()
                .expect("logs")
                .push(format!("{}={value:?}", field.name()));
        }

        fn record_str(&mut self, field: &Field, value: &str) {
            self.0
                .lock()
                .expect("logs")
                .push(format!("{}={value}", field.name()));
        }
    }

    struct Capturing(Arc<Mutex<Vec<String>>>);

    impl Subscriber for Capturing {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, span: &Attributes<'_>) -> Id {
            let mut visit = LogBucket(Mutex::new(Vec::new()));
            span.record(&mut visit);
            self.0
                .lock()
                .expect("logs")
                .extend(visit.0.into_inner().expect("inner"));
            Id::from_u64(1)
        }

        fn record(&self, _span: &Id, values: &Record<'_>) {
            let mut visit = LogBucket(Mutex::new(Vec::new()));
            values.record(&mut visit);
            self.0
                .lock()
                .expect("logs")
                .extend(visit.0.into_inner().expect("inner"));
        }

        fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            let mut visit = LogBucket(Mutex::new(Vec::new()));
            event.record(&mut visit);
            let mut logs = self.0.lock().expect("logs");
            logs.push(event.metadata().target().to_owned());
            logs.extend(visit.0.into_inner().expect("inner"));
        }

        fn enter(&self, _span: &Id) {}

        fn exit(&self, _span: &Id) {}
    }

    fn capture_logs() -> (Arc<Mutex<Vec<String>>>, DefaultGuard) {
        let logs = Arc::new(Mutex::new(Vec::new()));
        let guard = tracing::subscriber::set_default(Capturing(Arc::clone(&logs)));
        (logs, guard)
    }

    #[test]
    fn constant_time_eq_compares_every_byte_and_the_length() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(constant_time_eq(b"", b""));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"xbc"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"ab", b"abc"));
    }

    #[test]
    fn find_handler_visits_every_registration() {
        let echo = Arc::new(Echo) as Arc<dyn BridgeCallbackHandler>;
        let live = vec![
            Registration {
                plugin: key(),
                token: SHORT.to_owned(),
                handler: Arc::clone(&echo),
            },
            Registration {
                plugin: key(),
                token: TOKEN.to_owned(),
                handler: Arc::clone(&echo),
            },
            Registration {
                plugin: key(),
                token: OTHER.to_owned(),
                handler: echo,
            },
        ];
        assert!(find_handler(&live, TOKEN).is_some());
        assert!(find_handler(&live, "nope").is_none());
        assert!(find_handler(&live, "no").is_none());
        assert_eq!(live.len(), 3);
        assert_eq!(token_from_path(&format!("/callback/{TOKEN}")), Some(TOKEN));
        assert_eq!(token_from_path("/callback/"), None);
        assert_eq!(token_from_path("/other/token"), None);
        assert_eq!(token_from_path("/callback/a/b"), None);
    }

    #[tokio::test]
    async fn binds_only_ipv4_loopback() {
        let registry = BridgeCallbackRegistry::new();
        let url = registry.base_url().expect("listen");
        let port = registry.port().expect("port");
        assert_eq!(url, format!("http://127.0.0.1:{port}"));
        assert!(url.starts_with("http://127.0.0.1:"));
        assert!(!url.contains("0.0.0.0"));
        assert!(TcpStream::connect(("127.0.0.1", port)).await.is_ok());
        assert!(
            TcpStream::connect(("::1", port)).await.is_err(),
            "the listener must not answer on IPv6"
        );
    }

    #[tokio::test]
    async fn known_token_posts_the_body_to_the_handler() {
        let registry = BridgeCallbackRegistry::new();
        let counting = Arc::new(Counting::new(201, b"{\"ok\":true}"));
        let guard = {
            // register through the public handle path via the backend
            let url = registry
                .register(&key(), TOKEN, Arc::clone(&counting) as _)
                .expect("register");
            assert_eq!(
                url,
                format!(
                    "http://127.0.0.1:{}/callback/{TOKEN}",
                    registry.port().expect("port")
                )
            );
            url
        };
        let (status, body, head) = post(&guard, b"{\"tool\":\"x\"}").await;
        assert_eq!(status, 201);
        assert_eq!(body, b"{\"ok\":true}");
        assert!(
            head.to_ascii_lowercase()
                .contains("content-type: application/json")
        );
        assert_eq!(counting.hits.load(Ordering::SeqCst), 1);
        assert_eq!(
            counting.body.lock().expect("body").as_deref(),
            Some(b"{\"tool\":\"x\"}".as_slice())
        );
    }

    #[tokio::test]
    async fn unknown_missing_or_spent_token_returns_the_fixed_404() {
        let registry = BridgeCallbackRegistry::new();
        let counting = Arc::new(Counting::new(200, b"secret-body"));
        let url = registry
            .register(&key(), TOKEN, Arc::clone(&counting) as _)
            .expect("register");
        let port = registry.port().expect("port");

        let (status, body, text) =
            post(&format!("http://127.0.0.1:{port}/callback/{OTHER}"), b"{}").await;
        assert_eq!(status, 404);
        assert_eq!(body, UNKNOWN);
        assert!(!text.contains(OTHER) || text.contains(&format!("/callback/{OTHER}")));
        // the response body and reason must not echo the token
        let body_text = String::from_utf8_lossy(&body);
        assert_eq!(body_text, "unknown or expired bridge run");
        assert!(!body_text.contains(OTHER) && !body_text.contains(TOKEN));

        let (status, body, _) = post(&format!("http://127.0.0.1:{port}/callback/"), b"{}").await;
        assert_eq!(status, 404);
        assert_eq!(body, UNKNOWN);

        let (status, body, _) = post(&format!("http://127.0.0.1:{port}/other"), b"{}").await;
        assert_eq!(status, 404);
        assert_eq!(body, UNKNOWN);

        registry.unregister(&key(), TOKEN);
        let (status, body, _) = post(&url, b"{}").await;
        assert_eq!(status, 404);
        assert_eq!(body, UNKNOWN);
        assert_eq!(counting.hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn drop_guard_unregisters_the_token() {
        let registry = BridgeCallbackRegistry::new();
        let url = registry
            .register(&key(), TOKEN, Arc::new(Echo) as _)
            .expect("register");
        let (status, _, _) = post(&url, b"hi").await;
        assert_eq!(status, 200);
        registry.unregister(&key(), TOKEN);
        let (status, body, _) = post(&url, b"hi").await;
        assert_eq!(status, 404);
        assert_eq!(body, UNKNOWN);
    }

    #[tokio::test]
    async fn get_or_wrong_method_is_404_even_for_a_live_token() {
        let registry = BridgeCallbackRegistry::new();
        let url = registry
            .register(&key(), TOKEN, Arc::new(Echo) as _)
            .expect("register");
        let (status, body, _) = request("GET", &url, b"", true).await;
        assert_eq!(status, 404);
        assert_eq!(body, UNKNOWN);
        let (status, body, _) = request("PUT", &url, b"{}", true).await;
        assert_eq!(status, 404);
        assert_eq!(body, UNKNOWN);
    }

    #[tokio::test]
    async fn two_plugins_can_register_distinct_tokens() {
        let registry = BridgeCallbackRegistry::new();
        let a = Arc::new(Counting::new(200, b"\"a\""));
        let b = Arc::new(Counting::new(200, b"\"b\""));
        let url_a = registry
            .register(&key(), TOKEN, Arc::clone(&a) as _)
            .expect("a");
        let url_b = registry
            .register(&other_key(), OTHER, Arc::clone(&b) as _)
            .expect("b");
        let (status, body, _) = post(&url_a, b"{}").await;
        assert_eq!((status, body.as_slice()), (200, b"\"a\"".as_slice()));
        let (status, body, _) = post(&url_b, b"{}").await;
        assert_eq!((status, body.as_slice()), (200, b"\"b\"".as_slice()));
        assert_eq!(a.hits.load(Ordering::SeqCst), 1);
        assert_eq!(b.hits.load(Ordering::SeqCst), 1);
        registry.unregister(&key(), OTHER);
        // wrong plugin does not drop the other plugin's token
        let (status, _, _) = post(&url_b, b"{}").await;
        assert_eq!(status, 200);
    }

    #[tokio::test]
    async fn duplicate_token_is_rejected() {
        let registry = BridgeCallbackRegistry::new();
        registry
            .register(&key(), TOKEN, Arc::new(Echo) as _)
            .expect("first");
        let error = registry
            .register(&other_key(), TOKEN, Arc::new(Echo) as _)
            .expect_err("duplicate");
        assert_eq!(error.kind(), PluginErrorKind::Config);
    }

    #[tokio::test]
    async fn oversize_body_is_rejected() {
        let registry = BridgeCallbackRegistry::new();
        let url = registry
            .register(&key(), TOKEN, Arc::new(Echo) as _)
            .expect("register");
        let parsed = url::Url::parse(&url).expect("url");
        let port = parsed.port().expect("port");
        let path = parsed.path();
        let mut stream = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        let too_big = super::MAX_BODY + 1;
        let header = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: {too_big}\r\n\r\n"
        );
        stream.write_all(header.as_bytes()).await.expect("write");
        stream.shutdown().await.expect("shutdown");
        let mut reply = Vec::new();
        stream.read_to_end(&mut reply).await.expect("read");
        let text = String::from_utf8_lossy(&reply);
        assert!(text.starts_with("HTTP/1.1 413"), "{text}");
    }

    #[tokio::test]
    async fn request_targets_are_not_logged() {
        let (logs, _guard) = capture_logs();
        let registry = BridgeCallbackRegistry::new();
        let url = registry
            .register(&key(), TOKEN, Arc::new(Echo) as _)
            .expect("register");
        let (status, _, _) = post(&url, b"{}").await;
        assert_eq!(status, 200);
        let (status, _, _) = post(
            &format!(
                "http://127.0.0.1:{}/callback/{OTHER}",
                registry.port().expect("port")
            ),
            b"{}",
        )
        .await;
        assert_eq!(status, 404);
        let dumped = logs.lock().expect("logs").join("\n");
        assert!(
            !dumped.contains(TOKEN) && !dumped.contains(OTHER),
            "logs must not contain the request token: {dumped}"
        );
    }

    #[tokio::test]
    async fn testhost_register_drop_guard_unregisters() {
        let root = tempfile::tempdir().expect("root");
        let host = crate::TestHost::open_for(
            PluginKind::Subscription,
            PluginId::from_static("claude"),
            root.path().to_path_buf(),
        );
        let base = host.context().bridge_callback().base_url().expect("listen");
        assert!(base.starts_with("http://127.0.0.1:"));
        let url;
        {
            let guard = host
                .context()
                .bridge_callback()
                .register(TOKEN, Arc::new(Echo))
                .expect("register");
            url = guard.callback_url().to_owned();
            assert!(url.starts_with(&base));
            assert!(format!("{guard:?}").contains("claude"));
            assert!(!format!("{guard:?}").contains(TOKEN));
            let (status, body, _) = post(&url, b"hello").await;
            assert_eq!(status, 200);
            assert_eq!(body, b"hello");
        }
        let (status, body, _) = post(&url, b"hello").await;
        assert_eq!(status, 404);
        assert_eq!(body, UNKNOWN);
    }
}
