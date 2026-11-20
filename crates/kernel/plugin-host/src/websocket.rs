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
    /// Proxy the HTTP client would use. A value here refuses the dial.
    pub proxy: Option<&'a str>,
    /// Skip certificate checks, matching the HTTP client's insecure profile.
    pub insecure: bool,
}

/// Dials `dial` and returns an open session.
///
/// A configured proxy returns [`PluginErrorKind::Unsupported`](hofund_plugin_api::PluginErrorKind::Unsupported)
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
    if dial.proxy.is_some() {
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

    use super::{loopback_origin, loopback_uri, websocket_uri};
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
        let http = PooledHttp::new(HttpProfile::direct().insecure());
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
}
