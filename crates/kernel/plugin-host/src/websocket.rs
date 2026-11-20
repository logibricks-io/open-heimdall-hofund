//! Outbound WebSocket dialing on the host. Plugins do not open sockets.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use hofund_plugin_api::kernel::WebSocketIo;
use hofund_plugin_api::{PluginError, WebSocketMessage, WebSocketSession};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{Message, http};
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream, connect_async_tls_with_config,
};

/// Largest accepted frame or message. A peer that exceeds it is a transport error.
const MAX_FRAME: usize = 8 * 1024 * 1024;
/// How long a handshake may take before the caller sees a timeout.
const HANDSHAKE: Duration = Duration::from_secs(15);

/// What the pooled client knows about the dial.
pub(crate) struct Dial<'a> {
    /// `ws`, `wss`, `http`, or `https` URI.
    pub uri: &'a str,
    /// `(name, value, sensitive)`.
    pub headers: Vec<(&'a str, &'a str, bool)>,
    /// Idle budget between frames. This is [`crate::HttpLimits::read`].
    pub read: Duration,
    /// Close after this long with no send or recv.
    pub idle: Option<Duration>,
    /// Close this long after the handshake.
    pub age: Option<Duration>,
    /// Explicit profile proxy. `reqwest` sends every origin through it.
    pub proxy: Option<&'a str>,
    /// Environment the pooled client reads when `proxy` is absent.
    pub env: ProxyEnv,
    /// Skip certificate checks, matching the HTTP client's insecure profile.
    pub insecure: bool,
}

/// Environment variables the pooled HTTP client uses for its system proxy.
///
/// This is the same set `reqwest` reads when the profile has no explicit proxy:
/// `ALL_PROXY`, `HTTPS_PROXY`, `HTTP_PROXY`, and `NO_PROXY`, each with its
/// lowercase twin. A set `REQUEST_METHOD` is the CGI exception and disables
/// those variables, matching `hyper-util`'s matcher.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ProxyEnv {
    all: String,
    http: String,
    https: String,
    no: String,
    cgi: bool,
}

impl ProxyEnv {
    /// No proxy variables and no CGI exception.
    #[must_use]
    pub(crate) fn empty() -> Self {
        Self::default()
    }

    /// The process environment, read the way the pooled client reads it.
    #[must_use]
    pub(crate) fn process() -> Self {
        Self {
            cgi: std::env::var_os("REQUEST_METHOD").is_some(),
            all: first_var(&["ALL_PROXY", "all_proxy"]),
            http: first_var(&["HTTP_PROXY", "http_proxy"]),
            https: first_var(&["HTTPS_PROXY", "https_proxy"]),
            no: first_var(&["NO_PROXY", "no_proxy"]),
        }
    }

    /// A stand-in environment. Names match the process variables exactly.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn from_pairs(pairs: &[(&str, &str)]) -> Self {
        Self {
            cgi: pairs.iter().any(|(name, _)| *name == "REQUEST_METHOD"),
            all: pair(pairs, &["ALL_PROXY", "all_proxy"]),
            http: pair(pairs, &["HTTP_PROXY", "http_proxy"]),
            https: pair(pairs, &["HTTPS_PROXY", "https_proxy"]),
            no: pair(pairs, &["NO_PROXY", "no_proxy"]),
        }
    }

    fn has_candidate(&self) -> bool {
        !self.all.is_empty() || !self.http.is_empty() || !self.https.is_empty()
    }
}

fn first_var(names: &[&str]) -> String {
    for name in names {
        if let Ok(value) = std::env::var(name) {
            return value;
        }
    }
    String::new()
}

#[cfg(test)]
fn pair(pairs: &[(&str, &str)], names: &[&str]) -> String {
    for name in names {
        if let Some((_, value)) = pairs.iter().find(|(key, _)| *key == *name) {
            return (*value).to_owned();
        }
    }
    String::new()
}

/// Whether the pooled HTTP client would send this origin through a proxy.
///
/// `uri` may be `ws` or `wss`. The decision uses the `http` or `https` origin
/// the HTTP client would call, because the system matcher ignores `ws` and `wss`.
#[must_use]
pub(crate) fn http_path_uses_proxy(profile_proxy: Option<&str>, env: &ProxyEnv, uri: &str) -> bool {
    if profile_proxy.is_some() {
        return true;
    }
    if env.cgi {
        return false;
    }
    let Some(http_uri) = http_origin(uri) else {
        return env.has_candidate();
    };
    let Ok(parsed) = http_uri.parse::<::http::Uri>() else {
        return env.has_candidate();
    };
    hyper_util::client::proxy::matcher::Matcher::builder()
        .all(env.all.as_str())
        .http(env.http.as_str())
        .https(env.https.as_str())
        .no(env.no.as_str())
        .build()
        .intercept(&parsed)
        .is_some()
}

