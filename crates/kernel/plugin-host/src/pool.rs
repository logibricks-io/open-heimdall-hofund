//! Process-wide HTTP clients. One client per proxy and TLS profile.
//!
//! The pool is the `reqwest` client in the plugin stack. Plugins call
//! [`crate::host_context`]'s [`HttpHandle`](hofund_plugin_api::HttpHandle), and
//! the SDK builds the request. A success body streams as it arrives. Built-in
//! adapters that have not moved to the handle yet take the same client from
//! [`HttpBackend::pooled_client`](hofund_plugin_api::kernel::HttpBackend::pooled_client).
//! This module does not add a profile argument to
//! [`HttpBackend`](hofund_plugin_api::kernel::HttpBackend): the profile is
//! chosen when the handle is built.

use std::any::Any;
use std::collections::BTreeMap;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use hofund_plugin_api::kernel::{HttpBackend, HttpRequest, HttpResponse};
use hofund_plugin_api::{Body, BodyStream, DuplexRequest, DuplexSession, PluginError, read_budget};

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

    /// Same connect and idle-pool settings with a different read budget.
    #[must_use]
    pub const fn with_read(mut self, read: Duration) -> Self {
        self.read = read;
        self
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ClientMode {
    Unary,
    Duplex,
}

/// Cleartext prior knowledge versus TLS ALPN for the duplex pool slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum DuplexDial {
    Alpn,
    PriorKnowledge,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ClientKey {
    profile: HttpProfile,
    limits: HttpLimits,
    mode: ClientMode,
    duplex_dial: DuplexDial,
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
        client_for(
            &self.profile,
            self.limits,
            ClientMode::Unary,
            DuplexDial::Alpn,
        )
        .map(|_| ())
    }

    /// Profile and limits that select this backend's pooled client.
    #[must_use]
    pub fn pool_key(&self) -> PooledClientKey {
        PooledClientKey {
            profile: self.profile.clone(),
            limits: self.limits,
        }
    }
}

/// Selects one entry in the process-wide HTTP client pool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PooledClientKey {
    /// Proxy and TLS profile.
    pub profile: HttpProfile,
    /// Connect, read, and idle-pool limits.
    pub limits: HttpLimits,
}

static POOL: OnceLock<Mutex<BTreeMap<ClientKey, Arc<RwLock<reqwest::Client>>>>> = OnceLock::new();

#[cfg(test)]
static TEST_REFRESH_CALLS: AtomicUsize = AtomicUsize::new(0);
/// Replaces the pooled client for `profile` so the next send uses a new connection.
pub fn close_idle_after_region(profile: &HttpProfile, limits: HttpLimits) {
    let key = ClientKey {
        profile: profile.clone(),
        limits,
        mode: ClientMode::Unary,
        duplex_dial: DuplexDial::Alpn,
    };
    let Ok(guard) = pool().lock() else {
        return;
    };
    let Some(shared) = guard.get(&key) else {
        return;
    };
    let _ = refresh_shared(shared, &key);
}

/// Replaces the duplex client for `profile` and `dial`.
///
/// A header-timeout abort can leave the HTTP/2 connection active on the
/// server (no idle slot; the first stream is still open). The next Run
/// would otherwise multiplex onto that session and can read garbage as
/// Connect frames. A new client forces a fresh dial.
pub(crate) fn refresh_duplex_client(
    profile: &HttpProfile,
    limits: HttpLimits,
    dial: DuplexDial,
) -> Result<(), PluginError> {
    let key = ClientKey {
        profile: profile.clone(),
        limits,
        mode: ClientMode::Duplex,
        duplex_dial: dial,
    };
    let shared = client_handle(profile, limits, ClientMode::Duplex, dial)?;
    refresh_shared(&shared, &key)
}

/// Evicts `key` and returns a new pooled client with the same settings.
///
/// # Errors
///
/// Returns [`PluginError`] when the client cannot be built.
pub fn fresh_pooled_client(
    profile: &HttpProfile,
    limits: HttpLimits,
) -> Result<reqwest::Client, PluginError> {
    let key = ClientKey {
        profile: profile.clone(),
        limits,
        mode: ClientMode::Unary,
        duplex_dial: DuplexDial::Alpn,
    };
    let shared = client_handle(profile, limits, ClientMode::Unary, DuplexDial::Alpn)?;
    refresh_shared(&shared, &key)?;
    Ok(shared
        .read()
        .map_err(|_| PluginError::internal("http pool client lock is poisoned"))?
        .clone())
}

