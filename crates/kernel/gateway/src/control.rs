//! Loopback-only HTTP control listener.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[cfg(unix)]
use std::path::Path;

use axum::body::{Body, Bytes};
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{DefaultBodyLimit, Path as AxumPath, Query, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures_util::future::BoxFuture;
use futures_util::stream::{BoxStream, StreamExt};
use hofund_control_proto::{
    AgentChangeResponse, AgentConnectRequest, AgentDisconnectRequest, AgentListResponse,
    AgentModelRequest, AgentPlanRequest, AgentPlanResponse, AgentStatusResponse,
    ConfigApplyRequest, ConfigResponse, ConfigValidateRequest, ConfigValidationResponse,
    ControlError, ControlErrorKind, ErrorEnvelope, GatewayKeyRequest, GatewayKeyResponse,
    GatewayKeyRestoreRequest, HealthResponse, MAX_ID_BYTES, MAX_REQUEST_BODY_BYTES,
    ModelListResponse, PicksResponse, PicksSetRequest, ProviderKeyRequest, ProviderKeysResponse,
    ProviderListResponse, ProviderTestRequest, ProviderTestResponse, QuotaHistoryQuery,
    QuotaHistoryResponse, QuotaQuery, QuotaResponse, ReloadRequest, RouteListResponse,
    RouteModelListResponse, RouteQuery, SessionRouteResponse, ShutdownRequest, ShutdownResponse,
    StatusResponse, TokenRotateRequest, TokenRotateResponse, Topic, UnroutedListResponse,
    UsageQuery, UsageResponse, WarmupListResponse, WarmupRunRequest, WarmupRunResponse,
    decode_request,
};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::oneshot;
use tower::Service;

use super::{GatewayError, Running, TokenState, tokens_match};

/// Default loopback address for the control listener.
pub const DEFAULT_CONTROL_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 3426);

/// The only Host value accepted over local IPC.
pub const LOCAL_IPC_HOST: &str = "hofund-control";

/// Default interval between SSE heartbeat comments.
pub const SSE_HEARTBEAT: Duration = Duration::from_secs(15);

/// Longest `Last-Event-ID` header value passed to the daemon; longer values are
/// treated as malformed ids.
pub const MAX_LAST_EVENT_ID_BYTES: usize = 1024;

/// A parsed `GET /v1/hofund/events` request.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct EventsQuery {
    /// Requested topics in canonical order, without duplicates.
    pub topics: Vec<Topic>,
    /// Raw `Last-Event-ID` header value. The daemon parses it so a malformed
    /// id resynchronizes instead of failing the reconnect.
    pub last_event_id: Option<String>,
}

impl EventsQuery {
    /// Constructs a query; topics are put in canonical order without duplicates.
    #[must_use]
    pub fn new(topics: &[Topic], last_event_id: Option<String>) -> Self {
        Self {
            topics: Topic::CANONICAL
                .into_iter()
                .filter(|topic| topics.contains(topic))
                .collect(),
            last_event_id,
        }
    }
}

/// One server-sent event: composite id, event name, and serialized envelope.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SseFrame {
    /// Composite event id carrying every subscribed topic's cursor.
    pub id: String,
    /// Event name, the envelope payload tag.
    pub event: String,
    /// One serialized `EventEnvelope`.
    pub data: Arc<str>,
}

impl SseFrame {
    /// Constructs a frame.
    #[must_use]
    pub fn new(id: String, event: impl Into<String>, data: Arc<str>) -> Self {
        Self {
            id,
            event: event.into(),
            data,
        }
    }
}

/// An open event subscription. The stream ends after the daemon sends a
/// closing event such as `token_rotated` or `shutdown_started`.
pub struct EventSubscription {
    /// Frames in delivery order.
    pub frames: BoxStream<'static, SseFrame>,
}

impl EventSubscription {
    /// Wraps a frame stream.
    #[must_use]
    pub fn new(frames: BoxStream<'static, SseFrame>) -> Self {
        Self { frames }
    }
}

impl fmt::Debug for EventSubscription {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EventSubscription")
            .finish_non_exhaustive()
    }
}

/// A boxed daemon operation exposed through the control protocol.
pub type ControlFuture<'a, T> = BoxFuture<'a, Result<T, ControlFailure>>;

/// A control error and its optional upstream HTTP status.
pub struct ControlFailure {
    /// Transport-independent failure details.
    pub error: ControlError,
    /// Upstream status used only for an upstream rate limit.
    pub status: Option<u16>,
}