fn http_origin(uri: &str) -> Option<String> {
    if let Some(rest) = uri.strip_prefix("wss://") {
        Some(format!("https://{rest}"))
    } else if let Some(rest) = uri.strip_prefix("ws://") {
        Some(format!("http://{rest}"))
    } else if uri.starts_with("https://") || uri.starts_with("http://") {
        Some(uri.to_owned())
    } else {
        None
    }
}

/// Dials `dial` and returns an open session.
///
/// A proxy the pooled client would use for this origin returns
/// [`PluginErrorKind::Unsupported`](hofund_plugin_api::PluginErrorKind::Unsupported)
/// before any connection, so the bearer is not sent on a direct socket.
/// `insecure` uses a TLS client that accepts any certificate. The default
/// verifies with the native root store, the same policy as the pooled client.
///
/// # Errors
///
/// Returns [`PluginError`] when a proxy is configured, the URI is not a
/// handshake target, the dial fails, or the peer refuses the handshake. A
/// refusal keeps its HTTP status.
pub(crate) async fn connect(dial: Dial<'_>) -> Result<WebSocketSession, PluginError> {
    if http_path_uses_proxy(dial.proxy, &dial.env, dial.uri) {
        return Err(PluginError::unsupported(
            "websocket dial does not use this HTTP proxy",
        ));
    }
    let uri = websocket_uri(dial.uri)?;
    let mut request = uri
        .into_client_request()
        .map_err(|_| PluginError::config("the websocket URI is not a handshake target"))?;
    for (name, value, sensitive) in dial.headers {
        let header_name = http::HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| PluginError::config("a websocket header name is not valid"))?;
        let mut header_value = http::HeaderValue::from_str(value)
            .map_err(|_| PluginError::config("a websocket header value is not valid"))?;
        header_value.set_sensitive(sensitive);
        request.headers_mut().insert(header_name, header_value);
    }
    let mut config = WebSocketConfig::default();
    config.read_buffer_size = 8 * 1024;
    config.write_buffer_size = 8 * 1024;
    config.max_write_buffer_size = MAX_FRAME;
    config.max_message_size = Some(MAX_FRAME);
    config.max_frame_size = Some(MAX_FRAME);
    let connector = dial.insecure.then(insecure_connector);
    let opening = connect_async_tls_with_config(request, Some(config), true, connector);
    let (stream, _) = match tokio::time::timeout(HANDSHAKE, opening).await {
        Ok(Ok(opened)) => opened,
        Ok(Err(error)) => return Err(map_handshake(error)),
        Err(_) => {
            return Err(PluginError::timeout("websocket handshake timed out"));
        }
    };
    let opened = tokio::time::Instant::now();
    let shared = Arc::new(Shared {
        stream: tokio::sync::Mutex::new(stream),
        used: Mutex::new(opened),
        opened,
        idle: dial.idle.unwrap_or(Duration::MAX),
        age: dial.age.unwrap_or(Duration::MAX),
        read: dial.read,
        stopped: AtomicBool::new(false),
    });
    let watch = (dial.idle.is_some() || dial.age.is_some()).then(|| {
        let shared = Arc::clone(&shared);
        tokio::spawn(async move { watch(shared).await })
    });
    Ok(WebSocketSession::kernel_new(Box::new(LiveSocket {
        shared,
        watch,
    })))
}

/// `uri` with the origin replaced by loopback `origin`, keeping the path.
///
/// `origin` must be an `http` or `https` URL on `127.0.0.1`, `localhost`, or `[::1]`.
///
/// # Errors
///
/// Returns [`PluginError`] when `origin` is not that kind of URL.
pub(crate) fn loopback_uri(uri: &str, origin: &str) -> Result<String, PluginError> {
    if !loopback_origin(origin) {
        return Err(PluginError::unsupported(
            "this HTTP backend has no websocket client",
        ));
    }
    Ok(format!(
        "{}{}",
        origin.trim_end_matches('/'),
        path_and_query(uri)
    ))
}

