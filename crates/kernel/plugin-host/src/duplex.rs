//! Duplex HTTP on the process-wide pool.

use std::error::Error as StdError;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use hofund_plugin_api::kernel::{DuplexGuard, DuplexWriterBackend};
use hofund_plugin_api::{DuplexRequest, DuplexSession, DuplexWriter, HttpVersionPin, PluginError};
use tokio::sync::mpsc;
use url::Url;

use crate::pool::{self, DuplexDial, HttpLimits, PooledHttp};

pub(crate) const HEADERS_TIMED_OUT: &str = "duplex headers timed out";
pub(crate) const HTTP2_UNAVAILABLE: &str = "http2 unavailable";
pub(crate) const STREAM_REFUSED: &str = "stream refused";

const DUPLEX_READ: std::time::Duration = std::time::Duration::from_secs(5 * 60);

struct ActiveDuplexGuard {
    finished: AtomicBool,
    aborted: Arc<AtomicBool>,
    body_tx: Arc<Mutex<Option<mpsc::Sender<Bytes>>>>,
}

impl ActiveDuplexGuard {
    fn new(body_tx: mpsc::Sender<Bytes>) -> Self {
        Self {
            finished: AtomicBool::new(false),
            aborted: Arc::new(AtomicBool::new(false)),
            body_tx: Arc::new(Mutex::new(Some(body_tx))),
        }
    }

    fn shared_body_tx(&self) -> Arc<Mutex<Option<mpsc::Sender<Bytes>>>> {
        Arc::clone(&self.body_tx)
    }

    fn abort_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.aborted)
    }
}

impl DuplexGuard for ActiveDuplexGuard {
    fn finish(&self) {
        self.finished.store(true, Ordering::SeqCst);
    }

    fn abort_if_unfinished(&self) {
        if self.finished.load(Ordering::SeqCst) {
            return;
        }
        if self.aborted.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Ok(mut slot) = self.body_tx.lock() {
            slot.take();
        }
    }
}

struct ChannelWriter {
    body_tx: Arc<Mutex<Option<mpsc::Sender<Bytes>>>>,
}

#[async_trait]
impl DuplexWriterBackend for ChannelWriter {
    async fn write(&mut self, bytes: Bytes) -> Result<(), PluginError> {
        let sender = self
            .body_tx
            .lock()
            .map_err(|_| PluginError::internal("duplex body lock is poisoned"))?
            .as_ref()
            .ok_or_else(|| PluginError::transport("duplex request body is finished"))?
            .clone();
        sender
            .send(bytes)
            .await
            .map_err(|_| PluginError::transport("duplex request body is closed"))
    }

    async fn finish(&mut self) -> Result<(), PluginError> {
        if let Ok(mut slot) = self.body_tx.lock() {
            slot.take();
        }
        Ok(())
    }
}

fn request_body_stream(
    body_rx: mpsc::Receiver<Bytes>,
    aborted: Arc<AtomicBool>,
) -> impl futures_util::Stream<Item = Result<Bytes, Box<dyn std::error::Error + Send + Sync>>> {
    futures_util::stream::unfold(body_rx, move |mut receiver| {
        let aborted = Arc::clone(&aborted);
        async move {
            if aborted.load(Ordering::Relaxed) {
                return Some((
                    Err(std::io::Error::new(
                        std::io::ErrorKind::ConnectionReset,
                        "duplex request aborted",
                    )
                    .into()),
                    receiver,
                ));
            }
            receiver.recv().await.map(|bytes| (Ok(bytes), receiver))
        }
    })
}