impl From<ControlError> for ControlFailure {
    fn from(error: ControlError) -> Self {
        Self {
            error,
            status: None,
        }
    }
}

/// Daemon operations exposed through the control protocol.
pub trait ControlState: Send + Sync {
    /// Returns control health.
    fn health(&self) -> HealthResponse;
    /// Returns the current daemon status.
    fn status(&self) -> StatusResponse;
    /// Reports whether mutations are draining.
    fn draining(&self) -> bool;
    /// Lists configured providers.
    fn providers(&self) -> ControlFuture<'_, ProviderListResponse>;
    /// Lists a provider's upstream API keys without their secrets.
    fn provider_keys(&self, _id: String) -> ControlFuture<'_, ProviderKeysResponse> {
        Box::pin(async {
            Err(ControlError::new(
                ControlErrorKind::BadRequest,
                "provider keys are not supported by this control state",
                "control",
            )
            .into())
        })
    }
    /// Changes a provider's upstream API keys.
    fn change_provider_key(
        &self,
        _id: String,
        _request: ProviderKeyRequest,
    ) -> ControlFuture<'_, ProviderKeysResponse> {
        Box::pin(async {
            Err(ControlError::new(
                ControlErrorKind::BadRequest,
                "provider keys are not supported by this control state",
                "control",
            )
            .into())
        })
    }
    /// Reads a provider's cached models.
    fn provider_models(&self, id: String) -> ControlFuture<'_, ModelListResponse>;
    /// Refreshes a provider's models.
    fn refresh_models(&self, id: String) -> ControlFuture<'_, ModelListResponse>;
    /// Tests connectivity to a provider.
    fn test_provider(
        &self,
        id: String,
        request: ProviderTestRequest,
    ) -> ControlFuture<'_, ProviderTestResponse>;
    /// Reads a provider's model picks.
    fn picks(&self, id: String) -> ControlFuture<'_, PicksResponse>;
    /// Replaces or edits a provider's model picks.
    fn set_picks(&self, id: String, request: PicksSetRequest) -> ControlFuture<'_, PicksResponse>;
    /// Lists configured client-facing route models.
    fn route_models(&self) -> ControlFuture<'_, RouteModelListResponse>;
    /// Lists recent routing decisions.
    fn route_decisions(&self, query: RouteQuery) -> ControlFuture<'_, RouteListResponse> {
        let _ = query;
        Box::pin(async {
            Err(ControlError::new(
                ControlErrorKind::BadRequest,
                "routing decisions are not supported by this control state",
                "control",
            )
            .into())
        })
    }
    /// Returns the latest routing decision for one session.
    fn route_session(
        &self,
        _session: String,
        query: RouteQuery,
    ) -> ControlFuture<'_, SessionRouteResponse> {
        let _ = query;
        Box::pin(async {
            Err(ControlError::new(
                ControlErrorKind::BadRequest,
                "routing decisions are not supported by this control state",
                "control",
            )
            .into())
        })
    }
    /// Lists requests waiting before routing starts.
    fn route_unrouted(&self) -> ControlFuture<'_, UnroutedListResponse> {
        Box::pin(async {
            Err(ControlError::new(
                ControlErrorKind::BadRequest,
                "routing decisions are not supported by this control state",
                "control",
            )
            .into())
        })
    }
    /// Lists known agents.
    fn agents(&self) -> ControlFuture<'_, AgentListResponse>;
    /// Reports agent connection states.
    fn agent_status(&self) -> ControlFuture<'_, AgentStatusResponse>;
    /// Plans an agent lifecycle change.
    fn plan_agent(
        &self,
        id: String,
        request: AgentPlanRequest,
    ) -> ControlFuture<'_, AgentPlanResponse>;
    /// Connects an agent.
    fn connect_agent(
        &self,
        id: String,
        request: AgentConnectRequest,
    ) -> ControlFuture<'_, AgentChangeResponse>;
    /// Disconnects an agent.
    fn disconnect_agent(
        &self,
        id: String,
        request: AgentDisconnectRequest,
    ) -> ControlFuture<'_, AgentChangeResponse>;
    /// Sets or clears the model an agent uses, connecting it when needed.
    fn set_agent_model(
        &self,
        id: String,
        request: AgentModelRequest,
    ) -> ControlFuture<'_, AgentChangeResponse>;
    /// Reports usage for a period.
    fn usage(&self, query: UsageQuery) -> ControlFuture<'_, UsageResponse>;
    /// Reports what is left of every subscription, with each account's
    /// standing. A kept query never asks a vendor.
    fn quotas(&self, query: QuotaQuery) -> ControlFuture<'_, QuotaResponse>;
    /// Reports sampled quota history.
    fn quota_history(&self, query: QuotaHistoryQuery) -> ControlFuture<'_, QuotaHistoryResponse>;
    /// Lists last daily warm-up results per subscription account.
    fn warmups(&self) -> ControlFuture<'_, WarmupListResponse> {
        Box::pin(async {
            Err(ControlError::new(
                ControlErrorKind::BadRequest,
                "daily warm-up status is not supported by this control state",
                "control",
            )
            .into())
        })
    }
    /// Runs daily warm-up for one subscription account now.
    fn run_warmup(&self, _request: WarmupRunRequest) -> ControlFuture<'_, WarmupRunResponse> {
        Box::pin(async {
            Err(ControlError::new(
                ControlErrorKind::BadRequest,
                "daily warm-up is not supported by this control state",
                "control",
            )
            .into())
        })
    }
    /// Lists named gateway keys.
    fn gateway_keys(&self) -> ControlFuture<'_, GatewayKeyResponse> {
        Box::pin(async { Err(unsupported_gateway_keys()) })
    }
    /// Changes a named gateway key.
    fn change_gateway_key(
        &self,
        _request: GatewayKeyRequest,
    ) -> ControlFuture<'_, GatewayKeyResponse> {
        Box::pin(async { Err(unsupported_gateway_keys()) })
    }
    /// Exports complete named-key records for backup.
    fn export_gateway_keys(&self) -> ControlFuture<'_, GatewayKeyResponse> {
        Box::pin(async { Err(unsupported_gateway_keys()) })
    }
    /// Restores named-key records.
    fn restore_gateway_keys(
        &self,
        _request: GatewayKeyRestoreRequest,
    ) -> ControlFuture<'_, GatewayKeyResponse> {
        Box::pin(async { Err(unsupported_gateway_keys()) })
    }
    /// Rotates the control and model bearer token.
    fn rotate_token(&self, request: TokenRotateRequest) -> ControlFuture<'_, TokenRotateResponse>;
    /// Starts graceful daemon shutdown.
    fn shutdown(&self, request: ShutdownRequest) -> ControlFuture<'_, ShutdownResponse>;
    /// Reads the configuration document and its revisions.
    fn config(&self) -> ControlFuture<'_, ConfigResponse>;
    /// Validates a candidate configuration document without writing it.
    fn validate_config(
        &self,
        request: ConfigValidateRequest,
    ) -> ControlFuture<'_, ConfigValidationResponse>;
    /// Writes and publishes a configuration document.
    fn apply_config(&self, request: ConfigApplyRequest) -> ControlFuture<'_, ConfigResponse>;
    /// Reloads the configuration file from disk.
    fn reload_config(&self, request: ReloadRequest) -> ControlFuture<'_, ConfigResponse>;
    /// Opens an event subscription.
    ///
    /// # Errors
    ///
    /// Returns [`ControlFailure`] when the daemon cannot serve events.
    fn events(&self, query: EventsQuery) -> Result<EventSubscription, ControlFailure>;
}

