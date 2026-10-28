//! Loopback-only HTTP control listener.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

#[cfg(unix)]
use std::path::Path;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::Response;
use hofund_control_proto::{
    ControlError, ControlErrorKind, ErrorEnvelope, HealthResponse, MAX_REQUEST_BODY_BYTES,
    StatusResponse,
};
#[cfg(unix)]
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::oneshot;
#[cfg(unix)]
use tower::Service;

use super::{GatewayError, Running, TokenState, tokens_match};

/// Default loopback address for the control listener.
pub const DEFAULT_CONTROL_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 3426);

/// The only Host value accepted over local IPC.
pub const LOCAL_IPC_HOST: &str = "hofund-control";

/// Daemon operations exposed through the control protocol.
pub trait ControlState: Send + Sync {
    /// Returns control health.
    fn health(&self) -> HealthResponse;

    /// Returns the current daemon status.
    fn status(&self) -> StatusResponse;
}

/// A control server with its own router and token-file state.
#[derive(Clone)]
pub struct Control {
    inner: Arc<Inner>,
}

struct Inner {
    state: Arc<dyn ControlState>,
    tokens: Arc<TokenState>,
}

#[derive(Clone)]
struct HttpState {
    control: Arc<Inner>,
    listen: Option<SocketAddr>,
}

impl Control {
    /// Creates a control server using a fixed initial token.
    #[must_use]
    pub fn new(state: Arc<dyn ControlState>, token: String) -> Self {
        Self {
            inner: Arc::new(Inner {
                state,
                tokens: Arc::new(TokenState::new(token, None)),
            }),
        }
    }

    /// Creates a control server that re-reads `token_file` on every request.
    #[must_use]
    pub fn with_token_file(
        state: Arc<dyn ControlState>,
        token: String,
        token_file: PathBuf,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                state,
                tokens: Arc::new(TokenState::new(token, Some(token_file))),
            }),
        }
    }

    /// Builds only the control router. `listen` supplies the exact Host value.
    pub fn into_router(self, listen: SocketAddr) -> axum::Router {
        self.router(Some(listen))
    }

    #[cfg(unix)]
    fn into_local_router(self) -> axum::Router {
        self.router(None)
    }

    fn router(self, listen: Option<SocketAddr>) -> axum::Router {
        axum::Router::new()
            .fallback(axum::routing::any(handle))
            .with_state(HttpState {
                control: Arc::clone(&self.inner),
                listen,
            })
    }

    /// Binds a numeric loopback address and serves only control routes.
    ///
    /// # Errors
    ///
    /// Returns [`GatewayError`] for a non-loopback address or a bind failure.
    pub async fn bind(self, listen: SocketAddr) -> Result<Running, GatewayError> {
        if !listen.ip().is_loopback() {
            return Err(GatewayError::NonLoopback { listen });
        }
        let listener = TcpListener::bind(listen)
            .await
            .map_err(|source| GatewayError::Bind { listen, source })?;
        let local_addr = listener
            .local_addr()
            .map_err(|source| GatewayError::Bind { listen, source })?;
        let router = self.into_router(local_addr);
        let (shutdown, signal) = oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let _ = signal.await;
                })
                .await
        });
        Ok(Running::new(local_addr, shutdown, task))
    }

    /// Binds a Unix socket and serves control routes to same-user peers.
    #[cfg(unix)]
    ///
    /// # Errors
    ///
    /// Returns [`GatewayError`] when the path is too long, binding fails, or
    /// socket ownership cannot be established.
    pub async fn bind_unix(self, path: &Path) -> Result<Running, GatewayError> {
        use std::os::unix::fs::MetadataExt;
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let expected_uid = std::fs::metadata(parent)
            .map_err(|source| GatewayError::BindUnix {
                path: path.to_path_buf(),
                source,
            })?
            .uid();
        self.bind_unix_with_expected_uid(path, expected_uid).await
    }

    /// Binds a Unix socket with an explicit expected peer uid for testing.
    #[cfg(unix)]
    #[doc(hidden)]
    pub async fn bind_unix_with_expected_uid(
        self,
        path: &Path,
        expected_uid: u32,
    ) -> Result<Running, GatewayError> {
        use std::os::unix::fs::PermissionsExt;
        validate_socket_path(path)?;
        let listener = UnixListener::bind(path).map_err(|source| GatewayError::BindUnix {
            path: path.to_path_buf(),
            source,
        })?;
        let cleanup = SocketCleanup(path.to_path_buf());
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(
            |source| GatewayError::BindUnix {
                path: path.to_path_buf(),
                source,
            },
        )?;
        let router = self.into_local_router();
        let (shutdown, mut signal) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    _ = &mut signal => break,
                    accepted = listener.accept() => {
                        let (stream, _) = match accepted {
                            Ok(accepted) => accepted,
                            Err(error) => {
                                tracing::warn!(%error, "local control accept failed");
                                if let Some(backoff) = accept_backoff(&error) {
                                    tokio::time::sleep(backoff).await;
                                }
                                continue;
                            }
                        };
                        if !same_user_peer(&stream, expected_uid).unwrap_or(false) {
                            continue;
                        }
                        let router = router.clone();
                        connections.spawn(async move {
                            let service = hyper::service::service_fn(move |request| {
                                let mut router = router.clone();
                                async move { router.call(request.map(Body::new)).await }
                            });
                            let result = hyper::server::conn::http1::Builder::new()
                                .serve_connection(TokioIo::new(stream), service)
                                .await;
                            if let Err(error) = result {
                                tracing::debug!(%error, "local control connection ended");
                            }
                        });
                    }
                    Some(_) = connections.join_next(), if !connections.is_empty() => {}
                }
            }
            while connections.join_next().await.is_some() {}
            Ok(())
        });
        Ok(Running::new(DEFAULT_CONTROL_ADDR, shutdown, task).with_resource(cleanup))
    }
}