fn pool() -> &'static Mutex<BTreeMap<ClientKey, Arc<RwLock<reqwest::Client>>>> {
    POOL.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn refresh_shared(
    shared: &Arc<RwLock<reqwest::Client>>,
    key: &ClientKey,
) -> Result<(), PluginError> {
    #[cfg(test)]
    TEST_REFRESH_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let fresh = build_client(key)?;
    *shared
        .write()
        .map_err(|_| PluginError::internal("http pool client lock is poisoned"))? = fresh;
    Ok(())
}

/// Clients currently in the pool. Tests use this to show one profile is one client.
#[cfg(test)]
pub(crate) fn pooled_client_count() -> usize {
    pool().lock().map(|guard| guard.len()).unwrap_or(0)
}

fn client_handle(
    profile: &HttpProfile,
    limits: HttpLimits,
    mode: ClientMode,
    duplex_dial: DuplexDial,
) -> Result<Arc<RwLock<reqwest::Client>>, PluginError> {
    let key = ClientKey {
        profile: profile.clone(),
        limits,
        mode,
        duplex_dial,
    };
    let mut guard = pool()
        .lock()
        .map_err(|_| PluginError::internal("http pool lock is poisoned"))?;
    if let Some(shared) = guard.get(&key) {
        return Ok(Arc::clone(shared));
    }
    let shared = Arc::new(RwLock::new(build_client(&key)?));
    guard.insert(key, Arc::clone(&shared));
    Ok(shared)
}

fn client_for(
    profile: &HttpProfile,
    limits: HttpLimits,
    mode: ClientMode,
    duplex_dial: DuplexDial,
) -> Result<reqwest::Client, PluginError> {
    let shared = client_handle(profile, limits, mode, duplex_dial)?;
    Ok(shared
        .read()
        .map_err(|_| PluginError::internal("http pool client lock is poisoned"))?
        .clone())
}

pub(crate) fn duplex_client_for(
    profile: &HttpProfile,
    dial: DuplexDial,
) -> Result<reqwest::Client, PluginError> {
    client_for(
        profile,
        crate::duplex::duplex_limits(),
        ClientMode::Duplex,
        dial,
    )
}

#[cfg(test)]
pub(crate) fn duplex_client_for_read(
    profile: &HttpProfile,
    dial: DuplexDial,
    read: Duration,
) -> Result<reqwest::Client, PluginError> {
    client_for(
        profile,
        crate::duplex::duplex_limits().with_read(read),
        ClientMode::Duplex,
        dial,
    )
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
    match key.mode {
        ClientMode::Unary => {
            builder = builder.http1_only();
        }
        ClientMode::Duplex => match key.duplex_dial {
            DuplexDial::PriorKnowledge | DuplexDial::Alpn => {
                builder = builder.http2_prior_knowledge();
            }
        },
    }
    if let Some(proxy) = &key.profile.proxy {
        let proxy = reqwest::Proxy::all(proxy)
            .map_err(|error| PluginError::config(format!("http proxy is invalid: {error}")))?;
        builder = builder.proxy(proxy);
    }
    builder
        .build()
        .map_err(|error| PluginError::connect(error.to_string()))
}

fn limits_for_request(default: HttpLimits, request: &HttpRequest<Body>) -> HttpLimits {
    read_budget(request).map_or(default, |read| default.with_read(read))
}

fn client_for_unary(
    profile: &HttpProfile,
    pool_limits: HttpLimits,
    request: &HttpRequest<Body>,
) -> Result<reqwest::Client, PluginError> {
    let effective = limits_for_request(pool_limits, request);
    if effective.read() == pool_limits.read() {
        return client_for(profile, pool_limits, ClientMode::Unary, DuplexDial::Alpn);
    }
    build_client(&ClientKey {
        profile: profile.clone(),
        limits: effective,
        mode: ClientMode::Unary,
        duplex_dial: DuplexDial::Alpn,
    })
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
    if error.is_timeout() {
        let mut message = error.to_string();
        if !message.contains("timed out") {
            message.push_str(": timed out");
        }
        return PluginError::timeout(message);
    }
    PluginError::interrupted(error.to_string())
}

fn map_duplex_body(error: reqwest::Error) -> PluginError {
    if error.is_timeout() {
        let mut message = error.to_string();
        if !message.contains("timed out") {
            message.push_str(": timed out");
        }
        return PluginError::interrupted(message);
    }
    PluginError::interrupted(error.to_string())
}

/// A success body streamed chunk by chunk as the upstream sends it.
///
/// A read failure becomes one [`PluginError::interrupted`] chunk and ends the
/// stream. Dropping the stream drops the response and closes the connection.
pub(crate) fn live_body(response: reqwest::Response) -> BodyStream {
    BodyStream::from_stream(futures_util::stream::unfold(
        Some(response),
        |state| async move {
            let mut response = state?;
            match response.chunk().await {
                Ok(Some(chunk)) => Some((Ok(chunk), Some(response))),
                Ok(None) => None,
                Err(error) => Some((Err(map_body(error)), None)),
            }
        },
    ))
}