fn unsupported_gateway_keys() -> ControlFailure {
    ControlError::new(
        ControlErrorKind::Internal,
        "gateway-key management is not implemented by this control state",
        "control",
    )
    .into()
}

/// A control server with its own router and token-file state.
#[derive(Clone)]
pub struct Control {
    inner: Arc<Inner>,
    heartbeat: Duration,
}

struct Inner {
    state: Arc<dyn ControlState>,
    tokens: Arc<TokenState>,
}

#[derive(Clone)]
struct HttpState {
    control: Arc<Inner>,
    listen: Option<SocketAddr>,
    heartbeat: Duration,
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
            heartbeat: SSE_HEARTBEAT,
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
            heartbeat: SSE_HEARTBEAT,
        }
    }

    /// Sets the SSE heartbeat interval; tests use a short one.
    #[doc(hidden)]
    #[must_use]
    pub fn with_heartbeat(mut self, heartbeat: Duration) -> Self {
        self.heartbeat = heartbeat;
        self
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
        let state = HttpState {
            control: Arc::clone(&self.inner),
            listen,
            heartbeat: self.heartbeat,
        };
        axum::Router::new()
            .route("/v1/hofund/health", get(health).head(method_not_allowed))
            .route("/v1/hofund/status", get(status).head(method_not_allowed))
            .route(
                "/v1/hofund/providers",
                get(providers).head(method_not_allowed),
            )
            .route(
                "/v1/hofund/providers/{id}/keys",
                get(provider_keys)
                    .post(change_provider_key)
                    .head(method_not_allowed),
            )
            .route(
                "/v1/hofund/providers/{id}/models",
                get(provider_models).head(method_not_allowed),
            )
            .route(
                "/v1/hofund/providers/{id}/models/refresh",
                post(refresh_models),
            )
            .route("/v1/hofund/providers/{id}/test", post(test_provider))
            .route(
                "/v1/hofund/providers/{id}/picks",
                get(picks).put(set_picks).head(method_not_allowed),
            )
            .route(
                "/v1/hofund/models",
                get(route_models).head(method_not_allowed),
            )
            .route(
                "/v1/hofund/route",
                get(route_decisions).head(method_not_allowed),
            )
            .route(
                "/v1/hofund/route/session",
                get(route_session).head(method_not_allowed),
            )
            .route(
                "/v1/hofund/route/unrouted",
                get(route_unrouted).head(method_not_allowed),
            )
            .route("/v1/hofund/agents", get(agents).head(method_not_allowed))
            .route(
                "/v1/hofund/agents/status",
                get(agent_status).head(method_not_allowed),
            )
            .route("/v1/hofund/agents/{id}/plan", post(plan_agent))
            .route("/v1/hofund/agents/{id}/connect", post(connect_agent))
            .route("/v1/hofund/agents/{id}/disconnect", post(disconnect_agent))
            .route("/v1/hofund/agents/{id}/model", post(set_agent_model))
            .route("/v1/hofund/usage", get(usage).head(method_not_allowed))
            .route(
                "/v1/hofund/gateway-keys",
                get(gateway_keys)
                    .post(change_gateway_key)
                    .head(method_not_allowed),
            )
            .route(
                "/v1/hofund/gateway-keys/export",
                get(export_gateway_keys).head(method_not_allowed),
            )
            .route(
                "/v1/hofund/gateway-keys/restore",
                post(restore_gateway_keys),
            )
            .route("/v1/hofund/quotas", get(quotas).head(method_not_allowed))
            .route(
                "/v1/hofund/quotas/history",
                get(quota_history).head(method_not_allowed),
            )
            .route("/v1/hofund/warmups", get(warmups).head(method_not_allowed))
            .route("/v1/hofund/warmups/run", post(run_warmup))
            .route("/v1/hofund/token/rotate", post(rotate_token))
            .route("/v1/hofund/shutdown", post(shutdown))
            .route("/v1/hofund/config", get(config).head(method_not_allowed))
            .route("/v1/hofund/config/validate", post(validate_config))
            .route("/v1/hofund/config/apply", post(apply_config))
            .route("/v1/hofund/config/reload", post(reload_config))
            .route("/v1/hofund/events", get(events).head(method_not_allowed))
            .fallback(not_found)
            .method_not_allowed_fallback(method_not_allowed)
            .with_state(state.clone())
            // `pre_route` buffers the body up to `MAX_REQUEST_BODY_BYTES` and is
            // the only size gate. Disable the extractor default (2 MiB) so a
            // body within the protocol limit is never rejected outside the
            // control error envelope.
            .layer(DefaultBodyLimit::disable())
            .layer(middleware::from_fn_with_state(state, pre_route))
    }

    /// Serves the control routes on one trusted connection until the peer
    /// closes it.
    ///
    /// The peer is the process that started this one and owns both ends of
    /// `io`, as with a one-shot `hofundd` on its standard input and output.
    /// Each request is therefore sent with the current token in place of any
    /// `Authorization` header it carried; the `hofund-control` host, origin,
    /// content-type, and size checks still apply.
    ///
    /// # Errors
    ///
    /// Returns the connection error when HTTP framing fails.
    pub async fn serve_trusted<IO>(self, io: IO) -> std::io::Result<()>
    where
        IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let tokens = Arc::clone(&self.inner.tokens);
        let router = self.router(None);
        let service = hyper::service::service_fn(
            move |request: axum::http::Request<hyper::body::Incoming>| {
                let mut router = router.clone();
                let bearer = header::HeaderValue::from_str(&format!("Bearer {}", tokens.current()));
                async move {
                    let mut request = request.map(Body::new);
                    match bearer {
                        Ok(bearer) => {
                            request.headers_mut().insert(header::AUTHORIZATION, bearer);
                        }
                        Err(_) => {
                            request.headers_mut().remove(header::AUTHORIZATION);
                        }
                    }
                    router.call(request).await
                }
            },
        );
        hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(io), service)
            .await
            .map_err(std::io::Error::other)
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
        Ok(Running::new(local_addr, shutdown, task).with_control_drain())
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
            // Tells open connections to finish their current request and
            // close, so an idle keep-alive client cannot hold shutdown open.
            let (stop, stopping) = tokio::sync::watch::channel(false);
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
                        let mut stopping = stopping.clone();
                        connections.spawn(async move {
                            let service = hyper::service::service_fn(move |request| {
                                let mut router = router.clone();
                                async move { router.call(request.map(Body::new)).await }
                            });
                            let connection = hyper::server::conn::http1::Builder::new()
                                .serve_connection(TokioIo::new(stream), service);
                            tokio::pin!(connection);
                            let result = tokio::select! {
                                result = connection.as_mut() => result,
                                () = async {
                                    // The borrowed value is not `Send`; drop it here.
                                    let _ = stopping.wait_for(|stop| *stop).await.is_ok();
                                } => {
                                    connection.as_mut().graceful_shutdown();
                                    connection.await
                                }
                            };
                            if let Err(error) = result {
                                tracing::debug!(%error, "local control connection ended");
                            }
                        });
                    }
                    Some(_) = connections.join_next(), if !connections.is_empty() => {}
                }
            }
            stop.send_replace(true);
            while connections.join_next().await.is_some() {}
            Ok(())
        });
        Ok(Running::new(DEFAULT_CONTROL_ADDR, shutdown, task)
            .with_control_drain()
            .with_resource(cleanup))
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

