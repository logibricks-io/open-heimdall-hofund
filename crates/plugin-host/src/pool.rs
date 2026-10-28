//! Process-wide HTTP clients. One client per proxy and TLS profile.
//!
//! The pool is the `reqwest` client in the plugin stack. Plugins call
//! [`crate::host_context`]'s [`HttpHandle`](hofund_plugin_api::HttpHandle), and
//! the SDK builds the request. Built-in adapters that still stream take the
//! same client from [`HttpBackend::pooled_client`](hofund_plugin_api::kernel::HttpBackend::pooled_client).
//! This module does not add a profile argument to
//! [`HttpBackend`](hofund_plugin_api::kernel::HttpBackend): the profile is
//! chosen when the handle is built.

use std::any::Any;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use hofund_plugin_api::kernel::{HttpBackend, HttpRequest, HttpResponse};
use hofund_plugin_api::{Body, BodyStream, PluginError};

const USER_AGENT: &str = "hofund/0.1.0";
const MAX_IDLE_PER_HOST: usize = 8;
const DEFAULT_CONNECT: Duration = Duration::from_secs(10);
const DEFAULT_READ: Duration = Duration::from_secs(60);

/// Proxy and TLS settings that select a pooled client.
///
/// Two handles with the same profile share one [`reqwest::Client`].
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct HttpProfile {
    proxy: Option<String>,
    insecure: bool,
}

impl HttpProfile {
    /// No proxy, and TLS certificates are verified.
    #[must_use]
    pub fn direct() -> Self {
        Self {
            proxy: None,
            insecure: false,
        }
    }

    /// Sends through `proxy`. `reqwest` parses the URL when the client is built.
    #[must_use]
    pub fn with_proxy(mut self, proxy: impl Into<String>) -> Self {
        self.proxy = Some(proxy.into());
        self
    }

    /// Accepts invalid TLS certificates.
    #[must_use]
    pub fn insecure(mut self) -> Self {
        self.insecure = true;
        self
    }
}

impl Default for HttpProfile {
    fn default() -> Self {
        Self::direct()
    }
}

/// Host timeout and idle-pool config for the pooled client.
///
/// `connect` bounds DNS, TCP, and TLS setup. `read` is the daemon-wide reqwest
/// idle budget between socket reads, including for built-in adapters, and resets
/// after each successful read. It is not derived from an adapter's endpoint
/// timeout: when that endpoint timeout is longer, this read budget fires first.
/// A stream that keeps sending is not cut off. There is no whole-call timeout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct HttpLimits {
    connect: Duration,
    read: Duration,
    max_idle_per_host: usize,
}

impl HttpLimits {
    /// `connect` is the setup budget. `read` is the idle budget between reads.
    #[must_use]
    pub const fn new(connect: Duration, read: Duration) -> Self {
        Self {
            connect,
            read,
            max_idle_per_host: MAX_IDLE_PER_HOST,
        }
    }

    /// Sets the idle connections retained per host. Zero disables idle pooling.
    #[must_use]
    pub const fn with_max_idle_per_host(mut self, max_idle_per_host: usize) -> Self {
        self.max_idle_per_host = max_idle_per_host;
        self
    }

    /// DNS, TCP, and TLS budget.
    #[must_use]
    pub const fn connect(self) -> Duration {
        self.connect
    }

    /// Idle budget between socket reads.
    #[must_use]
    pub const fn read(self) -> Duration {
        self.read
    }

    /// Idle connections retained per host.
    #[must_use]
    pub const fn max_idle_per_host(self) -> usize {
        self.max_idle_per_host
    }
}

impl Default for HttpLimits {
    fn default() -> Self {
        Self::new(DEFAULT_CONNECT, DEFAULT_READ)
    }
}

/// [`HttpBackend`] over the process-wide pool.
#[derive(Clone, Debug)]
pub struct PooledHttp {
    profile: HttpProfile,
    limits: HttpLimits,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ClientKey {
    profile: HttpProfile,
    limits: HttpLimits,
}

impl PooledHttp {
    /// The client for a direct connection.
    #[must_use]
    pub fn direct() -> Self {
        Self::new(HttpProfile::direct())
    }