fn path_and_query(uri: &str) -> &str {
    let rest = uri.split_once("://").map_or(uri, |(_, rest)| rest);
    rest.find('/').map_or("/", |index| &rest[index..])
}

/// Whether `origin` is an HTTP URL on a loopback host.
#[must_use]
pub(crate) fn loopback_origin(origin: &str) -> bool {
    let Some(rest) = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
    else {
        return false;
    };
    let host = rest.split('/').next().unwrap_or("");
    let host = host.rsplit_once(':').map_or(host, |(name, _)| name);
    matches!(host, "127.0.0.1" | "localhost" | "[::1]")
}

fn websocket_uri(uri: &str) -> Result<String, PluginError> {
    if let Some(rest) = uri.strip_prefix("https://") {
        Ok(format!("wss://{rest}"))
    } else if let Some(rest) = uri.strip_prefix("http://") {
        Ok(format!("ws://{rest}"))
    } else if uri.starts_with("ws://") || uri.starts_with("wss://") {
        Ok(uri.to_owned())
    } else {
        Err(PluginError::config(
            "the websocket URI is not a handshake target",
        ))
    }
}

fn map_handshake(error: tokio_tungstenite::tungstenite::Error) -> PluginError {
    match error {
        tokio_tungstenite::tungstenite::Error::Http(response) => PluginError::status_error(
            response.status().as_u16(),
            "the websocket handshake was refused",
        ),
        tokio_tungstenite::tungstenite::Error::Io(error) => match error.kind() {
            std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::NotFound
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted => {
                PluginError::connect("websocket connect failed")
            }
            _ => PluginError::transport("websocket connection failed"),
        },
        tokio_tungstenite::tungstenite::Error::Url(_) => {
            PluginError::config("the websocket URI is not a handshake target")
        }
        _ => PluginError::transport("websocket handshake failed"),
    }
}

fn insecure_connector() -> Connector {
    Connector::Rustls(insecure_config())
}

fn insecure_config() -> Arc<rustls::ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    Arc::new(
        rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("rustls protocol versions")
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(SkipServerCert))
            .with_no_client_auth(),
    )
}

#[derive(Debug)]
struct SkipServerCert;

impl ServerCertVerifier for SkipServerCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

struct Shared {
    stream: tokio::sync::Mutex<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    used: Mutex<tokio::time::Instant>,
    opened: tokio::time::Instant,
    idle: Duration,
    age: Duration,
    read: Duration,
    stopped: AtomicBool,
}

async fn watch(shared: Arc<Shared>) {
    loop {
        if shared.stopped.load(Ordering::Acquire) {
            return;
        }
        let now = tokio::time::Instant::now();
        let used = *shared
            .used
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let deadline = match (
            used.checked_add(shared.idle),
            shared.opened.checked_add(shared.age),
        ) {
            (Some(idle_at), Some(age_at)) => idle_at.min(age_at),
            (Some(idle_at), None) => idle_at,
            (None, Some(age_at)) => age_at,
            (None, None) => return,
        };
        if now >= deadline {
            let mut stream = shared.stream.lock().await;
            let _ = stream.close(None).await;
            shared.stopped.store(true, Ordering::Release);
            return;
        }
        tokio::time::sleep_until(deadline).await;
    }
}

struct LiveSocket {
    shared: Arc<Shared>,
    watch: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for LiveSocket {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::Release);
        if let Some(watch) = self.watch.take() {
            watch.abort();
        }
    }
}

impl LiveSocket {
    fn touch(&self) {
        *self
            .shared
            .used
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = tokio::time::Instant::now();
    }
}

#[async_trait]
impl WebSocketIo for LiveSocket {
    async fn send_text(&mut self, text: String) -> Result<(), PluginError> {
        let mut stream = self.shared.stream.lock().await;
        stream
            .send(Message::Text(text.into()))
            .await
            .map_err(|_| PluginError::transport("websocket write failed"))?;
        drop(stream);
        self.touch();
        Ok(())
    }