macro_rules! operation_response {
    ($result:expr) => {
        match $result {
            Ok(value) => json_response(
                StatusCode::OK,
                serde_json::to_vec(&value).unwrap_or_default(),
            ),
            Err(failure) => failure_response(failure),
        }
    };
}

async fn pre_route(State(state): State<HttpState>, mut request: Request, next: Next) -> Response {
    if let Some(response) = check_host_and_origin(request.headers(), state.listen) {
        return response;
    }
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
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
    if method != Method::GET && method != Method::HEAD && !json_content_type(request.headers()) {
        return error_response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ControlErrorKind::BadRequest,
            "mutations require Content-Type: application/json",
        );
    }
    let body = std::mem::replace(request.body_mut(), Body::empty());
    let Ok(body) = axum::body::to_bytes(body, MAX_REQUEST_BODY_BYTES).await else {
        return error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            ControlErrorKind::BadRequest,
            "request body is too large",
        );
    };
    if state.control.state.draining()
        && method != Method::GET
        && !(method == Method::POST && path == "/v1/hofund/shutdown")
    {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            ControlErrorKind::Unavailable,
            "daemon is draining",
        );
    }
    *request.body_mut() = Body::from(body);
    next.run(request).await
}

async fn health(State(state): State<HttpState>) -> Response {
    json_response(
        StatusCode::OK,
        serde_json::to_vec(&state.control.state.health()).unwrap_or_default(),
    )
}