    /// The client for `profile`, using [`HttpLimits::default`].
    #[must_use]
    pub fn new(profile: HttpProfile) -> Self {
        Self::with_limits(profile, HttpLimits::default())
    }

    /// The client for `profile` and the host's timeout config.
    #[must_use]
    pub fn with_limits(profile: HttpProfile, limits: HttpLimits) -> Self {
        Self { profile, limits }
    }

    /// Builds the pooled client without sending.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the proxy URL or the client cannot be built.
    #[cfg(test)]
    pub(crate) fn ensure(&self) -> Result<(), PluginError> {
        client_for(&self.profile, self.limits).map(|_| ())
    }
}

static POOL: OnceLock<Mutex<BTreeMap<ClientKey, reqwest::Client>>> = OnceLock::new();

fn pool() -> &'static Mutex<BTreeMap<ClientKey, reqwest::Client>> {
    POOL.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Clients currently in the pool. Tests use this to show one profile is one client.
#[cfg(test)]
pub(crate) fn pooled_client_count() -> usize {
    pool().lock().map(|guard| guard.len()).unwrap_or(0)
}

fn client_for(profile: &HttpProfile, limits: HttpLimits) -> Result<reqwest::Client, PluginError> {
    let key = ClientKey {
        profile: profile.clone(),
        limits,
    };
    let mut guard = pool()
        .lock()
        .map_err(|_| PluginError::internal("http pool lock is poisoned"))?;
    if let Some(client) = guard.get(&key) {
        return Ok(client.clone());
    }
    let client = build_client(&key)?;
    guard.insert(key, client.clone());
    Ok(client)
}

fn build_client(key: &ClientKey) -> Result<reqwest::Client, PluginError> {
    // No whole-call timeout. `read` is per read and resets after each
    // successful read, so a long stream is not killed by a total budget.
    let mut builder = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .pool_max_idle_per_host(key.limits.max_idle_per_host())
        .connect_timeout(key.limits.connect())
        .read_timeout(key.limits.read())
        .danger_accept_invalid_certs(key.profile.insecure);
    if let Some(proxy) = &key.profile.proxy {
        let proxy = reqwest::Proxy::all(proxy)
            .map_err(|error| PluginError::config(format!("http proxy is invalid: {error}")))?;
        builder = builder.proxy(proxy);
    }
    builder
        .build()
        .map_err(|error| PluginError::connect(error.to_string()))
}

fn map_error(error: reqwest::Error) -> PluginError {
    // `is_connect` is DNS, TCP, or TLS setup, before the request is sent.
    // A connect timeout is that same pre-send failure when reqwest also
    // reports it as connect. A reset after the send and before a status is
    // `Transport` and is not retried for a non-idempotent call.
    if error.is_connect() {
        PluginError::connect(error.to_string())
    } else if error.is_timeout() {
        PluginError::timeout(error.to_string())
    } else {
        PluginError::transport(error.to_string())
    }
}

fn map_body(error: reqwest::Error) -> PluginError {
    let mut message = error.to_string();
    if error.is_timeout() && !message.contains("timed out") {
        message.push_str(": timed out");
    }
    PluginError::interrupted(message)
}

#[cfg(test)]
std::thread_local! {
    static SENDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn note_send() {
    SENDS.with(|sends| sends.set(sends.get() + 1));
}

#[cfg(test)]
fn take_sends() -> usize {
    SENDS.with(|sends| {
        let seen = sends.get();
        sends.set(0);
        seen
    })
}

#[async_trait]
impl HttpBackend for PooledHttp {
    async fn send(
        &self,
        request: HttpRequest<Body>,
    ) -> Result<HttpResponse<BodyStream>, PluginError> {
        #[cfg(test)]
        note_send();
        let client = client_for(&self.profile, self.limits)?;
        let method = reqwest::Method::from_bytes(request.method().as_str().as_bytes())
            .map_err(|error| PluginError::internal(error.to_string()))?;
        let url = request.uri().to_string();
        let mut outbound = client.request(method, &url);
        for (name, value) in request.headers() {
            let mut header = reqwest::header::HeaderValue::from_bytes(value.as_bytes())
                .map_err(|_| PluginError::internal(format!("invalid header {}", name.as_str())))?;
            header.set_sensitive(value.is_sensitive());
            outbound = outbound.header(name.as_str(), header);
        }
        let bytes = request.body().bytes().clone();
        let response = outbound.body(bytes).send().await.map_err(map_error)?;
        let status = response.status();
        let mut headers = response.headers().clone();
        let nominated = headers
            .get_all(reqwest::header::CONNECTION)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .filter_map(|name| reqwest::header::HeaderName::from_bytes(name.trim().as_bytes()).ok())
            .collect::<Vec<_>>();
        for name in [
            reqwest::header::CONTENT_LENGTH,
            reqwest::header::TRANSFER_ENCODING,
            reqwest::header::CONNECTION,
            reqwest::header::HeaderName::from_static("keep-alive"),
            reqwest::header::PROXY_AUTHENTICATE,
            reqwest::header::PROXY_AUTHORIZATION,
            reqwest::header::TE,
            reqwest::header::TRAILER,
            reqwest::header::UPGRADE,
            reqwest::header::HeaderName::from_static("proxy-connection"),
        ] {
            headers.remove(name);
        }
        for name in nominated {
            headers.remove(name);
        }
        // Set-Cookie stays because plugins are the HTTP clients and the host
        // keeps no cookie jar on their behalf.
        let success = status.is_success();
        let mut response = response;
        let mut collected = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => collected.extend_from_slice(&chunk),
                Ok(None) => break,
                Err(_) if !success => break,
                Err(error) => return Err(map_body(error)),
            }
        }
        let mut output = HttpResponse::new(BodyStream::from_bytes(collected));
        *output.status_mut() = status;
        *output.headers_mut() = headers;
        Ok(output)
    }

