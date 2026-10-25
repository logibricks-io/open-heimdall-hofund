//! Process-wide HTTP clients. One client per proxy and TLS profile.
//!
//! The pool is the only `reqwest` client in the plugin stack. Plugins call
//! [`crate::host_context`]'s [`HttpHandle`](hofund_plugin_api::HttpHandle), and
//! the SDK builds the request. This module does not add a profile argument to
//! [`HttpBackend`](hofund_plugin_api::kernel::HttpBackend): the profile is
//! chosen when the handle is built.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

use async_trait::async_trait;
use hofund_plugin_api::kernel::{HttpBackend, HttpRequest, HttpResponse};
use hofund_plugin_api::{Body, BodyStream, PluginError};

const USER_AGENT: &str = "hofund/0.1.0";
const MAX_IDLE_PER_HOST: usize = 8;

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

/// [`HttpBackend`] over the process-wide pool.
#[derive(Clone, Debug)]
pub struct PooledHttp {
    profile: HttpProfile,
}

impl PooledHttp {
    /// The client for a direct connection.
    #[must_use]
    pub fn direct() -> Self {
        Self::new(HttpProfile::direct())
    }

    /// The client for `profile`.
    #[must_use]
    pub fn new(profile: HttpProfile) -> Self {
        Self { profile }
    }

    /// Builds the pooled client without sending.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the proxy URL or the client cannot be built.
    #[cfg(test)]
    pub(crate) fn ensure(&self) -> Result<(), PluginError> {
        client_for(&self.profile).map(|_| ())
    }
}

static POOL: OnceLock<Mutex<BTreeMap<HttpProfile, reqwest::Client>>> = OnceLock::new();

fn pool() -> &'static Mutex<BTreeMap<HttpProfile, reqwest::Client>> {
    POOL.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Clients currently in the pool. Tests use this to show one profile is one client.
#[cfg(test)]
pub(crate) fn pooled_client_count() -> usize {
    pool().lock().map(|guard| guard.len()).unwrap_or(0)
}

fn client_for(profile: &HttpProfile) -> Result<reqwest::Client, PluginError> {
    let mut guard = pool()
        .lock()
        .map_err(|_| PluginError::internal("http pool lock is poisoned"))?;
    if let Some(client) = guard.get(profile) {
        return Ok(client.clone());
    }
    let client = build_client(profile)?;
    guard.insert(profile.clone(), client.clone());
    Ok(client)
}

fn build_client(profile: &HttpProfile) -> Result<reqwest::Client, PluginError> {
    let mut builder = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .pool_max_idle_per_host(MAX_IDLE_PER_HOST)
        .danger_accept_invalid_certs(profile.insecure);
    if let Some(proxy) = &profile.proxy {
        let proxy = reqwest::Proxy::all(proxy)
            .map_err(|error| PluginError::config(format!("http proxy is invalid: {error}")))?;
        builder = builder.proxy(proxy);
    }
    builder
        .build()
        .map_err(|error| PluginError::connect(error.to_string()))
}

fn map_error(error: reqwest::Error) -> PluginError {
    if error.is_timeout() {
        PluginError::timeout(error.to_string())
    } else {
        PluginError::connect(error.to_string())
    }
}

#[async_trait]
impl HttpBackend for PooledHttp {
    async fn send(
        &self,
        request: HttpRequest<Body>,
    ) -> Result<HttpResponse<BodyStream>, PluginError> {
        let client = client_for(&self.profile)?;
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
        let collected = response.bytes().await.map_err(map_error)?;
        let mut output = HttpResponse::new(BodyStream::from_bytes(collected));
        *output.status_mut() = status;
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    use hofund_plugin_api::kernel::{HttpBackend, HttpRequest};
    use hofund_plugin_api::{Body, PluginErrorKind};

    use super::{HttpProfile, PooledHttp, pooled_client_count};

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
        assert_eq!(pooled_client_count(), direct + 2);
    }

    #[test]
    fn a_bad_proxy_url_is_config() {
        let error = PooledHttp::new(HttpProfile::direct().with_proxy("not a proxy"))
            .ensure()
            .expect_err("proxy");
        assert_eq!(error.kind(), PluginErrorKind::Config);
    }
}