async fn status(State(state): State<HttpState>) -> Response {
    json_response(
        StatusCode::OK,
        serde_json::to_vec(&state.control.state.status()).unwrap_or_default(),
    )
}

async fn providers(State(state): State<HttpState>) -> Response {
    operation_response!(state.control.state.providers().await)
}

async fn provider_keys(
    State(state): State<HttpState>,
    id: Result<AxumPath<String>, PathRejection>,
) -> Response {
    let id = match path_id(id) {
        Ok(id) => id,
        Err(failure) => return failure_response(failure),
    };
    operation_response!(state.control.state.provider_keys(id).await)
}

async fn change_provider_key(
    State(state): State<HttpState>,
    id: Result<AxumPath<String>, PathRejection>,
    body: Bytes,
) -> Response {
    let id = match path_id(id) {
        Ok(id) => id,
        Err(failure) => return failure_response(failure),
    };
    let request = match decode_body::<ProviderKeyRequest>(&body) {
        Ok(request) => request,
        Err(failure) => return failure_response(failure),
    };
    operation_response!(state.control.state.change_provider_key(id, request).await)
}

async fn provider_models(
    State(state): State<HttpState>,
    id: Result<AxumPath<String>, PathRejection>,
) -> Response {
    let id = match path_id(id) {
        Ok(id) => id,
        Err(failure) => return failure_response(failure),
    };
    operation_response!(state.control.state.provider_models(id).await)
}