pub(crate) async fn open_duplex(
    http: &PooledHttp,
    request: DuplexRequest,
) -> Result<DuplexSession, PluginError> {
    let dial = dial_for(request.kernel_uri(), request.kernel_version_pin())?;
    validate_duplex_uri(request.kernel_uri())?;
    let client = pool::duplex_client_for(&http.pool_key().profile, dial)?;
    let method = reqwest::Method::from_bytes(request.kernel_method().as_bytes())
        .map_err(|error| PluginError::internal(error.to_string()))?;
    let (body_tx, body_rx) = mpsc::channel::<Bytes>(16);
    let guard = Arc::new(ActiveDuplexGuard::new(body_tx));
    let body_stream = request_body_stream(body_rx, guard.abort_flag());
    let mut outbound = client
        .request(method, request.kernel_uri())
        .body(reqwest::Body::wrap_stream(body_stream));
    for (name, value, sensitive) in request.kernel_header_triples() {
        let mut header_value = reqwest::header::HeaderValue::from_bytes(value.as_bytes())
            .map_err(|_| PluginError::internal(format!("invalid header {name}")))?;
        header_value.set_sensitive(sensitive);
        outbound = outbound.header(name, header_value);
    }
    let header_wait = request.kernel_header_wait();
    let send = outbound.send();
    let response = match tokio::time::timeout(header_wait, send).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => return Err(map_duplex_open_error(error)),
        Err(_) => {
            guard.abort_if_unfinished();
            return Err(PluginError::timeout(format!(
                "{HEADERS_TIMED_OUT}: no response headers within {header_wait:?}"
            )));
        }
    };
    let status = response.status();
    let headers = response.headers().clone();
    let reader = pool::live_duplex_body(response, guard.abort_flag());
    let body_tx = guard.shared_body_tx();
    let guard: Arc<dyn DuplexGuard> = guard;
    let writer = DuplexWriter::kernel_new(Box::new(ChannelWriter { body_tx }), Arc::clone(&guard));
    Ok(DuplexSession::kernel_new(
        status, headers, reader, writer, guard,
    ))
}

fn dial_for(uri: &str, pin: HttpVersionPin) -> Result<DuplexDial, PluginError> {
    let parsed = Url::parse(uri).map_err(|error| PluginError::config(error.to_string()))?;
    match (parsed.scheme(), pin) {
        ("https", HttpVersionPin::Http2) => Ok(DuplexDial::Alpn),
        ("http", HttpVersionPin::Http2) => Ok(DuplexDial::PriorKnowledge),
        ("https", HttpVersionPin::Http1) | ("http", HttpVersionPin::Http1) => Err(
            PluginError::config("duplex calls require HttpVersionPin::Http2"),
        ),
        (scheme, _) => Err(PluginError::config(format!(
            "duplex URI scheme {scheme} is not supported"
        ))),
    }
}

fn is_loopback_host(host: &str) -> bool {
    host == "127.0.0.1" || host.eq_ignore_ascii_case("localhost") || host == "::1"
}

fn validate_duplex_uri(uri: &str) -> Result<(), PluginError> {
    let parsed = Url::parse(uri).map_err(|error| PluginError::config(error.to_string()))?;
    match parsed.scheme() {
        "https" => Ok(()),
        "http" => {
            if duplex_loopback_host(&parsed) {
                Ok(())
            } else {
                Err(PluginError::config(
                    "cleartext duplex dials are limited to loopback hosts",
                ))
            }
        }
        scheme => Err(PluginError::config(format!(
            "duplex URI scheme {scheme} is not supported"
        ))),
    }
}

fn duplex_loopback_host(parsed: &Url) -> bool {
    match parsed.host() {
        Some(url::Host::Domain(name)) => is_loopback_host(name),
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    }
}

/// Maps a failed duplex open to the stable plugin-facing classes in the design doc.
#[must_use]
pub(crate) fn map_duplex_open_error(error: reqwest::Error) -> PluginError {
    let message = error_chain_message(&error);
    classify_duplex_failure(error.is_timeout(), error.is_connect(), &message)
}

fn error_chain_message(error: &reqwest::Error) -> String {
    let mut message = format!("{error:#}");
    let mut source = StdError::source(error);
    while let Some(err) = source {
        message.push_str(": ");
        message.push_str(&err.to_string());
        source = err.source();
    }
    message
}

fn http2_unavailable_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.starts_with("err_http2")
        || lower.contains("h2 is not supported")
        || lower.contains("alpn")
        || lower.contains("nghttp2")
        || lower.contains("protocol error")
        || lower.contains("http2 unavilable")
        || lower.contains("http/2")
        || lower.contains("cannot start a connection")
        || lower.contains("noapplicationprotocol")
        || lower.contains("no application protocol")
}