    fn pooled_client(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        client_for(&self.profile, self.limits)
            .ok()
            .map(|client| Arc::new(client) as Arc<dyn Any + Send + Sync>)
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use hofund_plugin_api::kernel::{HttpBackend, HttpRequest};
    use hofund_plugin_api::{Body, PluginErrorKind};
    use hofund_plugin_sdk::Call;

    use super::{HttpLimits, HttpProfile, PooledHttp, pooled_client_count, take_sends};
    use crate::context_for_http;

    async fn exclusive() -> tokio::sync::MutexGuard<'static, ()> {
        static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        LOCK.lock().await
    }

    fn serve(scripts: Vec<(String, Vec<u8>)>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            for (status, body) in scripts {
                let (mut stream, _) = listener.accept().expect("accept");
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("timeout");
                let mut buf = [0_u8; 2048];
                let _ = stream.read(&mut buf);
                let header = format!(
                    "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(header.as_bytes()).expect("write");
                stream.write_all(&body).expect("write");
            }
        });
        port
    }

    async fn get(
        port: u16,
        path: &str,
    ) -> hofund_plugin_api::kernel::HttpResponse<hofund_plugin_api::BodyStream> {
        let request = HttpRequest::builder()
            .method("GET")
            .uri(format!("http://127.0.0.1:{port}{path}"))
            .body(Body::empty())
            .expect("request");
        PooledHttp::direct().send(request).await.expect("send")
    }

    #[tokio::test]
    async fn one_profile_is_one_client_and_status_is_returned() {
        let _guard = exclusive().await;
        let before = pooled_client_count();
        PooledHttp::direct().ensure().expect("direct");
        let direct = pooled_client_count();
        assert!(direct == before || direct == before + 1);

        let proxy = HttpProfile::direct().with_proxy("http://127.0.0.1:9");
        PooledHttp::new(proxy.clone()).ensure().expect("proxy");
        assert_eq!(pooled_client_count(), direct + 1);
        PooledHttp::new(proxy).ensure().expect("proxy again");
        assert_eq!(pooled_client_count(), direct + 1);
        PooledHttp::new(HttpProfile::direct().insecure())
            .ensure()
            .expect("insecure");
        assert_eq!(pooled_client_count(), direct + 2);
        let slow = HttpLimits::new(Duration::from_secs(3), Duration::from_secs(4));
        PooledHttp::with_limits(HttpProfile::direct(), slow)
            .ensure()
            .expect("limits");
        assert_eq!(pooled_client_count(), direct + 3);
        PooledHttp::with_limits(HttpProfile::direct(), slow)
            .ensure()
            .expect("limits again");
        assert_eq!(pooled_client_count(), direct + 3);
        PooledHttp::with_limits(HttpProfile::direct(), slow.with_max_idle_per_host(0))
            .ensure()
            .expect("no idle pool");
        assert_eq!(pooled_client_count(), direct + 4);

        let settled = pooled_client_count();
        let port = serve(vec![
            ("200 OK".to_owned(), b"pong".to_vec()),
            ("500 ERR".to_owned(), b"no".to_vec()),
        ]);
        let ok = get(port, "/").await;
        assert_eq!(ok.status().as_u16(), 200);
        let chunk = ok
            .into_body()
            .next_chunk()
            .await
            .expect("chunk")
            .expect("bytes");
        assert_eq!(&chunk[..], b"pong");
        let failed = get(port, "/fail").await;
        assert_eq!(failed.status().as_u16(), 500);
        assert_eq!(pooled_client_count(), settled);
    }

    #[test]
    fn a_bad_proxy_url_is_config() {
        let error = PooledHttp::new(HttpProfile::direct().with_proxy("not a proxy"))
            .ensure()
            .expect_err("proxy");
        assert_eq!(error.kind(), PluginErrorKind::Config);
    }

    enum Action {
        ResetAfterBody,
        ResetAfterHeaders,
        Truncated,
        Respond(&'static [u8]),
        Hold,
    }

    fn script(actions: Vec<Action>) -> (u16, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let hits = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&hits);
        std::thread::spawn(move || {
            for action in actions {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                count.fetch_add(1, Ordering::SeqCst);
                let _ = stream.set_nodelay(true);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                match action {
                    Action::ResetAfterBody => {
                        let _ = read_request(&mut stream, true);
                        reset(stream);
                    }
                    Action::ResetAfterHeaders => {
                        let _ = read_request(&mut stream, false);
                        reset(stream);
                    }
                    Action::Truncated => {
                        let _ = read_request(&mut stream, false);
                        let _ = stream.write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-length: 4\r\nconnection: close\r\n\r\n{",
                        );
                    }
                    Action::Respond(body) => {
                        let _ = read_request(&mut stream, false);
                        respond(stream, body);
                    }
                    Action::Hold => {
                        let _ = read_request(&mut stream, false);
                        let mut buf = [0_u8; 8];
                        let _ = stream.read(&mut buf);
                    }
                }
            }
        });
        (port, hits)
    }

    fn read_request(stream: &mut TcpStream, want_body: bool) -> Vec<u8> {
        let mut buf = Vec::new();
        let mut tmp = [0_u8; 1024];
        loop {
            match stream.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    if request_complete(&buf, want_body) {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        buf
    }

    fn request_complete(buf: &[u8], want_body: bool) -> bool {
        let Some(end) = buf.windows(4).position(|window| window == b"\r\n\r\n") else {
            return false;
        };
        if !want_body {
            return true;
        }
        let headers = &buf[..end];
        let length = content_length(headers);
        buf.len() >= end + 4 + length
    }

    fn content_length(headers: &[u8]) -> usize {
        let text = String::from_utf8_lossy(headers);
        for line in text.split("\r\n") {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            if name.eq_ignore_ascii_case("content-length") {
                return value.trim().parse().unwrap_or(0);
            }
        }
        0
    }

    fn reset(stream: TcpStream) {
        drop(stream);
    }

    fn respond(mut stream: TcpStream, body: &[u8]) {
        let header = format!(
            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(header.as_bytes());
        let _ = stream.write_all(body);
    }

    fn http(limits: HttpLimits) -> hofund_plugin_api::HostContext {
        context_for_http(Arc::new(PooledHttp::with_limits(
            HttpProfile::direct(),
            limits,
        )))
    }

    async fn wait_until(hits: &AtomicUsize, want: usize) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while hits.load(Ordering::SeqCst) < want {
            assert!(
                tokio::time::Instant::now() < deadline,
                "saw {} connections",
                hits.load(Ordering::SeqCst)
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn a_post_that_resets_after_the_body_is_not_retried() {
        let _guard = exclusive().await;
        let (port, hits) = script(vec![
            Action::ResetAfterBody,
            Action::ResetAfterBody,
            Action::ResetAfterBody,
        ]);
        let context = http(HttpLimits::new(
            Duration::from_secs(2),
            Duration::from_secs(2),
        ));
        let error = Call::post(format!("http://127.0.0.1:{port}/v1"))
            .bytes(b"hello")
            .send_with_retry(context.http(), 3)
            .await
            .expect_err("reset");
        assert_eq!(error.kind(), PluginErrorKind::Transport);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_200_body_break_is_retried_only_for_idempotent_methods() {
        let _guard = exclusive().await;
        let limits = HttpLimits::new(Duration::from_secs(2), Duration::from_secs(2));
        let (port, hits) = script(vec![
            Action::Truncated,
            Action::Truncated,
            Action::Truncated,
        ]);
        let context = http(limits);
        let error = Call::post(format!("http://127.0.0.1:{port}/v1"))
            .bytes(b"hello")
            .send_with_retry(context.http(), 3)
            .await
            .expect_err("truncated");
        assert_eq!(error.kind(), PluginErrorKind::Interrupted);
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        let (port, hits) = script(vec![
            Action::Truncated,
            Action::Truncated,
            Action::Respond(b"ok"),
        ]);
        let context = http(limits);
        let reply = Call::get(format!("http://127.0.0.1:{port}/v1"))
            .send_with_retry(context.http(), 3)
            .await
            .expect("retried");
        assert_eq!(reply.body(), b"ok");
        assert_eq!(hits.load(Ordering::SeqCst), 3);
    }

    fn raw(response: &'static [u8]) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let _ = read_request(&mut stream, false);
            let _ = stream.write_all(response);
        });
        port
    }

    #[tokio::test]
    async fn a_429_preserves_status_and_retry_after_through_host_and_sdk() {
        const COMPLETE: &[u8] = b"HTTP/1.1 429 Too Many Requests\r\ncontent-length: 1\r\nretry-after: 120\r\nconnection: close\r\n\r\n{";
        const TRUNCATED: &[u8] = b"HTTP/1.1 429 Too Many Requests\r\ncontent-length: 4\r\nretry-after: 120\r\nconnection: x-foo\r\nkeep-alive: timeout=5\r\nproxy-connection: keep-alive\r\nproxy-authenticate: Basic\r\nproxy-authorization: Basic secret\r\nte: trailers\r\ntrailer: x-checksum\r\nupgrade: h2c\r\nx-foo: strip\r\nx-ordinary: survive\r\nset-cookie: session=plugin\r\n\r\n{";

        let _guard = exclusive().await;
        let port = raw(TRUNCATED);
        let request = HttpRequest::builder()
            .method("POST")
            .uri(format!("http://127.0.0.1:{port}/v1"))
            .body(Body::from_bytes(b"hello".to_vec()))
            .expect("request");
        let response = PooledHttp::direct()
            .send(request)
            .await
            .expect("status response");
        assert_eq!(response.status().as_u16(), 429);
        assert_eq!(
            response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok()),
            Some("120")
        );
        for name in [
            "connection",
            "content-length",
            "keep-alive",
            "proxy-authenticate",
            "proxy-authorization",
            "proxy-connection",
            "te",
            "trailer",
            "transfer-encoding",
            "upgrade",
            "x-foo",
        ] {
            assert!(!response.headers().contains_key(name), "{name}");
        }
        assert_eq!(
            response
                .headers()
                .get("x-ordinary")
                .and_then(|value| value.to_str().ok()),
            Some("survive")
        );
        assert_eq!(
            response
                .headers()
                .get(reqwest::header::SET_COOKIE)
                .and_then(|value| value.to_str().ok()),
            Some("session=plugin")
        );
        let body = response
            .into_body()
            .next_chunk()
            .await
            .expect("chunk")
            .expect("partial body");
        assert_eq!(&body[..], b"{");

        for response in [COMPLETE, TRUNCATED] {
            let port = raw(response);
            let context = http(HttpLimits::default());
            let error = Call::post(format!("http://127.0.0.1:{port}/v1"))
                .bytes(b"hello")
                .send(context.http())
                .await
                .expect_err("rate limited");
            assert_eq!(error.kind(), PluginErrorKind::Status);
            assert_eq!(error.status(), Some(429));
            assert_eq!(error.retry_after(), Some(Duration::from_secs(120)));
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_connect_refusal_is_retried() {
        let _guard = exclusive().await;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);
        let _ = take_sends();
        let context = http(HttpLimits::default());
        let error = Call::post(format!("http://127.0.0.1:{port}/v1"))
            .bytes(b"hello")
            .send_with_retry(context.http(), 3)
            .await
            .expect_err("refused");
        assert_eq!(error.kind(), PluginErrorKind::Connect);
        assert_eq!(take_sends(), 3);
    }

    #[tokio::test]
    async fn a_get_is_retried_after_a_mid_flight_reset() {
        let _guard = exclusive().await;
        let (port, hits) = script(vec![
            Action::ResetAfterHeaders,
            Action::ResetAfterHeaders,
            Action::Respond(b"ok"),
        ]);
        let context = http(HttpLimits::new(
            Duration::from_secs(2),
            Duration::from_secs(2),
        ));
        let reply = Call::get(format!("http://127.0.0.1:{port}/v1"))
            .send_with_retry(context.http(), 3)
            .await
            .expect("retried");
        assert_eq!(reply.body(), b"ok");
        assert_eq!(hits.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn dropping_the_call_does_not_send_again() {
        let _guard = exclusive().await;
        let (port, hits) = script(vec![Action::Hold, Action::Hold]);
        let context = http(HttpLimits::new(
            Duration::from_secs(2),
            Duration::from_secs(2),
        ));
        let url = format!("http://127.0.0.1:{port}/hold");
        let task = tokio::spawn(async move {
            let _ = Call::get(url).send_with_retry(context.http(), 3).await;
        });
        wait_until(&hits, 1).await;
        task.abort();
        let _ = task.await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_stream_survives_past_one_read_budget_and_a_stall_is_interrupted() {
        let _guard = exclusive().await;
        let limits = HttpLimits::new(Duration::from_secs(2), Duration::from_millis(400));
        let (port, _) = drip(6, Duration::from_millis(80));
        let context = http(limits);
        let reply = Call::get(format!("http://127.0.0.1:{port}/stream"))
            .send(context.http())
            .await
            .expect("stream");
        assert_eq!(reply.body(), b"abcdef");

        let (port, _) = stall();
        let context = http(limits);
        let started = std::time::Instant::now();
        let error = Call::get(format!("http://127.0.0.1:{port}/stall"))
            .send(context.http())
            .await
            .expect_err("stall");
        assert_eq!(error.kind(), PluginErrorKind::Interrupted);
        assert!(error.message().contains("timed out"), "{}", error.message());
        assert!(started.elapsed() < Duration::from_millis(1500));
    }

    fn drip(bytes: usize, gap: Duration) -> (u16, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let hits = Arc::new(AtomicUsize::new(0));
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let _ = stream.set_nodelay(true);
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let _ = read_request(&mut stream, false);
            let header =
                format!("HTTP/1.1 200 OK\r\ncontent-length: {bytes}\r\nconnection: close\r\n\r\n");
            let _ = stream.write_all(header.as_bytes());
            for index in 0..bytes {
                let _ = stream.write_all(&[b'a' + u8::try_from(index).unwrap_or(b'a')]);
                let _ = stream.flush();
                std::thread::sleep(gap);
            }
        });
        (port, hits)
    }

    fn stall() -> (u16, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let hits = Arc::new(AtomicUsize::new(0));
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let _ = read_request(&mut stream, false);
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 4\r\nconnection: close\r\n\r\n");
            let mut buf = [0_u8; 8];
            let _ = stream.read(&mut buf);
        });
        (port, hits)
    }
}