async fn refresh_models(
    State(state): State<HttpState>,
    id: Result<AxumPath<String>, PathRejection>,
    body: Bytes,
) -> Response {
    let id = match path_id(id) {
        Ok(id) => id,
        Err(failure) => return failure_response(failure),
    };
    if !body.is_empty() {
        return error_response(
            StatusCode::BAD_REQUEST,
            ControlErrorKind::BadRequest,
            "model refresh requires an empty body",
        );
    }
    operation_response!(state.control.state.refresh_models(id).await)
}

async fn test_provider(
    State(state): State<HttpState>,
    id: Result<AxumPath<String>, PathRejection>,
    body: Bytes,
) -> Response {
    let id = match path_id(id) {
        Ok(id) => id,
        Err(failure) => return failure_response(failure),
    };
    let request = match decode_body::<ProviderTestRequest>(&body) {
        Ok(request) => request,
        Err(failure) => return failure_response(failure),
    };
    operation_response!(state.control.state.test_provider(id, request).await)
}

async fn picks(
    State(state): State<HttpState>,
    id: Result<AxumPath<String>, PathRejection>,
) -> Response {
    let id = match path_id(id) {
        Ok(id) => id,
        Err(failure) => return failure_response(failure),
    };
    operation_response!(state.control.state.picks(id).await)
}

async fn set_picks(
    State(state): State<HttpState>,
    id: Result<AxumPath<String>, PathRejection>,
    body: Bytes,
) -> Response {
    let id = match path_id(id) {
        Ok(id) => id,
        Err(failure) => return failure_response(failure),
    };
    let request = match decode_body::<PicksSetRequest>(&body) {
        Ok(request) => request,
        Err(failure) => return failure_response(failure),
    };
    operation_response!(state.control.state.set_picks(id, request).await)
}

async fn route_models(State(state): State<HttpState>) -> Response {
    operation_response!(state.control.state.route_models().await)
}

async fn route_decisions(State(state): State<HttpState>, uri: Uri) -> Response {
    let query = RouteQuery::parse(uri.query());
    operation_response!(state.control.state.route_decisions(query).await)
}

async fn route_session(State(state): State<HttpState>, uri: Uri) -> Response {
    let query = RouteQuery::parse(uri.query());
    let Some(session) = query.session.clone().filter(|session| !session.is_empty()) else {
        return failure_response(
            ControlError::new(
                ControlErrorKind::BadRequest,
                "name the session with ?session=<id>",
                "control",
            )
            .into(),
        );
    };
    operation_response!(state.control.state.route_session(session, query).await)
}

async fn route_unrouted(State(state): State<HttpState>) -> Response {
    operation_response!(state.control.state.route_unrouted().await)
}

async fn agents(State(state): State<HttpState>) -> Response {
    operation_response!(state.control.state.agents().await)
}

async fn agent_status(State(state): State<HttpState>) -> Response {
    operation_response!(state.control.state.agent_status().await)
}

async fn plan_agent(
    State(state): State<HttpState>,
    id: Result<AxumPath<String>, PathRejection>,
    body: Bytes,
) -> Response {
    let id = match path_id(id) {
        Ok(id) => id,
        Err(failure) => return failure_response(failure),
    };
    let request = match decode_body::<AgentPlanRequest>(&body) {
        Ok(request) => request,
        Err(failure) => return failure_response(failure),
    };
    operation_response!(state.control.state.plan_agent(id, request).await)
}

async fn connect_agent(
    State(state): State<HttpState>,
    id: Result<AxumPath<String>, PathRejection>,
    body: Bytes,
) -> Response {
    let id = match path_id(id) {
        Ok(id) => id,
        Err(failure) => return failure_response(failure),
    };
    let request = match decode_body::<AgentConnectRequest>(&body) {
        Ok(request) => request,
        Err(failure) => return failure_response(failure),
    };
    operation_response!(state.control.state.connect_agent(id, request).await)
}

async fn disconnect_agent(
    State(state): State<HttpState>,
    id: Result<AxumPath<String>, PathRejection>,
    body: Bytes,
) -> Response {
    let id = match path_id(id) {
        Ok(id) => id,
        Err(failure) => return failure_response(failure),
    };
    let request = match decode_body::<AgentDisconnectRequest>(&body) {
        Ok(request) => request,
        Err(failure) => return failure_response(failure),
    };
    operation_response!(state.control.state.disconnect_agent(id, request).await)
}

async fn set_agent_model(
    State(state): State<HttpState>,
    id: Result<AxumPath<String>, PathRejection>,
    body: Bytes,
) -> Response {
    let id = match path_id(id) {
        Ok(id) => id,
        Err(failure) => return failure_response(failure),
    };
    let request = match decode_body::<AgentModelRequest>(&body) {
        Ok(request) => request,
        Err(failure) => return failure_response(failure),
    };
    operation_response!(state.control.state.set_agent_model(id, request).await)
}