    async fn recv(&mut self) -> Result<Option<WebSocketMessage>, PluginError> {
        // Yields before the budget is armed let a paused clock run the peer.
        // Arming the budget immediately makes that clock skip the peer.
        let mut yields = 0_u8;
        loop {
            let message = {
                let mut stream = self.shared.stream.lock().await;
                tokio::select! {
                    biased;
                    message = stream.next() => match message {
                        None => return Ok(None),
                        Some(Ok(message)) => message,
                        Some(Err(_)) => {
                            return Err(PluginError::transport("websocket read failed"));
                        }
                    },
                    () = tokio::task::yield_now(), if yields < 8 => {
                        yields += 1;
                        continue;
                    }
                    () = tokio::time::sleep(self.shared.read), if yields >= 8 => {
                        return Err(PluginError::timeout("websocket read timed out"));
                    }
                }
            };
            match message {
                Message::Text(text) => {
                    self.touch();
                    return Ok(Some(WebSocketMessage::Text(text.to_string())));
                }
                Message::Binary(bytes) => {
                    self.touch();
                    return Ok(Some(WebSocketMessage::Binary(bytes.to_vec())));
                }
                Message::Ping(payload) => {
                    self.touch();
                    let mut stream = self.shared.stream.lock().await;
                    stream
                        .send(Message::Pong(payload))
                        .await
                        .map_err(|_| PluginError::transport("websocket write failed"))?;
                }
                Message::Pong(_) | Message::Frame(_) => {}
                Message::Close(_) => return Ok(None),
            }
        }
    }