#[cfg(unix)]
struct SocketCleanup(PathBuf);

#[cfg(unix)]
impl Drop for SocketCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

// Keep these Unix errno values local because gateway does not depend on libc.
#[cfg(unix)]
const ENFILE: i32 = 23;
#[cfg(unix)]
const EMFILE: i32 = 24;

#[cfg(unix)]
fn accept_backoff(error: &std::io::Error) -> Option<std::time::Duration> {
    matches!(error.raw_os_error(), Some(ENFILE | EMFILE))
        .then_some(std::time::Duration::from_millis(100))
}

#[cfg(unix)]
fn same_user_peer(stream: &UnixStream, expected_uid: u32) -> std::io::Result<bool> {
    Ok(peer_allowed(stream.peer_cred()?.uid(), expected_uid))
}

#[cfg(unix)]
fn peer_allowed(peer_uid: u32, expected_uid: u32) -> bool {
    peer_uid == expected_uid
}

#[cfg(unix)]
fn validate_socket_path(path: &Path) -> Result<(), GatewayError> {
    use std::os::unix::ffi::OsStrExt;
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
    const LIMIT: usize = 104;
    #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "freebsd")))]
    const LIMIT: usize = 108;
    if path.as_os_str().as_bytes().len() >= LIMIT {
        return Err(GatewayError::SocketPathTooLong {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

impl fmt::Debug for Control {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Control").finish_non_exhaustive()
    }
}

async fn handle(
    axum::extract::State(state): axum::extract::State<HttpState>,
    request: Request,
) -> Response {
    if let Some(response) = check_host_and_origin(request.headers(), state.listen) {
        return response;
    }
    let method = request.method();
    let path = request.uri().path();
    let open_health = method == Method::GET && path == "/v1/hofund/health";
    if !open_health
        && !authorized(
            &state.control.tokens.current(),
            request.headers(),
            request.uri(),
        )
    {
        return error_response(
            StatusCode::UNAUTHORIZED,
            ControlErrorKind::Unauthorized,
            "unauthorized",
        );
    }
    if method != Method::GET && method != Method::HEAD {
        if !json_content_type(request.headers()) {
            return error_response(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                ControlErrorKind::BadRequest,
                "mutations require Content-Type: application/json",
            );
        }
        if axum::body::to_bytes(request.into_body(), MAX_REQUEST_BODY_BYTES)
            .await
            .is_err()
        {
            return error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                ControlErrorKind::BadRequest,
                "request body is too large",
            );
        }
        return error_response(
            StatusCode::METHOD_NOT_ALLOWED,
            ControlErrorKind::MethodNotAllowed,
            "method not allowed",
        );
    }

    match (method, path) {
        (&Method::GET, "/v1/hofund/health") => json_response(
            StatusCode::OK,
            serde_json::to_vec(&state.control.state.health()).unwrap_or_default(),
        ),
        (&Method::GET, "/v1/hofund/status") => json_response(
            StatusCode::OK,
            serde_json::to_vec(&state.control.state.status()).unwrap_or_default(),
        ),
        (&Method::GET, _) if path.starts_with("/v1/hofund/") || path == "/v1/hofund" => {
            error_response(
                StatusCode::NOT_FOUND,
                ControlErrorKind::NotFound,
                "not found",
            )
        }
        _ if path == "/v1/hofund/health" || path == "/v1/hofund/status" => error_response(
            StatusCode::METHOD_NOT_ALLOWED,
            ControlErrorKind::MethodNotAllowed,
            "method not allowed",
        ),
        _ => error_response(
            StatusCode::NOT_FOUND,
            ControlErrorKind::NotFound,
            "not found",
        ),
    }
}

fn check_host_and_origin(headers: &HeaderMap, listen: Option<SocketAddr>) -> Option<Response> {
    let expected = listen.map_or_else(|| LOCAL_IPC_HOST.to_owned(), |addr| addr.to_string());
    if !exact_header(headers, header::HOST, &expected) {
        let message = if listen.is_some() {
            "Host must be the numeric loopback control address"
        } else {
            "Host must be hofund-control over local IPC"
        };
        return Some(error_response(
            StatusCode::FORBIDDEN,
            ControlErrorKind::Forbidden,
            message,
        ));
    }
    if headers.contains_key(header::ORIGIN)
        && !exact_header(headers, header::ORIGIN, &format!("http://{expected}"))
    {
        return Some(error_response(
            StatusCode::FORBIDDEN,
            ControlErrorKind::Forbidden,
            "Origin does not match the control listener",
        ));
    }
    None
}

fn exact_header(headers: &HeaderMap, name: header::HeaderName, expected: &str) -> bool {
    let mut values = headers.get_all(name).iter();
    let Some(value) = values.next() else {
        return false;
    };
    values.next().is_none() && value.to_str().is_ok_and(|value| value == expected)
}

fn authorized(expected: &str, headers: &HeaderMap, uri: &Uri) -> bool {
    if headers.contains_key("x-api-key")
        || headers.contains_key("x-goog-api-key")
        || uri.query().is_some_and(has_query_token)
    {
        return false;
    }
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let Some(value) = values.next() else {
        return false;
    };
    if values.next().is_some() {
        return false;
    }
    let Ok(value) = value.to_str() else {
        return false;
    };
    let Some(token) = value.strip_prefix("Bearer ") else {
        return false;
    };
    !token.is_empty()
        && !token.bytes().any(|byte| byte.is_ascii_whitespace())
        && tokens_match(expected, token)
}

fn has_query_token(query: &str) -> bool {
    query.split('&').any(|pair| {
        let name = pair.split('=').next().unwrap_or_default();
        name == "key" || name == "token"
    })
}

fn json_content_type(headers: &HeaderMap) -> bool {
    let mut values = headers.get_all(header::CONTENT_TYPE).iter();
    let Some(value) = values.next() else {
        return false;
    };
    if values.next().is_some() {
        return false;
    }
    value.to_str().is_ok_and(|value| {
        value
            .split(';')
            .next()
            .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("application/json"))
    })
}

fn error_response(status: StatusCode, kind: ControlErrorKind, message: &str) -> Response {
    let mut envelope = ErrorEnvelope::default();
    envelope.error = ControlError::new(kind, message, "http");
    json_response(status, serde_json::to_vec(&envelope).unwrap_or_default())
}

fn json_response(status: StatusCode, body: Vec<u8>) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

#[cfg(all(test, unix))]
mod tests {
    #[test]
    fn unix_peer_rejects_a_different_uid() {
        assert!(super::peer_allowed(1000, 1000));
        assert!(!super::peer_allowed(1001, 1000));
    }

    #[test]
    fn accept_backoff_only_applies_to_descriptor_exhaustion() {
        for errno in [super::ENFILE, super::EMFILE] {
            let error = std::io::Error::from_raw_os_error(errno);
            assert_eq!(
                super::accept_backoff(&error),
                Some(std::time::Duration::from_millis(100))
            );
        }
        let error = std::io::Error::new(std::io::ErrorKind::ConnectionAborted, "aborted");
        assert_eq!(super::accept_backoff(&error), None);
    }
}