async fn usage(
    State(state): State<HttpState>,
    uri: Uri,
    query: Result<Query<UsageQuery>, QueryRejection>,
) -> Response {
    let query = if uri.query().is_none_or(str::is_empty) {
        UsageQuery::default()
    } else {
        match query {
            Ok(Query(query)) => query,
            Err(_) => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    ControlErrorKind::BadRequest,
                    "invalid usage query",
                );
            }
        }
    };
    if let Err(error) = query.validate() {
        return failure_response(error.into());
    }
    operation_response!(state.control.state.usage(query).await)
}

async fn gateway_keys(State(state): State<HttpState>) -> Response {
    operation_response!(state.control.state.gateway_keys().await)
}

async fn change_gateway_key(State(state): State<HttpState>, body: Bytes) -> Response {
    let request = match decode_body::<GatewayKeyRequest>(&body) {
        Ok(request) => request,
        Err(failure) => return failure_response(failure),
    };
    operation_response!(state.control.state.change_gateway_key(request).await)
}

async fn export_gateway_keys(State(state): State<HttpState>) -> Response {
    operation_response!(state.control.state.export_gateway_keys().await)
}

async fn restore_gateway_keys(State(state): State<HttpState>, body: Bytes) -> Response {
    let request = match decode_body::<GatewayKeyRestoreRequest>(&body) {
        Ok(request) => request,
        Err(failure) => return failure_response(failure),
    };
    operation_response!(state.control.state.restore_gateway_keys(request).await)
}

async fn quotas(State(state): State<HttpState>, uri: Uri) -> Response {
    let query = QuotaQuery::parse(uri.query());
    operation_response!(state.control.state.quotas(query).await)
}

async fn quota_history(State(state): State<HttpState>, uri: Uri) -> Response {
    let query = QuotaHistoryQuery::parse(uri.query());
    operation_response!(state.control.state.quota_history(query).await)
}

async fn warmups(State(state): State<HttpState>) -> Response {
    operation_response!(state.control.state.warmups().await)
}

async fn run_warmup(State(state): State<HttpState>, body: Bytes) -> Response {
    let request = match decode_body::<WarmupRunRequest>(&body) {
        Ok(request) => request,
        Err(failure) => return failure_response(failure),
    };
    operation_response!(state.control.state.run_warmup(request).await)
}

async fn rotate_token(State(state): State<HttpState>, body: Bytes) -> Response {
    let request = match decode_body::<TokenRotateRequest>(&body) {
        Ok(request) => request,
        Err(failure) => return failure_response(failure),
    };
    operation_response!(state.control.state.rotate_token(request).await)
}

async fn shutdown(State(state): State<HttpState>, body: Bytes) -> Response {
    let request = match decode_body::<ShutdownRequest>(&body) {
        Ok(request) => request,
        Err(failure) => return failure_response(failure),
    };
    operation_response!(state.control.state.shutdown(request).await)
}

async fn config(State(state): State<HttpState>) -> Response {
    operation_response!(state.control.state.config().await)
}

async fn validate_config(State(state): State<HttpState>, body: Bytes) -> Response {
    let request = match decode_body::<ConfigValidateRequest>(&body) {
        Ok(request) => request,
        Err(failure) => return failure_response(failure),
    };
    operation_response!(state.control.state.validate_config(request).await)
}

async fn apply_config(State(state): State<HttpState>, body: Bytes) -> Response {
    let request = match decode_body::<ConfigApplyRequest>(&body) {
        Ok(request) => request,
        Err(failure) => return failure_response(failure),
    };
    operation_response!(state.control.state.apply_config(request).await)
}

async fn reload_config(State(state): State<HttpState>, body: Bytes) -> Response {
    let request = match decode_body::<ReloadRequest>(&body) {
        Ok(request) => request,
        Err(failure) => return failure_response(failure),
    };
    operation_response!(state.control.state.reload_config(request).await)
}

async fn events(State(state): State<HttpState>, headers: HeaderMap, uri: Uri) -> Response {
    let query = match events_query(uri.query(), &headers) {
        Ok(query) => query,
        Err(message) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                ControlErrorKind::BadRequest,
                message,
            );
        }
    };
    // `pre_route` already authenticated this exact header value.
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or_default()
        .to_owned();
    let subscription = match state.control.state.events(query) {
        Ok(subscription) => subscription,
        Err(failure) => return failure_response(failure),
    };
    let stream = heartbeat_stream(
        subscription.frames,
        Arc::clone(&state.control.tokens),
        presented,
        state.heartbeat,
    );
    let mut response = Sse::new(stream).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    response
}