/// Response body for an open duplex session; aborts with an error chunk when the guard fires.
pub(crate) fn live_duplex_body(
    response: reqwest::Response,
    aborted: Arc<AtomicBool>,
) -> BodyStream {
    BodyStream::from_stream(futures_util::stream::unfold(Some(response), move |state| {
        let aborted = Arc::clone(&aborted);
        async move {
            if aborted.load(Ordering::Relaxed) {
                return Some((
                    Err(PluginError::interrupted("duplex response aborted")),
                    None,
                ));
            }
            let mut response = state?;
            match response.chunk().await {
                Ok(Some(chunk)) => Some((Ok(chunk), Some(response))),
                Ok(None) => None,
                Err(error) => Some((Err(map_duplex_body(error)), None)),
            }
        }
    }))
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
        let client = client_for_unary(&self.profile, self.limits, &request)?;
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
        let mut response = outbound.body(bytes).send().await.map_err(map_error)?;
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
        // RFC 9110 section 7.6.1 makes every Connection-nominated header
        // hop-by-hop, including Retry-After.
        for name in nominated {
            headers.remove(name);
        }
        // Set-Cookie stays because plugins are the HTTP clients and the host
        // keeps no cookie jar on their behalf.
        let body = if status.is_success() {
            live_body(response)
        } else {
            // An error body is short and only read for its message, so it is
            // buffered, and a break while reading it keeps what arrived.
            let mut collected = Vec::new();
            while let Ok(Some(chunk)) = response.chunk().await {
                collected.extend_from_slice(&chunk);
            }
            BodyStream::from_bytes(collected)
        };
        let mut output = HttpResponse::new(body);
        *output.status_mut() = status;
        *output.headers_mut() = headers;
        Ok(output)
    }

    async fn open_duplex(&self, request: DuplexRequest) -> Result<DuplexSession, PluginError> {
        crate::duplex::open_duplex(self, request).await
    }

    fn pooled_client(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        client_handle(
            &self.profile,
            self.limits,
            ClientMode::Unary,
            DuplexDial::Alpn,
        )
        .ok()
        // Box the shared handle itself so consumers downcast to
        // `Arc<RwLock<reqwest::Client>>` and see every later refresh.
        .map(|shared| Arc::new(shared) as Arc<dyn Any + Send + Sync>)
    }

    fn fresh_client(&self) -> Option<Result<Arc<dyn Any + Send + Sync>, PluginError>> {
        Some(
            fresh_pooled_client(&self.profile, self.limits)
                .map(|client| Arc::new(client) as Arc<dyn Any + Send + Sync>),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, RwLock};
    use std::time::Duration;

    use hofund_plugin_api::kernel::{HttpBackend, HttpRequest};
    use hofund_plugin_api::{Body, PluginErrorKind};
    use hofund_plugin_sdk::Call;

    use super::{
        HttpLimits, HttpProfile, PooledHttp, close_idle_after_region, pooled_client_count,
        take_sends,
    };
    use crate::context_for_http;

    async fn exclusive() -> tokio::sync::MutexGuard<'static, ()> {
        static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        super::TEST_REFRESH_CALLS.store(0, Ordering::SeqCst);
        LOCK.lock().await
    }

    fn refresh_calls() -> usize {
        super::TEST_REFRESH_CALLS.load(Ordering::SeqCst)
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
        raw_accepts(response, 1)
    }

    fn raw_accepts(response: &'static [u8], connections: usize) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            for _ in 0..connections {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let _ = read_request(&mut stream, false);
                let _ = stream.write_all(response);
            }
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

    #[tokio::test]
    async fn connection_nominates_retry_after_as_hop_by_hop() {
        const RESPONSE: &[u8] = b"HTTP/1.1 429 Too Many Requests\r\ncontent-length: 1\r\nconnection: retry-after\r\nretry-after: 5\r\n\r\n{";

        let _guard = exclusive().await;
        let port = raw(RESPONSE);
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
        assert!(
            !response
                .headers()
                .contains_key(reqwest::header::RETRY_AFTER)
        );
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
        // Inter-chunk gap must sit clearly below the 400ms per-read idle
        // budget, while the full drip must last clearly longer than one
        // budget. Six 80ms gaps put the last byte at exactly 400ms, which
        // failed on macOS CI as `error decoding response body: timed out`.
        let (port, _) = drip(12, Duration::from_millis(50));
        let context = http(limits);
        let reply = Call::get(format!("http://127.0.0.1:{port}/stream"))
            .send(context.http())
            .await
            .expect("stream");
        assert_eq!(reply.body(), b"abcdefghijkl");

        let (port, _) = stall();
        let context = http(limits);
        let started = std::time::Instant::now();
        let error = Call::get(format!("http://127.0.0.1:{port}/stall"))
            .send(context.http())
            .await
            .expect_err("stall");
        assert_eq!(error.kind(), PluginErrorKind::Timeout);
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

    fn hold_after_headers() -> (
        u16,
        Arc<AtomicUsize>,
        Arc<Mutex<Option<std::time::Instant>>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let hits = Arc::new(AtomicUsize::new(0));
        let headers_written = Arc::new(Mutex::new(None));
        std::thread::spawn({
            let hits = Arc::clone(&hits);
            let headers_written = Arc::clone(&headers_written);
            move || {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                hits.fetch_add(1, Ordering::SeqCst);
                let _ = read_request(&mut stream, false);
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-length: 4\r\nconnection: close\r\n\r\n",
                );
                *headers_written.lock().expect("headers time") = Some(std::time::Instant::now());
                std::thread::sleep(Duration::from_secs(30));
            }
        });
        (port, hits, headers_written)
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

    /// Serves one 200 response: `first`, then waits for `release`, then
    /// `rest` (or closes early when `rest` is `None`).
    fn gated(
        first: &'static [u8],
        rest: Option<&'static [u8]>,
        total: usize,
    ) -> (u16, std::sync::mpsc::Sender<()>) {
        gated_status("200 OK", first, rest, total)
    }

    /// Like `gated`, with the status line given.
    fn gated_status(
        status: &'static str,
        first: &'static [u8],
        rest: Option<&'static [u8]>,
        total: usize,
    ) -> (u16, std::sync::mpsc::Sender<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let (release, released) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let _ = stream.set_nodelay(true);
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let _ = read_request(&mut stream, false);
            let header = format!(
                "HTTP/1.1 {status}\r\ncontent-length: {total}\r\nconnection: close\r\n\r\n"
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(first);
            let _ = stream.flush();
            let _ = released.recv_timeout(Duration::from_secs(5));
            if let Some(rest) = rest {
                let _ = stream.write_all(rest);
            }
        });
        (port, release)
    }

    fn get_request(port: u16) -> HttpRequest<Body> {
        HttpRequest::builder()
            .method("GET")
            .uri(format!("http://127.0.0.1:{port}/sse"))
            .body(Body::empty())
            .expect("request")
    }

    #[tokio::test]
    async fn a_success_body_streams_before_the_upstream_finishes() {
        let _guard = exclusive().await;
        let (port, release) = gated(b"data: one\n\n", Some(b"data: two\n\n"), 22);
        let mut response = PooledHttp::direct()
            .send(get_request(port))
            .await
            .expect("send");
        assert_eq!(response.status().as_u16(), 200);
        // The upstream holds the rest until released, so a buffering send
        // would never return the first chunk here.
        let first = tokio::time::timeout(Duration::from_secs(2), response.body_mut().next_chunk())
            .await
            .expect("first chunk before the upstream finished")
            .expect("chunk")
            .expect("ok");
        assert_eq!(&first[..], b"data: one\n\n");
        release.send(()).expect("release");
        let mut rest = Vec::new();
        while let Some(chunk) = response.body_mut().next_chunk().await {
            rest.extend_from_slice(&chunk.expect("ok"));
        }
        assert_eq!(rest, b"data: two\n\n");
    }

    #[tokio::test]
    async fn duplex_response_body_read_timeout_is_interrupted_not_timeout() {
        let _guard = exclusive().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let Ok(mut connection) = h2::server::handshake(stream).await else {
                        return;
                    };
                    while let Some(result) = connection.accept().await {
                        let Ok((request, mut respond)) = result else {
                            continue;
                        };
                        tokio::spawn(async move {
                            let response = http::Response::builder()
                                .status(200)
                                .body(())
                                .expect("response");
                            let Ok(mut send) = respond.send_response(response, false) else {
                                return;
                            };
                            let _ = send.send_data(bytes::Bytes::from_static(b"hold"), false);
                            let _ = request.into_body();
                            tokio::time::sleep(Duration::from_secs(30)).await;
                        });
                    }
                });
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let client = super::duplex_client_for_read(
            &HttpProfile::direct(),
            super::DuplexDial::PriorKnowledge,
            Duration::from_millis(400),
        )
        .expect("client");
        let response = client
            .post(format!("http://127.0.0.1:{port}/duplex"))
            .body(reqwest::Body::from("x"))
            .send()
            .await
            .expect("headers");
        let mut body = super::live_duplex_body(response, Arc::new(AtomicBool::new(false)));
        let first = body.next_chunk().await.expect("first chunk").expect("ok");
        assert_eq!(&first[..], b"hold");
        let started = std::time::Instant::now();
        let error = body
            .next_chunk()
            .await
            .expect("timeout chunk")
            .expect_err("stall");
        assert_eq!(
            error.kind(),
            PluginErrorKind::Interrupted,
            "{}",
            error.message()
        );
        assert_ne!(error.kind(), PluginErrorKind::Timeout);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn a_success_body_break_is_an_interrupted_chunk_after_the_data() {
        let _guard = exclusive().await;
        let (port, release) = gated(b"data: one\n\n", None, 64);
        let mut response = PooledHttp::direct()
            .send(get_request(port))
            .await
            .expect("headers arrive before the break");
        let first = response
            .body_mut()
            .next_chunk()
            .await
            .expect("chunk")
            .expect("ok");
        assert_eq!(&first[..], b"data: one\n\n");
        release.send(()).expect("release");
        let error = response
            .body_mut()
            .next_chunk()
            .await
            .expect("an error chunk")
            .expect_err("truncated");
        assert_eq!(error.kind(), PluginErrorKind::Interrupted);
        assert!(response.body_mut().next_chunk().await.is_none());
    }

    #[tokio::test]
    async fn an_error_body_is_buffered_and_a_break_keeps_what_arrived() {
        let _guard = exclusive().await;
        // The body arrives in two writes and then breaks short of its
        // length, so the buffer must keep reading past the first chunk.
        let (port, release) = gated_status("503 Service Unavailable", b"bu", Some(b"sy"), 64);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            let _ = release.send(());
        });
        let mut response = PooledHttp::direct()
            .send(get_request(port))
            .await
            .expect("send");
        assert_eq!(response.status().as_u16(), 503);
        let body = response
            .body_mut()
            .next_chunk()
            .await
            .expect("chunk")
            .expect("ok");
        assert_eq!(&body[..], b"busy");
        assert!(response.body_mut().next_chunk().await.is_none());
    }

    #[tokio::test]
    async fn pooled_client_sends_an_http11_request_line() {
        let _guard = exclusive().await;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let (request_tx, request_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(1);
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let request = read_request(&mut stream, false);
            let _ = request_tx.send(request);
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
        });
        let _ = PooledHttp::direct()
            .send(get_request(port))
            .await
            .expect("send");
        let request = request_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("request bytes");
        let first_line = request
            .split(|byte| *byte == b'\n')
            .next()
            .expect("request line");
        assert!(
            first_line.ends_with(b" HTTP/1.1\r"),
            "request line was {:?}",
            String::from_utf8_lossy(first_line)
        );
    }

    #[tokio::test]
    async fn pooled_client_sends_an_http11_request_line_over_https_when_alpn_offers_h2_first() {
        let _guard = exclusive().await;
        let captured = Arc::new(Mutex::new(Vec::new()));
        let port = crate::test_tls::spawn_tls_capture(
            vec![b"h2".to_vec(), b"http/1.1".to_vec()],
            Arc::clone(&captured),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let request = HttpRequest::builder()
            .method("GET")
            .uri(format!("https://127.0.0.1:{port}/probe"))
            .body(Body::empty())
            .expect("request");
        PooledHttp::new(HttpProfile::direct().insecure())
            .send(request)
            .await
            .expect("send");
        let bytes = captured.lock().expect("capture").clone();
        assert!(
            !bytes.starts_with(b"PRI * HTTP/2.0"),
            "client must not send the HTTP/2 connection preface: {:?}",
            String::from_utf8_lossy(&bytes)
        );
        let first_line = bytes
            .split(|byte| *byte == b'\n')
            .next()
            .expect("request line");
        assert!(
            first_line.ends_with(b" HTTP/1.1\r"),
            "request line was {:?}",
            String::from_utf8_lossy(first_line)
        );
    }

    #[tokio::test]
    async fn an_empty_success_body_yields_no_chunks() {
        let _guard = exclusive().await;
        let port = raw(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
        let mut response = PooledHttp::direct()
            .send(get_request(port))
            .await
            .expect("send");
        assert_eq!(response.status().as_u16(), 200);
        assert!(response.body_mut().next_chunk().await.is_none());
    }

    #[tokio::test]
    async fn dropping_a_streamed_body_closes_the_connection() {
        let _guard = exclusive().await;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let (closed_tx, closed) = std::sync::mpsc::channel::<bool>();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let _ = stream.set_nodelay(true);
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let _ = read_request(&mut stream, false);
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 64\r\n\r\ndata: one\n\n");
            let _ = stream.flush();
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let mut buffer = [0_u8; 16];
            // EOF or a reset means the client closed; a timeout means it
            // kept the connection open.
            let closed = match std::io::Read::read(&mut stream, &mut buffer) {
                Ok(0) => true,
                Ok(_) => false,
                Err(error) => !matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ),
            };
            let _ = closed_tx.send(closed);
        });
        let mut response = PooledHttp::direct()
            .send(get_request(port))
            .await
            .expect("send");
        let first = response
            .body_mut()
            .next_chunk()
            .await
            .expect("chunk")
            .expect("ok");
        assert_eq!(&first[..], b"data: one\n\n");
        drop(response);
        let closed =
            tokio::task::spawn_blocking(move || closed.recv_timeout(Duration::from_secs(8)))
                .await
                .expect("join")
                .expect("server result");
        assert!(
            closed,
            "dropping the body must close the upstream connection"
        );
    }

    #[tokio::test]
    async fn call_replies_are_unchanged_for_chunked_bodies() {
        const OK: &[u8] = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n5\r\nhello\r\n1\r\n \r\n5\r\nworld\r\n0\r\n\r\n";
        const BUSY: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n2\r\nbu\r\n2\r\nsy\r\n0\r\n\r\n";

        let _guard = exclusive().await;
        let context = http(HttpLimits::default());
        let port = raw(OK);
        let reply = Call::get(format!("http://127.0.0.1:{port}/v1"))
            .send(context.http())
            .await
            .expect("chunked reply");
        assert_eq!(reply.status(), 200);
        assert_eq!(reply.body(), b"hello world");

        let port = raw(BUSY);
        let error = Call::get(format!("http://127.0.0.1:{port}/v1"))
            .send(context.http())
            .await
            .expect_err("chunked error");
        assert_eq!(error.kind(), PluginErrorKind::Status);
        assert_eq!(error.status(), Some(503));
        assert_eq!(error.message(), "busy");
    }

    #[tokio::test]
    async fn open_returns_a_2xx_body_unread_and_an_error_status_as_send_does() {
        const OK: &[u8] = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        const BUSY: &[u8] = b"HTTP/1.1 429 Too Many Requests\r\nretry-after: 3\r\ncontent-length: 4\r\nconnection: close\r\n\r\nbusy";

        let _guard = exclusive().await;
        let context = http(HttpLimits::default());
        let port = raw(OK);
        let response = Call::get(format!("http://127.0.0.1:{port}/v1"))
            .open(context.http(), 1)
            .await
            .expect("open");
        assert_eq!(response.status().as_u16(), 200);
        let mut stream = response.into_body();
        let mut body = Vec::new();
        while let Some(chunk) = stream.next_chunk().await {
            body.extend_from_slice(&chunk.expect("chunk"));
        }
        assert_eq!(body, b"hello");

        let port = raw(BUSY);
        let error = Call::get(format!("http://127.0.0.1:{port}/v1"))
            .open(context.http(), 1)
            .await
            .expect_err("status");
        assert_eq!(error.kind(), PluginErrorKind::Status);
        assert_eq!(error.status(), Some(429));
        assert_eq!(error.retry_after(), Some(Duration::from_secs(3)));
        assert_eq!(error.message(), "busy");
    }

    fn serve_two_on_keepalive(body: Vec<u8>) -> (u16, Arc<AtomicUsize>) {
        serve_keepalive(2, body)
    }

    fn serve_keepalive(requests_per_connection: usize, body: Vec<u8>) -> (u16, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let accepts = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&accepts);
        let body = Arc::new(body);
        std::thread::spawn(move || {
            loop {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                count.fetch_add(1, Ordering::SeqCst);
                let body = Arc::clone(&body);
                std::thread::spawn(move || {
                    for _ in 0..requests_per_connection {
                        let _ = read_request(&mut stream, true);
                        let header = format!(
                            "HTTP/1.1 400 Bad Request\r\ncontent-length: {}\r\nconnection: keep-alive\r\n\r\n",
                            body.len()
                        );
                        let _ = stream.write_all(header.as_bytes());
                        let _ = stream.write_all(&body);
                    }
                });
            }
        });
        (port, accepts)
    }

    #[tokio::test]
    async fn r4_71_region_fresh_closes_idle_before_reuse() {
        let _guard = exclusive().await;
        let limits = HttpLimits::default().with_max_idle_per_host(4);
        let http = PooledHttp::with_limits(HttpProfile::direct(), limits);
        let body = b"User location is not supported for the API use.".to_vec();
        let (port, accepts) = serve_two_on_keepalive(body);
        let url = format!("http://127.0.0.1:{port}/v1");
        let request = |path: &str| {
            HttpRequest::builder()
                .method("GET")
                .uri(format!("{url}{path}"))
                .body(Body::empty())
                .expect("request")
        };
        http.send(request("/first")).await.expect("first");
        let before = accepts.load(Ordering::SeqCst);
        assert!(before >= 1, "the first request must have been accepted");
        close_idle_after_region(&HttpProfile::direct(), limits);
        http.send(request("/third")).await.expect("third");
        wait_until(&accepts, before + 1).await;
    }

    #[tokio::test]
    async fn region_evict_drops_only_matching_profile() {
        let _guard = exclusive().await;
        let limits = HttpLimits::default();
        let proxy = HttpProfile::direct().with_proxy("http://127.0.0.1:9");
        let direct = HttpProfile::direct();
        close_idle_after_region(&proxy, limits);
        close_idle_after_region(&direct, limits);
        let proxy_http = PooledHttp::with_limits(proxy.clone(), limits);
        let direct_http = PooledHttp::with_limits(direct, limits);
        proxy_http.ensure().expect("proxy client");
        direct_http.ensure().expect("direct client");
        let count = pooled_client_count();
        close_idle_after_region(&proxy, limits);
        assert_eq!(pooled_client_count(), count);
        assert!(direct_http.pooled_client().is_some());
        let handle = proxy_http.pooled_client().expect("proxy handle");
        assert!(
            handle
                .downcast_ref::<Arc<std::sync::RwLock<reqwest::Client>>>()
                .is_some(),
            "the pooled handle must be the shared slot"
        );
    }

    #[test]
    fn recording_http_has_no_fresh_client() {
        use hofund_plugin_api::PluginId;

        let root = tempfile::tempdir().expect("temp dir");
        let host = crate::test_host(PluginId::try_from("echo").expect("id"), root.path());
        assert!(host.http().pooled_client().is_none());
        assert!(host.http().fresh_client().is_none());
    }

    #[tokio::test]
    async fn fresh_client_evicts_its_pool_key() {
        let _guard = exclusive().await;
        let limits = HttpLimits::default();
        let profile = HttpProfile::direct();
        close_idle_after_region(&profile, limits);
        let http = PooledHttp::with_limits(profile, limits);
        http.ensure().expect("seed pool");
        let count = pooled_client_count();
        http.fresh_client()
            .expect("pooled backend")
            .expect("fresh client");
        assert_eq!(pooled_client_count(), count);
    }

    fn shared_slot(http: &PooledHttp) -> Arc<RwLock<reqwest::Client>> {
        let handle = http.pooled_client().expect("pooled handle");
        Arc::clone(
            handle
                .downcast_ref::<Arc<RwLock<reqwest::Client>>>()
                .expect("shared slot type"),
        )
    }

    async fn get_via_slot(shared: &Arc<RwLock<reqwest::Client>>, url: &str) {
        let client = shared.read().expect("client lock").clone();
        let _ = client.get(url).send().await.expect("request");
    }

    #[tokio::test]
    async fn r4_72_fresh_client_refreshes_shared_slot_connection() {
        let _guard = exclusive().await;
        let limits = HttpLimits::default().with_max_idle_per_host(4);
        let profile = HttpProfile::direct();
        close_idle_after_region(&profile, limits);
        let http = PooledHttp::with_limits(profile, limits);
        let body = b"keep-alive body".to_vec();
        let (port, accepts) = serve_keepalive(8, body);
        let url = format!("http://127.0.0.1:{port}/v1");
        let shared = shared_slot(&http);
        let client = shared.read().expect("lock").clone();

        client.get(&url).send().await.expect("first");
        let before = accepts.load(Ordering::SeqCst);
        assert!(before >= 1, "the first request must have been accepted");

        let refreshes = refresh_calls();
        http.fresh_client()
            .expect("pooled backend")
            .expect("fresh client");
        assert_eq!(
            refresh_calls(),
            refreshes + 1,
            "fresh_client must refresh the shared slot"
        );

        let client = shared.read().expect("lock").clone();
        client.get(&url).send().await.expect("after fresh");
        wait_until(&accepts, before + 1).await;
    }

    // Keep-alive reuse of two pre-refresh requests (`accepts == 1`) flakes
    // under load on ubuntu (left: 2, right: 1, #704). These tests prove a
    // refresh opens a new connection: one request before, one after.
    #[tokio::test]
    async fn r4_72_close_idle_after_region_refreshes_pooled_slot() {
        let _guard = exclusive().await;
        let limits = HttpLimits::default().with_max_idle_per_host(4);
        let profile = HttpProfile::direct();
        close_idle_after_region(&profile, limits);
        let http = PooledHttp::with_limits(profile.clone(), limits);
        let body = b"region body".to_vec();
        let (port, accepts) = serve_keepalive(4, body);
        let url = format!("http://127.0.0.1:{port}/v1");
        let shared = shared_slot(&http);

        get_via_slot(&shared, &url).await;
        let before = accepts.load(Ordering::SeqCst);
        assert!(before >= 1, "the first request must have been accepted");

        let refreshes = refresh_calls();
        close_idle_after_region(&profile, limits);
        assert_eq!(
            refresh_calls(),
            refreshes + 1,
            "close_idle_after_region must refresh the pooled slot"
        );
        let client = shared.read().expect("lock").clone();
        client.get(&url).send().await.expect("after region close");
        wait_until(&accepts, before + 1).await;
    }

    #[tokio::test]
    async fn per_call_read_budget_shorter_than_the_pool_default_fires_first() {
        let _guard = exclusive().await;
        let pool_limits = HttpLimits::new(Duration::from_secs(2), Duration::from_secs(60));
        let (port, _, headers_written) = hold_after_headers();
        let context = http(pool_limits);
        let error = Call::get(format!("http://127.0.0.1:{port}/stall"))
            .read_timeout(Duration::from_millis(400))
            .send(context.http())
            .await
            .expect_err("short budget");
        assert_eq!(error.kind(), PluginErrorKind::Timeout);
        let started = headers_written
            .lock()
            .expect("headers time")
            .expect("headers written before the read budget fires");
        // Time the idle wait from the response headers, not bind/connect, so
        // macOS CI setup jitter is outside the window. 1500ms matches the
        // sibling stall test and stays far below the 60s pool read default
        // and the 2s connect timeout.
        assert!(started.elapsed() < Duration::from_millis(1500));
        assert!(started.elapsed() > Duration::from_millis(250));
    }

    #[tokio::test]
    async fn per_call_read_budget_longer_than_the_pool_default_is_honoured() {
        let _guard = exclusive().await;
        let pool_limits = HttpLimits::new(Duration::from_secs(2), Duration::from_millis(400));
        let (port, _) = drip(4, Duration::from_millis(550));
        let context = http(pool_limits);
        let reply = Call::get(format!("http://127.0.0.1:{port}/stream"))
            .read_timeout(Duration::from_secs(2))
            .send(context.http())
            .await
            .expect("longer budget");
        assert_eq!(reply.body(), b"abcd");
    }

    #[tokio::test]
    async fn per_call_read_budget_does_not_add_pool_entries() {
        let _guard = exclusive().await;
        let pool_limits = HttpLimits::default();
        let before = pooled_client_count();
        PooledHttp::with_limits(HttpProfile::direct(), pool_limits)
            .ensure()
            .expect("seed");
        let settled = pooled_client_count();
        assert!(settled >= before);
        let context = http(pool_limits);
        const OK: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok";
        let port = raw_accepts(OK, 2);
        let url = format!("http://127.0.0.1:{port}/a");
        let _ = Call::get(url.clone())
            .read_timeout(Duration::from_millis(500))
            .send(context.http())
            .await
            .expect("first");
        let _ = Call::get(url)
            .read_timeout(Duration::from_secs(90))
            .send(context.http())
            .await
            .expect("second");
        assert_eq!(pooled_client_count(), settled);
    }

    #[tokio::test]
    async fn r4_72_close_idle_after_region_refreshes_only_matching_profile() {
        let _guard = exclusive().await;
        let limits_a = HttpLimits::default().with_max_idle_per_host(4);
        let limits_b = HttpLimits::new(Duration::from_secs(11), Duration::from_secs(61))
            .with_max_idle_per_host(4);
        let direct = HttpProfile::direct();
        let proxy = HttpProfile::direct().with_proxy("http://127.0.0.1:9");
        close_idle_after_region(&direct, limits_a);
        close_idle_after_region(&proxy, limits_b);
        let http_a = PooledHttp::with_limits(direct, limits_a);
        let http_b = PooledHttp::with_limits(proxy, limits_b);
        let body = b"region body".to_vec();
        let (port, accepts) = serve_keepalive(4, body);
        let url = format!("http://127.0.0.1:{port}/v1");
        let shared_a = shared_slot(&http_a);
        let shared_b_before = shared_slot(&http_b);
        let pool_keys = pooled_client_count();

        get_via_slot(&shared_a, &url).await;
        let before = accepts.load(Ordering::SeqCst);
        assert!(before >= 1, "the first request must have been accepted");

        close_idle_after_region(&HttpProfile::direct(), limits_a);
        assert_eq!(pooled_client_count(), pool_keys);
        let shared_b_after = shared_slot(&http_b);
        assert!(
            Arc::ptr_eq(&shared_b_before, &shared_b_after),
            "refresh must not replace the other profile pool slot"
        );
        get_via_slot(&shared_a, &url).await;
        wait_until(&accepts, before + 1).await;
    }
}
