//! Loopback-only HTTP control listener.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::Response;
use hofund_control_proto::{
    ControlError, ControlErrorKind, ErrorEnvelope, HealthResponse, MAX_REQUEST_BODY_BYTES,
    StatusResponse,
};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use super::{GatewayError, Running, TokenState, tokens_match};

/// Default loopback address for the control listener.
pub const DEFAULT_CONTROL_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 3426);

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
    listen: SocketAddr,
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

fn check_host_and_origin(headers: &HeaderMap, listen: SocketAddr) -> Option<Response> {
    let expected = listen.to_string();
    if !exact_header(headers, header::HOST, &expected) {
        return Some(error_response(
            StatusCode::FORBIDDEN,
            ControlErrorKind::Forbidden,
            "Host must be the numeric loopback control address",
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