pub(crate) fn classify_duplex_failure(
    is_timeout: bool,
    is_connect: bool,
    message: &str,
) -> PluginError {
    if http2_unavailable_message(message) {
        return PluginError::transport(format!("{HTTP2_UNAVAILABLE}: {message}"));
    }
    if is_connect {
        return PluginError::connect(message.to_owned());
    }
    if is_timeout {
        return PluginError::timeout(format!("{HEADERS_TIMED_OUT}: {message}"));
    }
    let lower = message.to_ascii_lowercase();
    if lower.contains("stream refused") || lower.contains("refused_stream") {
        return PluginError::transport(format!("{STREAM_REFUSED}: {message}"));
    }
    if lower.contains("stream error") && !lower.contains("refused") {
        return PluginError::transport(format!("{STREAM_REFUSED}: {message}"));
    }
    PluginError::transport(message.to_owned())
}

pub(crate) fn duplex_limits() -> HttpLimits {
    HttpLimits::new(HttpLimits::default().connect(), DUPLEX_READ).with_max_idle_per_host(0)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use hofund_plugin_api::{DuplexRequest, PluginErrorKind};
    use rustls::ServerConfig;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_rustls::TlsAcceptor;

    use super::{
        HEADERS_TIMED_OUT, HTTP2_UNAVAILABLE, STREAM_REFUSED, classify_duplex_failure,
        is_loopback_host, validate_duplex_uri,
    };
    use crate::context_for_http;
    use crate::pool::{HttpProfile, PooledHttp};

    #[test]
    fn header_timeout_uses_the_duplex_prefix() {
        let mapped = classify_duplex_failure(true, false, "operation timed out");
        assert_eq!(mapped.kind(), PluginErrorKind::Timeout);
        assert!(
            mapped.message().starts_with(HEADERS_TIMED_OUT),
            "{}",
            mapped.message()
        );
    }

    #[test]
    fn connect_timeout_with_both_flags_is_connect_not_header_timeout() {
        let mapped = classify_duplex_failure(true, true, "operation timed out");
        assert_eq!(mapped.kind(), PluginErrorKind::Connect);
        assert!(
            !mapped.message().starts_with(HEADERS_TIMED_OUT),
            "{}",
            mapped.message()
        );
    }

    #[test]
    fn alpn_failure_uses_http2_unavailable_prefix() {
        let error = classify_duplex_failure(false, false, "h2 is not supported by proxy");
        assert_eq!(error.kind(), PluginErrorKind::Transport);
        assert!(
            error.message().starts_with(HTTP2_UNAVAILABLE),
            "{}",
            error.message()
        );
    }

    #[test]
    fn stream_refused_uses_its_prefix() {
        let error = classify_duplex_failure(false, false, "REFUSED_STREAM before headers");
        assert_eq!(error.kind(), PluginErrorKind::Transport);
        assert!(
            error.message().starts_with(STREAM_REFUSED),
            "{}",
            error.message()
        );
    }

    #[test]
    fn inverted_prefix_branches_fail() {
        let error = classify_duplex_failure(false, false, "h2 is not supported");
        assert!(
            !error.message().starts_with(STREAM_REFUSED),
            "http2 unavailable must not look like stream refused"
        );
        let error = classify_duplex_failure(false, false, "REFUSED_STREAM");
        assert!(
            !error.message().starts_with(HTTP2_UNAVAILABLE),
            "stream refused must not look like http2 unavailable"
        );
    }

    #[test]
    fn loopback_includes_ipv6_localhost() {
        assert!(is_loopback_host("::1"));
        assert!(validate_duplex_uri("http://[::1]:9/x").is_ok());
    }

    async fn spawn_tls_http11_only_server() -> u16 {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into(), "127.0.0.1".into()])
            .expect("cert");
        let cert_der = CertificateDer::from(cert.cert.der().to_vec());
        let key_der = PrivateKeyDer::try_from(cert.signing_key.serialize_der()).expect("key");
        let mut server_config = ServerConfig::builder_with_protocol_versions(&[
            &rustls::version::TLS12,
            &rustls::version::TLS13,
        ])
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .expect("server config");
        server_config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let acceptor = TlsAcceptor::from(Arc::new(server_config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(mut tls) = acceptor.accept(stream).await else {
                        return;
                    };
                    let mut buf = vec![0_u8; 4096];
                    let _ = tls.read(&mut buf).await;
                    let _ = tls
                        .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")
                        .await;
                });
            }
        });
        port
    }

    #[tokio::test]
    async fn alpn_http11_only_server_yields_http2_unavailable() {
        let port = spawn_tls_http11_only_server().await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let http = Arc::new(PooledHttp::new(HttpProfile::direct().insecure()));
        let context = context_for_http(http);
        let error = match context
            .http()
            .open_duplex(DuplexRequest::post(format!(
                "https://127.0.0.1:{port}/duplex"
            )))
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("http2-only client must not open on an http/1.1 ALPN server"),
        };
        assert_eq!(
            error.kind(),
            PluginErrorKind::Transport,
            "message was {}",
            error.message()
        );
        assert!(
            error.message().starts_with(HTTP2_UNAVAILABLE),
            "{}",
            error.message()
        );
    }

    async fn spawn_h2_loopback_duplex_server(request_body_reset: Arc<AtomicBool>) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let request_body_reset = Arc::clone(&request_body_reset);
                tokio::spawn(async move {
                    let Ok(mut connection) = h2::server::handshake(stream).await else {
                        return;
                    };
                    while let Some(result) = connection.accept().await {
                        let Ok((request, mut respond)) = result else {
                            continue;
                        };
                        let request_body_reset = Arc::clone(&request_body_reset);
                        tokio::spawn(async move {
                            let response = http::Response::builder()
                                .status(200)
                                .body(())
                                .expect("response");
                            let Ok(mut send) = respond.send_response(response, false) else {
                                return;
                            };
                            let mut body = request.into_body();
                            loop {
                                match body.data().await {
                                    Some(Ok(chunk)) => {
                                        let _ = body.flow_control().release_capacity(chunk.len());
                                    }
                                    Some(Err(error)) => {
                                        if error.is_reset() {
                                            request_body_reset.store(true, Ordering::SeqCst);
                                        }
                                        break;
                                    }
                                    None => break,
                                }
                            }
                            let _ = send.send_data(bytes::Bytes::from_static(b"hold"), false);
                            tokio::time::sleep(Duration::from_secs(30)).await;
                        });
                    }
                });
            }
        });
        port
    }

    async fn wait_for_reset(flag: &AtomicBool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !flag.load(Ordering::SeqCst) {
            if tokio::time::Instant::now() >= deadline {
                panic!("timed out waiting for the peer to see a stream reset");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn dropping_open_duplex_session_resets_the_request_body_stream() {
        let reset = Arc::new(AtomicBool::new(false));
        let port = spawn_h2_loopback_duplex_server(Arc::clone(&reset)).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let http = Arc::new(PooledHttp::new(HttpProfile::direct()));
        let context = context_for_http(http);
        let session = context
            .http()
            .open_duplex(DuplexRequest::post(format!(
                "http://127.0.0.1:{port}/duplex"
            )))
            .await
            .expect("duplex session");
        assert_eq!(session.status().as_u16(), 200);
        drop(session);
        wait_for_reset(&reset).await;
    }

    #[tokio::test]
    async fn dropping_duplex_writer_half_resets_the_request_body_stream() {
        let reset = Arc::new(AtomicBool::new(false));
        let port = spawn_h2_loopback_duplex_server(Arc::clone(&reset)).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let http = Arc::new(PooledHttp::new(HttpProfile::direct()));
        let context = context_for_http(http);
        let session = context
            .http()
            .open_duplex(DuplexRequest::post(format!(
                "http://127.0.0.1:{port}/duplex"
            )))
            .await
            .expect("duplex session");
        let (mut reader, writer) = session.into_split();
        drop(writer);
        let _ = reader.reader().next_chunk().await;
        wait_for_reset(&reset).await;
    }

    #[tokio::test]
    async fn dropping_duplex_reader_half_resets_the_request_body_stream() {
        let reset = Arc::new(AtomicBool::new(false));
        let port = spawn_h2_loopback_duplex_server(Arc::clone(&reset)).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let http = Arc::new(PooledHttp::new(HttpProfile::direct()));
        let context = context_for_http(http);
        let session = context
            .http()
            .open_duplex(DuplexRequest::post(format!(
                "http://127.0.0.1:{port}/duplex"
            )))
            .await
            .expect("duplex session");
        let (reader, writer) = session.into_split();
        drop(reader);
        wait_for_reset(&reset).await;
        drop(writer);
    }
}