    async fn close(&mut self) -> Result<(), PluginError> {
        self.shared.stopped.store(true, Ordering::Release);
        let mut stream = self.shared.stream.lock().await;
        stream
            .close(None)
            .await
            .map_err(|_| PluginError::transport("websocket close failed"))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use hofund_plugin_api::kernel::HttpBackend;
    use hofund_plugin_api::{PluginErrorKind, WebSocketRequest};

    use super::{ProxyEnv, http_path_uses_proxy, loopback_origin, loopback_uri, websocket_uri};
    use crate::pool::{HttpLimits, HttpProfile, PooledHttp};

    #[test]
    fn http_origins_become_websocket_uris() {
        assert_eq!(
            websocket_uri("https://example.test/responses").expect("wss"),
            "wss://example.test/responses"
        );
        assert_eq!(
            websocket_uri("http://127.0.0.1:9/responses").expect("ws"),
            "ws://127.0.0.1:9/responses"
        );
        assert!(websocket_uri("ftp://example.test").is_err());
    }

    #[test]
    fn loopback_rewrite_keeps_the_path() {
        assert!(loopback_origin("http://127.0.0.1:9"));
        assert!(!loopback_origin("https://example.test"));
        assert_eq!(
            loopback_uri("wss://api.example.test/responses?x=1", "http://127.0.0.1:9")
                .expect("rewrite"),
            "http://127.0.0.1:9/responses?x=1"
        );
        assert!(loopback_uri("wss://api.example.test/responses", "https://example.test").is_err());
    }

    #[tokio::test]
    async fn a_proxy_profile_does_not_dial() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("addr");
        let accepted = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&accepted);
        tokio::spawn(async move {
            if listener.accept().await.is_ok() {
                flag.store(true, Ordering::SeqCst);
            }
        });
        let http = PooledHttp::with_limits(
            HttpProfile::direct().with_proxy("http://127.0.0.1:9"),
            HttpLimits::default(),
        );
        let error = http
            .open_websocket(
                WebSocketRequest::get(format!("ws://{address}/responses"))
                    .sensitive_header("Authorization", "Bearer secret"),
            )
            .await
            .expect_err("proxy refuses the socket");
        assert_eq!(error.kind(), PluginErrorKind::Unsupported);
        tokio::task::yield_now().await;
        assert!(
            !accepted.load(Ordering::SeqCst),
            "the bearer must not leave on a direct socket"
        );
    }

    #[tokio::test]
    async fn an_insecure_profile_dials_without_verifying() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let port = crate::test_tls::spawn_tls_capture(Vec::new(), Arc::clone(&captured)).await;
        let http =
            PooledHttp::new(HttpProfile::direct().insecure()).with_proxy_env(ProxyEnv::empty());
        let _ = http
            .open_websocket(
                WebSocketRequest::get(format!("wss://127.0.0.1:{port}/responses"))
                    .sensitive_header("Authorization", "Bearer secret"),
            )
            .await;
        let bytes = captured.lock().expect("capture").clone();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            text.to_ascii_lowercase()
                .contains("authorization: bearer secret"),
            "insecure profile should reach the handshake, saw {text:?}"
        );
    }

    #[test]
    fn system_proxy_matches_the_http_origin() {
        let proxy = [("HTTPS_PROXY", "http://127.0.0.1:9")];
        let env = ProxyEnv::from_pairs(&proxy);
        assert!(http_path_uses_proxy(
            None,
            &env,
            "wss://api.example.test/responses"
        ));
        assert!(!http_path_uses_proxy(
            None,
            &ProxyEnv::from_pairs(&[
                ("HTTPS_PROXY", "http://127.0.0.1:9"),
                ("NO_PROXY", "api.example.test"),
            ]),
            "wss://api.example.test/responses"
        ));
        assert!(!http_path_uses_proxy(
            None,
            &ProxyEnv::from_pairs(&[
                ("HTTPS_PROXY", "http://127.0.0.1:9"),
                ("REQUEST_METHOD", "GET"),
            ]),
            "wss://api.example.test/responses"
        ));
        assert!(!http_path_uses_proxy(
            None,
            &ProxyEnv::from_pairs(&[("HTTP_PROXY", "http://127.0.0.1:9")]),
            "wss://api.example.test/responses"
        ));
        assert!(http_path_uses_proxy(
            None,
            &ProxyEnv::from_pairs(&[("HTTP_PROXY", "http://127.0.0.1:9")]),
            "ws://api.example.test/responses"
        ));
        assert!(http_path_uses_proxy(
            None,
            &ProxyEnv::from_pairs(&[("ALL_PROXY", "http://127.0.0.1:9")]),
            "wss://api.example.test/responses"
        ));
        assert!(http_path_uses_proxy(
            Some("http://127.0.0.1:9"),
            &ProxyEnv::from_pairs(&[("REQUEST_METHOD", "GET"), ("NO_PROXY", "*")]),
            "wss://api.example.test/responses"
        ));
    }

    #[tokio::test]
    async fn an_environment_proxy_does_not_dial() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("addr");
        let captured = Arc::new(Mutex::new(Vec::new()));
        let accepted = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&accepted);
        let bytes = Arc::clone(&captured);
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                flag.store(true, Ordering::SeqCst);
                let mut buf = vec![0_u8; 2048];
                if let Ok(n) = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await {
                    bytes.lock().expect("capture").extend_from_slice(&buf[..n]);
                }
            }
        });
        let http = PooledHttp::direct().with_proxy_env(ProxyEnv::from_pairs(&[(
            "HTTPS_PROXY",
            "http://127.0.0.1:9",
        )]));
        let error = http
            .open_websocket(
                WebSocketRequest::get(format!("wss://{address}/responses"))
                    .sensitive_header("Authorization", "Bearer secret"),
            )
            .await
            .expect_err("an environment proxy refuses the socket");
        assert_eq!(error.kind(), PluginErrorKind::Unsupported);
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert!(
            !accepted.load(Ordering::SeqCst),
            "the upstream accepted a direct socket"
        );
        let wire = captured.lock().expect("capture").clone();
        assert!(
            !String::from_utf8_lossy(&wire)
                .to_ascii_lowercase()
                .contains("bearer"),
            "the bearer left on the wire: {wire:?}"
        );
    }

    #[tokio::test]
    async fn no_proxy_and_cgi_still_dial() {
        let direct = dial_cleartext(ProxyEnv::from_pairs(&[
            ("HTTP_PROXY", "http://127.0.0.1:9"),
            ("NO_PROXY", "127.0.0.1"),
        ]))
        .await;
        assert!(
            direct.contains("authorization: bearer secret"),
            "NO_PROXY should dial directly, saw {direct:?}"
        );
        let cgi = dial_cleartext(ProxyEnv::from_pairs(&[
            ("HTTP_PROXY", "http://127.0.0.1:9"),
            ("REQUEST_METHOD", "GET"),
        ]))
        .await;
        assert!(
            cgi.contains("authorization: bearer secret"),
            "the CGI exception should dial directly, saw {cgi:?}"
        );
    }

    async fn dial_cleartext(env: ProxyEnv) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("addr");
        let captured = Arc::new(Mutex::new(String::new()));
        let bytes = Arc::clone(&captured);
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = vec![0_u8; 2048];
                if let Ok(n) = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await {
                    *bytes.lock().expect("capture") =
                        String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
                }
            }
        });
        let http = PooledHttp::direct().with_proxy_env(env);
        let _ = http
            .open_websocket(
                WebSocketRequest::get(format!("ws://{address}/responses"))
                    .sensitive_header("Authorization", "Bearer secret"),
            )
            .await;
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        captured.lock().expect("capture").clone()
    }
}