/// Interleaves daemon frames with heartbeat comments. Each heartbeat re-reads
/// the token file and ends the stream when the presented token is no longer
/// current, so a rotated-away client cannot keep reading.
fn heartbeat_stream(
    frames: BoxStream<'static, SseFrame>,
    tokens: Arc<TokenState>,
    presented: String,
    heartbeat: Duration,
) -> BoxStream<'static, Result<Event, std::convert::Infallible>> {
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + heartbeat, heartbeat);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    futures_util::stream::unfold(
        (frames, ticker, tokens, presented),
        |(mut frames, mut ticker, tokens, presented)| async move {
            let event = tokio::select! {
                biased;
                frame = frames.next() => {
                    let frame = frame?;
                    Event::default().id(frame.id).event(frame.event).data(&*frame.data)
                }
                _ = ticker.tick() => {
                    if !tokens_match(&tokens.current(), &presented) {
                        return None;
                    }
                    Event::default().comment("")
                }
            };
            Some((Ok(event), (frames, ticker, tokens, presented)))
        },
    )
    .boxed()
}

fn events_query(query: Option<&str>, headers: &HeaderMap) -> Result<EventsQuery, &'static str> {
    let mut topics = None;
    for pair in query
        .unwrap_or_default()
        .split('&')
        .filter(|pair| !pair.is_empty())
    {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        match name {
            "topics" if topics.is_none() => {
                let mut parsed = Vec::new();
                for label in value.split(',') {
                    let topic = Topic::CANONICAL
                        .into_iter()
                        .find(|topic| topic.as_str() == label)
                        .ok_or("unknown event topic")?;
                    parsed.push(topic);
                }
                topics = Some(parsed);
            }
            "topics" => return Err("topics may appear only once"),
            "last_event_id" | "lastEventId" => {
                return Err("send the event cursor in the Last-Event-ID header, not the URL");
            }
            _ => return Err("unknown events query parameter"),
        }
    }
    let mut ids = headers.get_all("last-event-id").iter();
    let last_event_id = match (ids.next(), ids.next()) {
        (None, _) => None,
        (Some(_), Some(_)) => return Err("Last-Event-ID may appear only once"),
        // Invalid bytes or an oversized value reach the daemon as a malformed
        // id, which resynchronizes rather than failing the reconnect.
        (Some(value), None) => Some(
            value
                .to_str()
                .ok()
                .filter(|value| value.len() <= MAX_LAST_EVENT_ID_BYTES)
                .unwrap_or("malformed")
                .to_owned(),
        ),
    };
    let topics = topics.unwrap_or_else(|| vec![Topic::Control]);
    Ok(EventsQuery::new(&topics, last_event_id))
}

async fn not_found() -> Response {
    error_response(
        StatusCode::NOT_FOUND,
        ControlErrorKind::NotFound,
        "not found",
    )
}

async fn method_not_allowed() -> Response {
    error_response(
        StatusCode::METHOD_NOT_ALLOWED,
        ControlErrorKind::MethodNotAllowed,
        "method not allowed",
    )
}

fn path_id(id: Result<AxumPath<String>, PathRejection>) -> Result<String, ControlFailure> {
    let AxumPath(id) =
        id.map_err(|_| ControlError::new(ControlErrorKind::BadRequest, "invalid id", "http"))?;
    if id.is_empty() || id.len() > MAX_ID_BYTES {
        return Err(ControlError::new(
            ControlErrorKind::BadRequest,
            "id must be nonempty and at most 256 bytes",
            "http",
        )
        .into());
    }
    Ok(id)
}

fn decode_body<T: hofund_control_proto::Request>(body: &Bytes) -> Result<T, ControlFailure> {
    decode_request(1, body).map_err(ControlFailure::from)
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

fn failure_response(failure: ControlFailure) -> Response {
    let status = if failure.error.kind == ControlErrorKind::Upstream && failure.status == Some(429)
    {
        StatusCode::TOO_MANY_REQUESTS
    } else {
        StatusCode::from_u16(failure.error.kind.http_status())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
    };
    let mut envelope = ErrorEnvelope::default();
    envelope.error = failure.error;
    json_response(status, serde_json::to_vec(&envelope).unwrap_or_default())
}

fn json_response(status: StatusCode, body: Vec<u8>) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CACHE_CONTROL, "no-store")
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
