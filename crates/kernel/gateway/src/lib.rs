//! HTTP edge of the local gateway.
//!
//! The process authenticates a request, decodes it with the codec it was
//! given for that path, asks the router, and writes the encoded body. Vendor
//! names do not belong in this crate. Generative Language paths are matched
//! in `endpoints`.

pub mod control;
mod endpoints;
mod prev_response;
mod quotas;
mod relisten;
mod responses_ws;

pub use endpoints::{GENERATIVE_LIST, GENERATIVE_STREAM, GENERATIVE_UNARY};
#[doc(hidden)]
pub use quotas::{
    QuotaAccess, QuotaCaller, QuotaScope, QuotaScopeHook, QuotaSource, tell_everything,
};
pub use relisten::BindHook;
pub use relisten::{Relisten, RelistenError};

use std::collections::HashMap;
use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};

use axum::response::Response;
use axum::routing::{get, post};
use endpoints::{
    GENERATIVE_CATCHALL, inject_target, match_count, match_generate, percent_decode, query_key,
};
use futures_util::{Stream, StreamExt, TryStreamExt};
use hofund_codec::{Codec, StreamEncoder};
use hofund_ir::{Request as ModelRequest, StreamEvent};
use hofund_plugin_api::{
    BodyStream, InboundSurface, Method, PluginError, PluginErrorKind, ProtocolId,
};
use hofund_plugin_host::HttpBackend;
use hofund_router::{Completion, Routed, RouterError, Snapshot, WaitHook};
use hofund_usage::{Ledger, Row};
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::sync::{oneshot, watch};
use tracing::Instrument;

const BODY_LIMIT: usize = 8 * 1024 * 1024;
/// The largest body a surface takes: room for a speech-to-text upload, which
/// the common speech-to-text APIs cap at 25 MB.
const SURFACE_BODY_LIMIT: usize = 32 * 1024 * 1024;
/// How long a surface call may take: a forwarded transcription of a long
/// recording takes minutes.
const SURFACE_TIMEOUT: Duration = Duration::from_secs(180);

/// The HTTP server and the state it shares across requests.
pub struct Gateway {
    inner: Arc<Inner>,
}

struct Inner {
    snapshot: Snapshot,
    tokens: TokenState,
    codecs: HashMap<String, Arc<dyn Codec>>,
    usage: Arc<Ledger>,
    prev: prev_response::PrevStore,
    surfaces: Vec<Arc<dyn InboundSurface>>,
    /// Optional request-topic emitter. Absent when control is off.
    request_events: Option<Arc<dyn RequestEvents>>,
    surface_http: Option<Arc<dyn HttpBackend>>,
    /// Shared in-flight counter surfaced on control status and snapshots.
    in_flight: Arc<AtomicU64>,
    sockets: Arc<responses_ws::SocketRegistry>,
    /// Quota report and history source. Absent until attached.
    quota_source: Option<Arc<dyn QuotaSource>>,
    /// What each caller of the quota routes may be told.
    quota_scope: QuotaScopeHook,
}

struct TokenState {
    /// Last token read from the file, or the initial token when it cannot be read.
    token: Mutex<String>,
    token_file: Option<PathBuf>,
}

impl TokenState {
    fn new(token: String, token_file: Option<PathBuf>) -> Self {
        Self {
            token: Mutex::new(token),
            token_file,
        }
    }

    fn current(&self) -> String {
        let mut known = self
            .token
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(path) = &self.token_file
            && let Ok(text) = std::fs::read_to_string(path)
        {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                *known = trimmed.to_owned();
            }
        }
        known.clone()
    }
}

impl fmt::Debug for TokenState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("TokenState").finish_non_exhaustive()
    }
}

struct GatewayWait {
    notice: Mutex<Option<oneshot::Sender<()>>>,
}

impl GatewayWait {
    fn channel() -> (Arc<Self>, oneshot::Receiver<()>) {
        let (sender, receiver) = oneshot::channel();
        (
            Arc::new(Self {
                notice: Mutex::new(Some(sender)),
            }),
            receiver,
        )
    }
}

impl WaitHook for GatewayWait {
    fn waiting(&self) {
        if let Some(sender) = self
            .notice
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = sender.send(());
        }
    }

    fn wait(
        &self,
        duration: Duration,
    ) -> Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(tokio::time::sleep(duration))
    }

    fn now(&self) -> Instant {
        tokio::time::Instant::now().into_std()
    }
}

/// Summary emitted when an authenticated model request begins.
///
/// Carries ids and protocol only. Never headers, bodies, query strings, or
/// credentials.
#[derive(Clone, Debug)]
pub struct RequestStart {
    /// Correlation id generated by the gateway for this request.
    pub request_id: String,
    /// Client-facing route id (model name).
    pub route: String,
    /// Inbound codec / protocol id.
    pub inbound_protocol: String,
    /// Unix milliseconds when the request started.
    pub started_at_ms: u64,
}

/// Summary emitted when a model request finishes.
///
/// Usage token counts are included when the upstream reported them. Auth
/// tokens, headers, and bodies are never included.
#[derive(Clone, Debug)]
pub struct RequestEnd {
    /// Correlation id from [`RequestStart`].
    pub request_id: String,
    /// Selected upstream route id when known.
    pub member: Option<String>,
    /// Client-facing model whose call finished.
    pub model: String,
    /// HTTP status code of the response.
    pub status: u16,
    /// Elapsed milliseconds.
    pub duration_ms: u64,
    /// Input tokens when reported.
    pub input_tokens: Option<u64>,
    /// Output tokens when reported.
    pub output_tokens: Option<u64>,
}

/// Receives request start and finish summaries for the control `requests` topic.
///
/// The gateway never blocks on these callbacks. Implementations must emit
/// without waiting on slow subscribers.
pub trait RequestEvents: Send + Sync {
    /// An authenticated request is about to be dispatched.
    fn started(&self, start: &RequestStart);
    /// A request finished (success, failure, or stream end).
    fn finished(&self, end: &RequestEnd);
}

/// Tracks one in-flight request for events and the shared counter.
struct RequestTrack {
    events: Option<Arc<dyn RequestEvents>>,
    in_flight: Arc<AtomicU64>,
    request_id: String,
    model: String,
    started: Instant,
    active: bool,
    finished: bool,
    drop_status: u16,
    cancel_usage: Option<(Arc<Ledger>, UsageMeta)>,
}

impl RequestTrack {
    fn begin(app: &Inner, start: RequestStart) -> Self {
        let events = app.request_events.clone();
        let in_flight = Arc::clone(&app.in_flight);
        if let Some(ref hook) = events {
            hook.started(&start);
        }
        in_flight.fetch_add(1, Ordering::SeqCst);
        Self {
            events,
            in_flight,
            request_id: start.request_id,
            model: start.route,
            started: Instant::now(),
            active: true,
            finished: false,
            drop_status: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
            cancel_usage: None,
        }
    }

    fn finish(&mut self, end: RequestEnd) {
        if self.finished {
            return;
        }
        self.finished = true;
        if let Some(ref hook) = self.events {
            hook.finished(&end);
        }
        self.release();
    }

    fn release(&mut self) {
        if self.active {
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            self.active = false;
        }
    }

    fn client_waiting(&mut self, usage: Arc<Ledger>, request: &ModelRequest) {
        self.drop_status = 499;
        self.cancel_usage = Some((usage, UsageMeta::new(request, "", self.started)));
    }

    fn end_with(
        &mut self,
        member: Option<String>,
        status: u16,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
    ) {
        let duration_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.finish(RequestEnd {
            request_id: self.request_id.clone(),
            member,
            model: self.model.clone(),
            status,
            duration_ms,
            input_tokens,
            output_tokens,
        });
    }

    /// Moves tracking into a streaming body. The caller must not finish again.
    fn take_for_stream(&mut self) -> Self {
        let owned = Self {
            events: self.events.take(),
            in_flight: Arc::clone(&self.in_flight),
            request_id: self.request_id.clone(),
            model: self.model.clone(),
            started: self.started,
            active: self.active,
            finished: self.finished,
            drop_status: self.drop_status,
            cancel_usage: self.cancel_usage.take(),
        };
        self.active = false;
        self.finished = true;
        owned
    }
}

impl Drop for RequestTrack {
    fn drop(&mut self) {
        if !self.finished {
            // Panic, cancellation, or a path that forgot finish: still emit and
            // return the counter to zero.
            let status = self.drop_status;
            if status == 499
                && let Some((usage, meta)) = self.cancel_usage.take()
            {
                write_usage(&usage, &meta, None, StatusCode::from_u16(499).expect("499"));
            }
            self.end_with(None, status, None, None);
        } else {
            self.release();
        }
    }
}

static REQUEST_SEQ: AtomicU64 = AtomicU64::new(1);

fn next_request_id() -> String {
    format!("r{}", REQUEST_SEQ.fetch_add(1, Ordering::Relaxed))
}

fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// Maximum graceful drain for a control listener before its connections close.
#[doc(hidden)]
pub const CONTROL_DRAIN: std::time::Duration = std::time::Duration::from_secs(4);

/// A bound server.
pub struct Running {
    /// The address the listener accepted at start. After a relisten,
    /// [`Running::listen_addr`] gives the address served now.
    pub local_addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
    attached: Vec<tokio::task::JoinHandle<()>>,
    linked: Vec<Running>,
    control_addr: Option<SocketAddr>,
    control_socket_path: Option<PathBuf>,
    resources: Vec<Box<dyn std::any::Any + Send>>,
    shutdown_request: Option<(watch::Sender<bool>, watch::Receiver<bool>)>,
    drain_bound: Option<std::time::Duration>,
    sockets: Option<Arc<responses_ws::SocketRegistry>>,
    relisten: Option<Relisten>,
    fatal: Option<watch::Receiver<Option<String>>>,
}

/// The listener could not be bound.
#[derive(Debug, Error)]
pub enum GatewayError {
    /// Binding the address failed.
    #[error("cannot bind {listen}: {source}")]
    Bind {
        /// The address that was requested.
        listen: SocketAddr,
        /// The operating-system error.
        source: std::io::Error,
    },
    /// A control listener was requested on a non-loopback address.
    #[error("control listener must use a numeric loopback address, got {listen}")]
    NonLoopback {
        /// The address that was rejected.
        listen: SocketAddr,
    },
    /// Binding a Unix control socket failed.
    #[cfg(unix)]
    #[error("cannot bind control socket {path}: {source}")]
    BindUnix {
        /// The socket path that was requested.
        path: PathBuf,
        /// The operating-system error.
        source: std::io::Error,
    },
    /// A Unix control socket exceeds the platform path limit.
    #[cfg(unix)]
    #[error("control socket path exceeds the platform limit: {path}")]
    SocketPathTooLong {
        /// The rejected socket path.
        path: PathBuf,
    },
}

impl Gateway {
    /// A gateway that routes with `snapshot`, requires `token`, and decodes each
    /// path with the codec in `codecs`.
    ///
    /// Each request loads the snapshot for that call. A later store does not
    /// change a call that already loaded.
    #[must_use]
    pub fn new(
        snapshot: Snapshot,
        token: String,
        codecs: HashMap<String, Arc<dyn Codec>>,
        usage: Arc<Ledger>,
    ) -> Self {
        Self::build(snapshot, token, None, codecs, usage)
    }

    /// Like [`Self::new`], and re-reads `token_file` on each request.
    ///
    /// A failed or empty read keeps the last token that was read. The file is
    /// replaced by rename, so a reader sees the previous token or the new one.
    #[must_use]
    pub fn with_token_file(
        snapshot: Snapshot,
        token: String,
        token_file: PathBuf,
        codecs: HashMap<String, Arc<dyn Codec>>,
        usage: Arc<Ledger>,
    ) -> Self {
        Self::build(snapshot, token, Some(token_file), codecs, usage)
    }

    fn build(
        snapshot: Snapshot,
        token: String,
        token_file: Option<PathBuf>,
        codecs: HashMap<String, Arc<dyn Codec>>,
        usage: Arc<Ledger>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                snapshot,
                tokens: TokenState::new(token, token_file),
                codecs,
                usage,
                prev: prev_response::PrevStore::new(),
                surfaces: Vec::new(),
                request_events: None,
                surface_http: None,
                in_flight: Arc::new(AtomicU64::new(0)),
                sockets: Arc::new(responses_ws::SocketRegistry::default()),
                quota_source: None,
                quota_scope: tell_everything(),
            }),
        }
    }

    /// Mounts inbound surfaces. A request that matches one is authenticated,
    /// then handled inside the inbound span with a 60 second deadline.
    ///
    /// Codec routes stay on their own paths. A surface is reached from the
    /// fallback, so a codec path wins when both could match.
    #[must_use]
    pub fn with_surfaces(mut self, surfaces: Vec<Arc<dyn InboundSurface>>) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("gateway is not shared yet")
            .surfaces = surfaces;
        self
    }

    /// Gives inbound surfaces outbound HTTP on `http`, the kernel's pooled
    /// client. Without it a surface's [`hofund_plugin_api::SurfaceHost::http`]
    /// is `None`.
    #[must_use]
    pub fn with_surface_http(mut self, http: Arc<dyn HttpBackend>) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("gateway is not shared yet")
            .surface_http = Some(http);
        self
    }

    /// Attaches a request-topic emitter and a shared in-flight counter.
    ///
    /// Call before [`Self::bind`]. The same `in_flight` Arc is read by control
    /// status and snapshot builders.
    #[must_use]
    pub fn with_request_events(
        mut self,
        events: Arc<dyn RequestEvents>,
        in_flight: Arc<AtomicU64>,
    ) -> Self {
        let inner = Arc::get_mut(&mut self.inner).expect("gateway is not shared yet");
        inner.request_events = Some(events);
        inner.in_flight = in_flight;
        self
    }

    /// Serves `GET /v1/hofund/quotas` and `GET /v1/hofund/quotas/history`
    /// from `source`, telling every token holder everything until
    /// [`Self::with_quota_scope`] narrows it. Call before [`Self::bind`].
    #[must_use]
    pub fn with_quotas(mut self, source: Arc<dyn QuotaSource>) -> Self {
        let inner = Arc::get_mut(&mut self.inner).expect("gateway is not shared yet");
        inner.quota_source = Some(source);
        self
    }

    /// Sets what each caller of the quota routes may be told. Has no effect
    /// unless [`Self::with_quotas`] is also called. Call before [`Self::bind`].
    #[must_use]
    pub fn with_quota_scope(mut self, scope: QuotaScopeHook) -> Self {
        let inner = Arc::get_mut(&mut self.inner).expect("gateway is not shared yet");
        inner.quota_scope = scope;
        self
    }

    /// The axum router. `/health` is open. Every other route requires a token.
    ///
    /// Codec paths that contain `{` are map keys, not routable patterns. When
    /// a Generative Language codec is registered, the process binds the
    /// catch-all generate route and the model list instead.
    pub fn into_router(self) -> axum::Router {
        request_router(&self.inner)
    }

    /// Binds `listen` and serves until [`Running`] shuts down.
    ///
    /// # Errors
    ///
    /// Returns [`GatewayError`] when the address cannot be bound.
    ///
    /// The returned server can move to another address with
    /// [`Running::relisten`].
    pub async fn bind(self, listen: SocketAddr) -> Result<Running, GatewayError> {
        let sockets = Arc::clone(&self.inner.sockets);
        serve_relistenable(self.into_router(), listen, sockets).await
    }
}

/// Serves `router` on `listen` behind a swappable accept loop.
async fn serve_relistenable(
    router: axum::Router,
    listen: SocketAddr,
    sockets: Arc<responses_ws::SocketRegistry>,
) -> Result<Running, GatewayError> {
    let listener = TcpListener::bind(listen)
        .await
        .map_err(|source| GatewayError::Bind { listen, source })?;
    let local_addr = listener
        .local_addr()
        .map_err(|source| GatewayError::Bind { listen, source })?;
    let (accept, handle) = relisten::swappable(listener, listen, local_addr);
    let (shutdown, signal) = oneshot::channel();
    let task = tokio::spawn(async move {
        axum::serve(accept, router)
            .with_graceful_shutdown(async move {
                let _ = signal.await;
            })
            .await
    });
    let mut running = Running::new(local_addr, shutdown, task);
    running.relisten = Some(handle);
    running.sockets = Some(sockets);
    Ok(running)
}

impl Running {
    fn new(
        local_addr: SocketAddr,
        shutdown: oneshot::Sender<()>,
        task: tokio::task::JoinHandle<std::io::Result<()>>,
    ) -> Self {
        Self {
            local_addr,
            shutdown: Some(shutdown),
            task: Some(task),
            attached: Vec::new(),
            linked: Vec::new(),
            control_addr: None,
            control_socket_path: None,
            resources: Vec::new(),
            shutdown_request: None,
            drain_bound: None,
            sockets: None,
            relisten: None,
            fatal: None,
        }
    }

    /// The address the model listener serves now: the start address until a
    /// relisten moves it.
    #[must_use]
    pub fn listen_addr(&self) -> SocketAddr {
        self.relisten
            .as_ref()
            .map_or(self.local_addr, Relisten::local_addr)
    }

    /// A handle that moves this server's listener, shared with its clones.
    /// `None` for control listeners, which never move.
    #[must_use]
    pub fn relisten_handle(&self) -> Option<Relisten> {
        self.relisten.clone()
    }

    /// Moves the listener to `to`; see [`Relisten::relisten`].
    ///
    /// # Errors
    ///
    /// Returns [`RelistenError`] when the move fails, and
    /// [`RelistenError::Stopped`] for a server that cannot move.
    pub async fn relisten(&self, to: SocketAddr) -> Result<SocketAddr, RelistenError> {
        match &self.relisten {
            Some(handle) => handle.relisten(to).await,
            None => Err(RelistenError::Stopped),
        }
    }

    /// Runs `hook` before each relisten bind, for tests that need one to fail.
    /// Call it before taking a [`Running::relisten_handle`].
    #[doc(hidden)]
    #[must_use]
    pub fn with_bind_hook(mut self, hook: BindHook) -> Self {
        if let Some(handle) = self.relisten.as_mut() {
            handle.set_hook(hook);
        }
        self
    }

    /// Attaches the channel on which the daemon reports a fatal error. A
    /// reported error also resolves [`Running::shutdown_requested`].
    #[doc(hidden)]
    #[must_use]
    pub fn with_fatal(mut self, fatal: watch::Receiver<Option<String>>) -> Self {
        self.fatal = Some(fatal);
        self
    }

    /// The fatal error the daemon reported, if any. A server that stops for
    /// this reason should exit non-zero.
    #[must_use]
    pub fn fatal(&self) -> Option<String> {
        self.fatal.as_ref().and_then(|fatal| fatal.borrow().clone())
    }

    /// Attaches the flag a control client or embedded shutdown sets to request shutdown.
    #[doc(hidden)]
    #[must_use]
    pub fn with_shutdown_request(mut self, request: watch::Sender<bool>) -> Self {
        self.shutdown_request = Some((request.clone(), request.subscribe()));
        self
    }

    /// Resolves once a linked control client requests shutdown.
    ///
    /// Without an attached request flag, or after its sender is gone without
    /// a request, this never resolves; a signal handler must stop the server.
    ///
    /// A fatal error reported through [`Running::with_fatal`] resolves it too.
    pub async fn shutdown_requested(&mut self) {
        let (request, fatal) = (self.shutdown_request.as_mut(), self.fatal.as_mut());
        let requested = async {
            let Some((_, request)) = request else {
                return std::future::pending().await;
            };
            if request.wait_for(|requested| *requested).await.is_err() {
                std::future::pending::<()>().await;
            }
        };
        let fatal = async {
            let Some(fatal) = fatal else {
                return std::future::pending().await;
            };
            if fatal.wait_for(Option::is_some).await.is_err() {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            () = requested => {}
            () = fatal => {}
        }
    }

    /// Bounds the graceful drain for a control listener.
    fn with_control_drain(mut self) -> Self {
        self.drain_bound = Some(CONTROL_DRAIN);
        self
    }

    /// Attaches a background task aborted by both shutdown and drop.
    #[doc(hidden)]
    #[must_use]
    pub fn with_task(mut self, task: tokio::task::JoinHandle<()>) -> Self {
        self.attached.push(task);
        self
    }

    /// Keeps a daemon resource alive for this server's lifetime.
    #[doc(hidden)]
    #[must_use]
    pub fn with_resource<T: std::any::Any + Send>(mut self, resource: T) -> Self {
        self.resources.push(Box::new(resource));
        self
    }

    /// Links a control listener to this server's lifetime.
    #[doc(hidden)]
    #[must_use]
    pub fn with_control(mut self, control: Running) -> Self {
        self.control_addr = Some(control.local_addr);
        self.linked.push(control);
        self
    }

    /// Links a Unix control listener to this server's lifetime.
    #[cfg(unix)]
    #[doc(hidden)]
    #[must_use]
    pub fn with_unix_control(mut self, control: Running, path: PathBuf) -> Self {
        self.control_socket_path = Some(path);
        self.linked.push(control);
        self
    }

    /// Returns the bound control address when a control listener is linked.
    #[must_use]
    pub fn control_local_addr(&self) -> Option<SocketAddr> {
        self.control_addr
    }

    /// Returns the bound Unix control socket when one is linked.
    #[cfg(unix)]
    #[must_use]
    pub fn control_socket_path(&self) -> Option<&std::path::Path> {
        self.control_socket_path.as_deref()
    }

    fn request_shutdown(&self) {
        if let Some((request, _)) = &self.shutdown_request {
            request.send_replace(true);
        }
    }

    /// Asks the server to stop, aborts attached tasks, and waits for the server.
    /// Control listeners close connections that outlast their drain deadline.
    /// Responses sockets close after their turns; dropping a bounded drain cuts them.
    pub async fn shutdown(mut self) {
        self.request_shutdown();
        for task in self.attached.drain(..) {
            task.abort();
        }
        if let Some(sockets) = &self.sockets {
            sockets.drain();
        }
        for linked in self.linked.drain(..) {
            Box::pin(linked.shutdown()).await;
        }
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(sockets) = &self.sockets {
            sockets.wait().await;
        }
        if let Some(mut task) = self.task.take() {
            match self.drain_bound {
                Some(bound) => {
                    if tokio::time::timeout(bound, &mut task).await.is_err() {
                        task.abort();
                        let _ = task.await;
                    }
                }
                None => {
                    let _ = task.await;
                }
            }
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        for task in self.attached.drain(..) {
            task.abort();
        }
        if let Some(sockets) = &self.sockets {
            sockets.cut();
        }
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

/// The complete outer request path, also used to replay a socket turn.
fn request_router(app: &Arc<Inner>) -> axum::Router {
    let mut routes = codec_routes(app);
    if app.quota_source.is_some() {
        routes = quotas::routes(routes);
    }
    if !app.surfaces.is_empty() {
        routes = routes.fallback(dispatch_surface);
    }
    routes
        .with_state(Arc::clone(app))
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(app),
            responses_ws::path_guard,
        ))
}

/// `/health`, the model lists and every codec route. Surfaces are not here,
/// so the in-process gateway a surface calls cannot reach another surface.
fn codec_routes(inner: &Inner) -> axum::Router<Arc<Inner>> {
    let mut routes = axum::Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(models));
    let generative =
        inner.codecs.contains_key(GENERATIVE_UNARY) || inner.codecs.contains_key(GENERATIVE_STREAM);
    let responses = inner
        .codecs
        .values()
        .any(|codec| codec.id() == ProtocolId::OPENAI_RESPONSES.as_str());
    let paths: Vec<String> = inner.codecs.keys().cloned().collect();
    let messages = paths.iter().any(|path| path == "/v1/messages");
    for path in paths {
        if path.contains('{') || responses && responses_ws::is_responses_path(&path) {
            continue;
        }
        routes = routes.route(&path, post(complete));
    }
    if responses {
        routes = routes
            .route("/v1/responses", post(complete).get(responses_ws::get))
            .route("/responses", post(complete).get(responses_ws::get));
    }
    if messages {
        routes = routes.route("/v1/messages/count_tokens", post(complete));
    }
    if generative {
        routes = routes
            .route(GENERATIVE_CATCHALL, post(complete))
            .route(GENERATIVE_LIST, get(generative_models));
    }
    routes.fallback(not_found)
}

/// The gateway's codec routes, run in process for an inbound surface.
///
/// The request presents the current gateway token as its bearer. Any other
/// credential the surface sets is ignored: one matching secret is enough for
/// `authorized`, so it can neither help nor hurt.
struct InProcess {
    app: std::sync::Weak<Inner>,
}

#[async_trait::async_trait]
impl hofund_plugin_host::GatewayBackend for InProcess {
    async fn call(
        &self,
        request: axum::http::Request<hofund_plugin_api::Body>,
    ) -> Result<axum::http::Response<BodyStream>, PluginError> {
        let Some(app) = self.app.upgrade() else {
            return Err(PluginError::cancelled("the gateway is shutting down"));
        };
        let (mut parts, body) = request.into_parts();
        let bearer =
            axum::http::HeaderValue::try_from(format!("Bearer {}", current_token(&app)))
                .map_err(|_| PluginError::internal("the gateway token is not a header value"))?;
        parts.headers.insert(header::AUTHORIZATION, bearer);
        let request = Request::from_parts(parts, Body::from(body.bytes().clone()));
        let router = codec_routes(&app).with_state(Arc::clone(&app));
        let response = tower::ServiceExt::oneshot(router, request)
            .await
            .unwrap_or_else(|never| match never {});
        let (parts, body) = response.into_parts();
        let stream = body
            .into_data_stream()
            .map(|chunk| chunk.map_err(|error| PluginError::transport(error.to_string())));
        Ok(axum::http::Response::from_parts(
            parts,
            BodyStream::from_stream(stream),
        ))
    }

    async fn forward(
        &self,
        call: hofund_plugin_api::Forward,
    ) -> Result<hofund_plugin_api::RawAnswer, PluginError> {
        let Some(app) = self.app.upgrade() else {
            return Err(PluginError::cancelled("the gateway is shutting down"));
        };
        let started = Instant::now();
        let mut track = RequestTrack::begin(
            &app,
            RequestStart {
                request_id: next_request_id(),
                route: call.model().to_owned(),
                inbound_protocol: call.path().to_owned(),
                started_at_ms: unix_ms_now(),
            },
        );
        let router = app.snapshot.load();
        let usage = |upstream: &str| UsageMeta {
            model: call.model().to_owned(),
            upstream: upstream.to_owned(),
            started,
            input: None,
            output: None,
        };
        match router.forward(&call).await {
            Ok(forwarded) => {
                let answer = forwarded.answer;
                let status =
                    StatusCode::from_u16(answer.status()).unwrap_or(StatusCode::BAD_GATEWAY);
                let (input, output) = forwarded_tokens(answer.body());
                write_usage(
                    &app.usage,
                    &usage(&forwarded.upstream_id),
                    Some((input, output)),
                    status,
                );
                track.end_with(
                    Some(forwarded.upstream_id),
                    status.as_u16(),
                    input.and_then(|value| u64::try_from(value).ok()),
                    output.and_then(|value| u64::try_from(value).ok()),
                );
                Ok(answer)
            }
            Err(error) => {
                let (upstream, error) = match error {
                    RouterError::NoRoute { .. } => (
                        String::new(),
                        PluginError::status_error(404, error.to_string()),
                    ),
                    RouterError::Ambiguous { .. }
                    | RouterError::PreviousResponseNotFound { .. } => (
                        String::new(),
                        PluginError::status_error(400, error.to_string()),
                    ),
                    RouterError::Upstream { upstream_id, error }
                        if error.kind() == PluginErrorKind::Unsupported =>
                    {
                        let message = format!(
                            "no provider of model `{}` serves {}",
                            call.model(),
                            call.path()
                        );
                        (upstream_id, PluginError::status_error(404, message))
                    }
                    RouterError::Upstream { upstream_id, error } => (upstream_id, error),
                };
                let status = status_of(&error);
                write_usage(&app.usage, &usage(&upstream), None, status);
                let member = (!upstream.is_empty()).then_some(upstream);
                track.end_with(member, status.as_u16(), None, None);
                Err(error)
            }
        }
    }
}

/// Input and output tokens in a forwarded JSON answer, read the way the
/// reference reads them: `usage.prompt_tokens`, `usage.input_tokens`,
/// `usage.total_tokens`, or `meta.billed_units.input_tokens` for input, and
/// `usage.output_tokens` for output.
fn forwarded_tokens(body: &[u8]) -> (Option<i64>, Option<i64>) {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return (None, None);
    };
    let count = |pointer: &str| {
        value
            .pointer(pointer)
            .and_then(serde_json::Value::as_i64)
            .filter(|tokens| *tokens > 0)
    };
    let input = count("/usage/prompt_tokens")
        .or_else(|| count("/usage/input_tokens"))
        .or_else(|| count("/usage/total_tokens"))
        .or_else(|| count("/meta/billed_units/input_tokens"));
    (input, count("/usage/output_tokens"))
}

async fn not_found(request: Request) -> Response {
    if responses_ws::wants_socket(request.method(), request.headers()) {
        return responses_ws::upgrade_required();
    }
    text_response(StatusCode::NOT_FOUND, "not found")
}

async fn dispatch_surface(State(app): State<Arc<Inner>>, request: Request) -> Response {
    if responses_ws::wants_socket(request.method(), request.headers()) {
        return responses_ws::get(State(app), request).await;
    }
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let Some(surface) = app.surfaces.iter().find(|surface| {
        surface
            .routes()
            .iter()
            .any(|route| method_is(route.method(), &method) && path_matches(route.prefix(), &path))
    }) else {
        return text_response(StatusCode::NOT_FOUND, "not found");
    };
    let surface = Arc::clone(surface);
    let uri = request.uri().clone();
    let headers = request.headers().clone();
    if let Some(response) = reject(&current_token(&app), &headers, &uri, None) {
        return response;
    }
    let Some(route) =
        surface.routes().iter().copied().find(|route| {
            method_is(route.method(), &method) && path_matches(route.prefix(), &path)
        })
    else {
        return text_response(StatusCode::NOT_FOUND, "not found");
    };
    let (mut parts, body) = request.into_parts();
    without_own_token(&current_token(&app), &mut parts);
    let bytes = match axum::body::to_bytes(body, SURFACE_BODY_LIMIT).await {
        Ok(bytes) => bytes,
        Err(_) => return text_response(StatusCode::PAYLOAD_TOO_LARGE, "request body is too large"),
    };
    let inbound =
        axum::http::Request::from_parts(parts, hofund_plugin_api::Body::from_bytes(bytes));
    let gateway = Arc::new(InProcess {
        app: Arc::downgrade(&app),
    });
    let members = app.snapshot.load().member_ids();
    let protocols = protocols_of(&app.codecs);
    let host = match &app.surface_http {
        Some(http) => hofund_plugin_host::surface_host_with_http(
            members,
            protocols,
            true,
            gateway,
            Arc::clone(http),
        ),
        None => hofund_plugin_host::surface_host_with_gateway(members, protocols, true, gateway),
    };
    let plugin_id = surface.id().as_str().to_owned();
    let span = tracing::info_span!(
        "inbound",
        "plugin.id" = plugin_id,
        "http.method" = method.as_str(),
        "http.route" = route.prefix(),
    );
    match tokio::time::timeout(
        SURFACE_TIMEOUT,
        async move { surface.handle(&route, inbound, &host).await }.instrument(span),
    )
    .await
    {
        Ok(Ok(response)) => surface_response(response),
        Ok(Err(error)) => surface_error(error),
        Err(_) => text_response(StatusCode::GATEWAY_TIMEOUT, "kernel deadline exceeded"),
    }
}

fn protocols_of(codecs: &HashMap<String, Arc<dyn Codec>>) -> Vec<ProtocolId> {
    let mut ids = Vec::new();
    for codec in codecs.values() {
        if let Ok(id) = ProtocolId::try_from(codec.id())
            && !ids.contains(&id)
        {
            ids.push(id);
        }
    }
    ids
}

fn method_is(route: Method, method: &axum::http::Method) -> bool {
    let name = match route {
        Method::Get => "GET",
        Method::Post => "POST",
        Method::Put => "PUT",
        Method::Patch => "PATCH",
        Method::Delete => "DELETE",
        _ => return false,
    };
    method.as_str().eq_ignore_ascii_case(name)
}

fn path_matches(prefix: &str, path: &str) -> bool {
    path == prefix
        || prefix == "/"
        || (path.len() > prefix.len()
            && path.starts_with(prefix)
            && path.as_bytes()[prefix.len()] == b'/')
}

fn surface_response(response: axum::http::Response<BodyStream>) -> Response {
    let (parts, body) = response.into_parts();
    let stream = futures_util::stream::unfold(body, |mut body| async move {
        match body.next_chunk().await {
            Some(Ok(bytes)) => Some((Ok::<Bytes, std::io::Error>(bytes), body)),
            Some(Err(error)) => Some((Err(std::io::Error::other(error.to_string())), body)),
            None => None,
        }
    });
    let mut out = Response::new(Body::from_stream(stream));
    *out.status_mut() = parts.status;
    *out.headers_mut() = parts.headers;
    out
}

fn surface_error(error: PluginError) -> Response {
    let status = error
        .status()
        .and_then(|code| StatusCode::from_u16(code).ok())
        .unwrap_or(StatusCode::BAD_GATEWAY);
    text_response(status, error.message())
}

async fn health() -> Response {
    json_response(
        StatusCode::OK,
        br#"{"status":"ok","service":"hofund"}"#.to_vec(),
    )
}

async fn models(State(app): State<Arc<Inner>>, request: Request) -> Response {
    if let Some(response) = reject(&current_token(&app), request.headers(), request.uri(), None) {
        return response;
    }
    let data: Vec<_> = app
        .snapshot
        .load()
        .models()
        .into_iter()
        .map(|id| serde_json::json!({"id": id, "object": "model", "owned_by": "hofund"}))
        .collect();
    json_response(
        StatusCode::OK,
        serde_json::to_vec(&serde_json::json!({"object": "list", "data": data}))
            .unwrap_or_else(|_| b"{\"object\":\"list\",\"data\":[]}".to_vec()),
    )
}

async fn generative_models(State(app): State<Arc<Inner>>, request: Request) -> Response {
    let codec = app
        .codecs
        .get(GENERATIVE_UNARY)
        .or_else(|| app.codecs.get(GENERATIVE_STREAM))
        .map(AsRef::as_ref);
    if let Some(response) = reject(
        &current_token(&app),
        request.headers(),
        request.uri(),
        codec,
    ) {
        return response;
    }
    let models: Vec<_> = app
        .snapshot
        .load()
        .models()
        .into_iter()
        .map(|id| {
            serde_json::json!({
                "name": format!("models/{id}"),
                "displayName": id,
                "supportedGenerationMethods": ["generateContent", "streamGenerateContent"],
            })
        })
        .collect();
    json_response(
        StatusCode::OK,
        serde_json::to_vec(&serde_json::json!({"models": models}))
            .unwrap_or_else(|_| b"{\"models\":[]}".to_vec()),
    )
}

async fn complete(State(app): State<Arc<Inner>>, request: Request) -> Response {
    let path = request.uri().path().to_owned();
    let uri = request.uri().clone();
    let headers = request.headers().clone();
    if path == "/v1/messages/count_tokens" {
        return count_tokens(app, headers, uri, request, "/v1/messages", None).await;
    }
    if let Some(model) = match_count(&path) {
        return count_tokens(app, headers, uri, request, GENERATIVE_UNARY, Some(model)).await;
    }
    let matched = match_generate(&path);
    let codec_path = if path == "/responses" {
        "/v1/responses"
    } else {
        path.as_str()
    };
    let codec_key = matched
        .as_ref()
        .map(|route| route.key)
        .unwrap_or(codec_path);
    let Some(codec) = app.codecs.get(codec_key).cloned() else {
        return text_response(StatusCode::NOT_FOUND, "not found");
    };
    if let Some(response) = reject(&current_token(&app), &headers, &uri, Some(codec.as_ref())) {
        return response;
    }
    let started = Instant::now();
    let bytes = match axum::body::to_bytes(request.into_body(), BODY_LIMIT).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return coded(
                codec.as_ref(),
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body is too large",
            );
        }
    };
    let bytes = if let Some(route) = &matched {
        Bytes::from(inject_target(&bytes, &route.model, route.stream))
    } else {
        bytes
    };
    let expansion = (codec.id() == ProtocolId::OPENAI_RESPONSES.as_str())
        .then(|| app.prev.expand(&prev_response::request_scope(), &bytes));
    let bytes = expansion
        .as_ref()
        .map_or(bytes, |expansion| Bytes::from(expansion.body.clone()));
    let ask = expansion
        .as_ref()
        .and_then(|expansion| expansion.ask.clone());
    let capture = expansion.and_then(|expansion| expansion.capture);
    let model_request = match codec.decode_request(&bytes) {
        Ok(model_request) => model_request,
        Err(error) => {
            write_usage(
                &app.usage,
                &UsageMeta::new(&ModelRequest::new(model_name(&bytes)), "", started),
                None,
                StatusCode::BAD_REQUEST,
            );
            return coded(codec.as_ref(), StatusCode::BAD_REQUEST, &error.to_string());
        }
    };
    // Authenticated request with a decoded body: start the request topic and
    // the in-flight counter before dispatch.
    let mut track = RequestTrack::begin(
        &app,
        RequestStart {
            request_id: next_request_id(),
            route: model_request.model.clone(),
            inbound_protocol: codec.id().to_owned(),
            started_at_ms: unix_ms_now(),
        },
    );
    let router = app.snapshot.load();
    let attempt = ask.as_ref().map(|ask| ask.attempt());
    let (wait, notice) = GatewayWait::channel();
    let mut dispatch: DispatchFuture = Box::pin({
        let request = model_request.clone();
        let protocol = codec.id().to_owned();
        async move {
            router
                .dispatch_with_wait(&request, &protocol, attempt, Some(wait))
                .await
        }
    });
    let result = tokio::select! {
        result = &mut dispatch => result,
        _ = notice => {
            track.client_waiting(Arc::clone(&app.usage), &model_request);
            if model_request.stream {
                return held_response(
                    app,
                    codec,
                    model_request,
                    started,
                    track,
                    capture,
                    dispatch,
                );
            }
            dispatch.await
        }
    };
    match result {
        Ok(routed) => {
            success(
                app,
                codec,
                &model_request,
                &routed.upstream_id,
                &routed.upstream_model,
                routed.completion,
                started,
                &mut track,
                capture,
            )
            .await
        }
        Err(error) => failure(
            app,
            codec.as_ref(),
            &model_request,
            error,
            started,
            &mut track,
        ),
    }
}

type DispatchFuture =
    Pin<Box<dyn std::future::Future<Output = Result<Routed, RouterError>> + Send>>;
type BytesStream = Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>;

struct HeldResponse {
    dispatch: Option<DispatchFuture>,
    app: Arc<Inner>,
    codec: Arc<dyn Codec>,
    request: ModelRequest,
    started: Instant,
    track: Option<RequestTrack>,
    capture: Option<prev_response::Capture>,
    incoming: Option<BytesStream>,
    keepalive: Pin<Box<tokio::time::Sleep>>,
}

fn held_response(
    app: Arc<Inner>,
    codec: Arc<dyn Codec>,
    request: ModelRequest,
    started: Instant,
    track: RequestTrack,
    capture: Option<prev_response::Capture>,
    dispatch: DispatchFuture,
) -> Response {
    let content_type = codec.stream_content_type();
    let state = HeldResponse {
        dispatch: Some(dispatch),
        app,
        codec,
        request,
        started,
        track: Some(track),
        capture,
        incoming: None,
        keepalive: Box::pin(tokio::time::sleep(Duration::ZERO)),
    };
    let body = futures_util::stream::unfold((state, false), |(mut state, done)| async move {
        if done {
            return None;
        }
        loop {
            if let Some(incoming) = state.incoming.as_mut() {
                if let Some(chunk) = incoming.next().await {
                    return Some((chunk, (state, false)));
                }
                return None;
            }
            let dispatch = state.dispatch.as_mut().expect("held dispatch");
            tokio::select! {
                () = &mut state.keepalive => {
                    state.keepalive.as_mut().reset(
                        tokio::time::Instant::now() + Duration::from_secs(5),
                    );
                    return Some((
                        Ok(Bytes::from_static(b": keepalive\n\n")),
                        (state, false),
                    ));
                }
                result = dispatch => {
                    state.dispatch = None;
                    match result {
                        Ok(routed) => {
                            let response = success(
                                Arc::clone(&state.app),
                                Arc::clone(&state.codec),
                                &state.request,
                                &routed.upstream_id,
                                &routed.upstream_model,
                                routed.completion,
                                state.started,
                                state.track.as_mut().expect("held request track"),
                                state.capture.take(),
                            )
                            .await;
                            let stream = response
                                .into_body()
                                .into_data_stream()
                                .map_err(std::io::Error::other);
                            state.incoming = Some(Box::pin(stream));
                        }
                        Err(error) => {
                            let (status, message) = stream_error(&error);
                            let _ = failure(
                                Arc::clone(&state.app),
                                state.codec.as_ref(),
                                &state.request,
                                error,
                                state.started,
                                state.track.as_mut().expect("held request track"),
                            );
                            let bytes = state
                                .codec
                                .encode_stream_error(status.as_u16(), &message);
                            return Some((Ok(Bytes::from(bytes)), (state, true)));
                        }
                    }
                }
            }
        }
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from_stream(body))
        .unwrap_or_else(|_| text_response(StatusCode::INTERNAL_SERVER_ERROR, "stream"))
}

fn stream_error(error: &RouterError) -> (StatusCode, String) {
    match error {
        RouterError::NoRoute { .. } => (StatusCode::NOT_FOUND, error.to_string()),
        RouterError::Ambiguous { .. } | RouterError::PreviousResponseNotFound { .. } => {
            (StatusCode::BAD_REQUEST, error.to_string())
        }
        RouterError::Upstream { error, .. } => (status_of(error), error.message().to_owned()),
    }
}

async fn count_tokens(
    app: Arc<Inner>,
    headers: HeaderMap,
    uri: Uri,
    request: Request,
    codec_key: &str,
    path_model: Option<String>,
) -> Response {
    let Some(codec) = app.codecs.get(codec_key).cloned() else {
        return text_response(StatusCode::NOT_FOUND, "not found");
    };
    if let Some(response) = reject(&current_token(&app), &headers, &uri, Some(codec.as_ref())) {
        return response;
    }
    let bytes = match axum::body::to_bytes(request.into_body(), BODY_LIMIT).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return coded(
                codec.as_ref(),
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body is too large",
            );
        }
    };
    let bytes = if let Some(model) = &path_model {
        Bytes::from(inject_target(&bytes, model, false))
    } else {
        bytes
    };
    let model_request = match codec.decode_request(&bytes) {
        Ok(model_request) => model_request,
        Err(error) => return coded(codec.as_ref(), StatusCode::BAD_REQUEST, &error.to_string()),
    };
    let router = app.snapshot.load();
    match router.count(&model_request, codec.id()).await {
        Ok(count) => json_response(
            StatusCode::OK,
            codec.encode_token_count(count.tokens, count.estimated),
        ),
        Err(error) => {
            let (status, message) = match error {
                RouterError::NoRoute { .. } => (StatusCode::NOT_FOUND, error.to_string()),
                RouterError::Ambiguous { .. } | RouterError::PreviousResponseNotFound { .. } => {
                    (StatusCode::BAD_REQUEST, error.to_string())
                }
                RouterError::Upstream { error, .. } => {
                    (status_of(&error), error.message().to_owned())
                }
            };
            coded(codec.as_ref(), status, &message)
        }
    }
}

fn reject(
    expected: &str,
    headers: &HeaderMap,
    uri: &Uri,
    codec: Option<&dyn Codec>,
) -> Option<Response> {
    if authorized(expected, headers, uri) {
        return None;
    }
    let body = match codec {
        Some(codec) => codec.encode_error(StatusCode::UNAUTHORIZED.as_u16(), "unauthorized"),
        None => list_error("unauthorized"),
    };
    Some(json_response(StatusCode::UNAUTHORIZED, body))
}

fn current_token(app: &Inner) -> String {
    app.tokens.current()
}

/// Accepts the bearer token, `x-api-key`, `x-goog-api-key`, or a `key` query value.
///
/// Each presented secret is compared in constant time. One match is enough.
/// A request with no secret is still rejected.
fn authorized(expected: &str, headers: &HeaderMap, uri: &Uri) -> bool {
    let mut saw = false;
    let mut matched = false;
    if let Some(token) = bearer(headers) {
        saw = true;
        matched |= tokens_match(expected, token);
    }
    if let Some(token) = header_secret(headers, "x-api-key") {
        saw = true;
        matched |= tokens_match(expected, token);
    }
    if let Some(token) = header_secret(headers, "x-goog-api-key") {
        saw = true;
        matched |= tokens_match(expected, token);
    }
    if let Some(query) = uri.query()
        && let Some(token) = query_key(query)
    {
        saw = true;
        matched |= tokens_match(expected, &token);
    }
    saw && matched
}

/// Takes the gateway's own token out of `parts` before a surface sees them.
///
/// A surface may relay the request as it came, so each presented secret that
/// matches the token goes: the bearer, `x-api-key`, `x-goog-api-key`, and any
/// `key` query pair. A credential that does not match is the caller's own
/// and stays, in its place.
fn without_own_token(expected: &str, parts: &mut axum::http::request::Parts) {
    if bearer(&parts.headers).is_some_and(|token| tokens_match(expected, token)) {
        parts.headers.remove(header::AUTHORIZATION);
    }
    for name in ["x-api-key", "x-goog-api-key"] {
        if header_secret(&parts.headers, name).is_some_and(|token| tokens_match(expected, token)) {
            parts.headers.remove(name);
        }
    }
    let Some(query) = parts.uri.query() else {
        return;
    };
    let kept: Vec<&str> = query
        .split('&')
        .filter(|pair| !is_own_key(expected, pair))
        .collect();
    if kept.len() == query.split('&').count() {
        return;
    }
    let path = parts.uri.path();
    let target = if kept.is_empty() {
        path.to_owned()
    } else {
        format!("{path}?{}", kept.join("&"))
    };
    // What is left of a valid query is a valid query.
    if let Ok(uri) = target.parse() {
        parts.uri = uri;
    }
}

/// Whether the query pair `pair` is `key=` with the gateway's token.
fn is_own_key(expected: &str, pair: &str) -> bool {
    pair.split_once('=').is_some_and(|(name, value)| {
        name == "key" && tokens_match(expected, &percent_decode(value))
    })
}

fn header_secret<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let value = headers.get(name)?.to_str().ok()?;
    let token = value.trim();
    if token.is_empty() { None } else { Some(token) }
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    value.trim().strip_prefix("Bearer ")
}

fn tokens_match(expected: &str, presented: &str) -> bool {
    let expected = expected.as_bytes();
    let presented = presented.as_bytes();
    let mut diff = expected.len() ^ presented.len();
    let width = expected.len().max(presented.len());
    for index in 0..width {
        let left = expected.get(index).copied().unwrap_or(0);
        let right = presented.get(index).copied().unwrap_or(0);
        diff |= usize::from(left ^ right);
    }
    diff == 0
}

#[allow(clippy::too_many_arguments)]
async fn success(
    app: Arc<Inner>,
    codec: Arc<dyn Codec>,
    request: &ModelRequest,
    upstream: &str,
    upstream_model: &str,
    completion: Completion,
    started: Instant,
    track: &mut RequestTrack,
    capture: Option<prev_response::Capture>,
) -> Response {
    let meta = UsageMeta::new(request, upstream, started);
    let member = Some(upstream.to_owned());
    let capture = prev_response::ReplyCapture::new(capture);
    match completion {
        Completion::Unary(response) => {
            let input = response
                .usage
                .as_ref()
                .and_then(|usage| as_i64(usage.input_tokens));
            let output = response
                .usage
                .as_ref()
                .and_then(|usage| as_i64(usage.output_tokens));
            write_usage(&app.usage, &meta, Some((input, output)), StatusCode::OK);
            let status = StatusCode::OK;
            let encoded = codec.encode_response_for(request, &response);
            let mut capture = capture;
            let (status, body) = match encoded {
                Ok(bytes) => {
                    if let Some(capture) = capture.as_mut() {
                        capture.push(&bytes);
                    }
                    if let Some(capture) = capture.take() {
                        capture.finish();
                    }
                    (status, json_response(status, bytes))
                }
                Err(error) => {
                    let status = StatusCode::BAD_GATEWAY;
                    (status, coded(codec.as_ref(), status, &error.to_string()))
                }
            };
            track.end_with(
                member,
                status.as_u16(),
                input.and_then(|value| u64::try_from(value).ok()),
                output.and_then(|value| u64::try_from(value).ok()),
            );
            body
        }
        Completion::Stream(events) => {
            let encoder = codec.stream_encoder_for(request);
            let on_error = {
                let codec = Arc::clone(&codec);
                move |error: &PluginError| {
                    codec.encode_stream_error(status_of(error).as_u16(), error.message())
                }
            };
            // Move the track into the stream so finish runs when the body ends.
            let body = sse_body(
                events,
                encoder,
                RecordOnEnd {
                    usage: Arc::clone(&app.usage),
                    meta,
                    input: None,
                    output: None,
                    track: Some(track.take_for_stream()),
                    member,
                },
                Box::new(on_error),
            );
            let body = capture_stream(body, capture);
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, codec.stream_content_type())
                .body(Body::from_stream(body))
                .unwrap_or_else(|_| text_response(StatusCode::INTERNAL_SERVER_ERROR, "stream"))
        }
        Completion::Raw(body) => {
            raw_body(
                app,
                codec,
                request,
                upstream_model,
                meta,
                body,
                track,
                member,
                capture,
            )
            .await
        }
        _ => {
            track.end_with(member, StatusCode::BAD_GATEWAY.as_u16(), None, None);
            text_response(StatusCode::BAD_GATEWAY, "unsupported completion")
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn raw_body(
    app: Arc<Inner>,
    codec: Arc<dyn Codec>,
    request: &ModelRequest,
    upstream_model: &str,
    mut meta: UsageMeta,
    mut body: BodyStream,
    track: &mut RequestTrack,
    member: Option<String>,
    capture: Option<prev_response::ReplyCapture>,
) -> Response {
    let content_type = codec.response_content_type(request.stream);
    if request.stream {
        let usage = Arc::clone(&app.usage);
        let tap = codec.raw_tap(&request.model, upstream_model);
        let on_error = {
            let codec = Arc::clone(&codec);
            move |error: &PluginError| {
                codec.encode_stream_error(status_of(error).as_u16(), error.message())
            }
        };
        let owned = track.take_for_stream();
        let stream = futures_util::stream::unfold(
            (
                body,
                meta,
                usage,
                tap,
                on_error,
                true,
                0u8,
                Some(owned),
                member,
            ),
            |(mut body, mut meta, usage, mut tap, on_error, boundary, phase, mut track, member)| async move {
                let finish = |meta: &UsageMeta,
                              usage: &Ledger,
                              track: &mut Option<RequestTrack>,
                              member: &Option<String>,
                              status: StatusCode| {
                    write_usage(usage, meta, Some((meta.input, meta.output)), status);
                    if let Some(track) = track.as_mut() {
                        track.end_with(
                            member.clone(),
                            status.as_u16(),
                            meta.input.and_then(|value| u64::try_from(value).ok()),
                            meta.output.and_then(|value| u64::try_from(value).ok()),
                        );
                    }
                };
                if phase == 1 {
                    finish(&meta, &usage, &mut track, &member, StatusCode::OK);
                    return None;
                }
                if phase == 2 {
                    return None;
                }
                match body.next_chunk().await {
                    None => {
                        let tail = tap.finish();
                        let (input, output) = tap.tokens();
                        meta.input = input.or(meta.input);
                        meta.output = output.or(meta.output);
                        if tail.is_empty() {
                            finish(&meta, &usage, &mut track, &member, StatusCode::OK);
                            None
                        } else {
                            Some((
                                Ok::<Bytes, std::io::Error>(Bytes::from(tail)),
                                (body, meta, usage, tap, on_error, boundary, 1, track, member),
                            ))
                        }
                    }
                    Some(Err(error)) => {
                        let mut tail = tap.finish();
                        let (input, output) = tap.tokens();
                        meta.input = input.or(meta.input);
                        meta.output = output.or(meta.output);
                        finish(&meta, &usage, &mut track, &member, StatusCode::BAD_GATEWAY);
                        let mut frame = on_error(&error);
                        if tail.is_empty() && !boundary {
                            let mut prefixed = b"\n\n".to_vec();
                            prefixed.extend(frame);
                            frame = prefixed;
                        }
                        tail.extend(frame);
                        Some((
                            Ok(Bytes::from(tail)),
                            (body, meta, usage, tap, on_error, true, 2, track, member),
                        ))
                    }
                    Some(Ok(chunk)) => {
                        let forwarded = tap.push(&chunk);
                        let (input, output) = tap.tokens();
                        meta.input = input.or(meta.input);
                        meta.output = output.or(meta.output);
                        let boundary = if forwarded.is_empty() {
                            boundary
                        } else {
                            frame_closed(&forwarded)
                        };
                        Some((
                            Ok(Bytes::from(forwarded)),
                            (body, meta, usage, tap, on_error, boundary, 0, track, member),
                        ))
                    }
                }
            },
        );
        let stream = capture_stream(stream, capture);
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, content_type)
            .body(Body::from_stream(stream))
            .unwrap_or_else(|_| text_response(StatusCode::INTERNAL_SERVER_ERROR, "stream"));
    }
    let mut bytes = Vec::new();
    let mut failed: Option<PluginError> = None;
    while let Some(chunk) = body.next_chunk().await {
        match chunk {
            Ok(chunk) => bytes.extend_from_slice(&chunk),
            Err(error) => {
                failed = Some(error);
                break;
            }
        }
    }
    if let Some(error) = failed {
        write_usage(&app.usage, &meta, None, StatusCode::BAD_GATEWAY);
        track.end_with(member, StatusCode::BAD_GATEWAY.as_u16(), None, None);
        return coded(codec.as_ref(), StatusCode::BAD_GATEWAY, error.message());
    }
    let (input, output) = codec.response_tokens(&bytes);
    meta.input = input;
    meta.output = output;
    write_usage(&app.usage, &meta, Some((input, output)), StatusCode::OK);
    track.end_with(
        member,
        StatusCode::OK.as_u16(),
        input.and_then(|value| u64::try_from(value).ok()),
        output.and_then(|value| u64::try_from(value).ok()),
    );
    let body = codec.rewrite_model(&bytes, &request.model, upstream_model);
    if let Some(mut capture) = capture {
        capture.push(&body);
        capture.finish();
    }
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, codec.response_content_type(false))
        .body(Body::from(body))
        .unwrap_or_else(|_| text_response(StatusCode::INTERNAL_SERVER_ERROR, "response"))
}

fn failure(
    app: Arc<Inner>,
    codec: &dyn Codec,
    request: &ModelRequest,
    error: RouterError,
    started: Instant,
    track: &mut RequestTrack,
) -> Response {
    if let RouterError::PreviousResponseNotFound { id } = &error {
        write_usage(
            &app.usage,
            &UsageMeta::new(request, "", started),
            None,
            StatusCode::BAD_REQUEST,
        );
        track.end_with(None, StatusCode::BAD_REQUEST.as_u16(), None, None);
        return json_response(StatusCode::BAD_REQUEST, prev_response::not_found_body(id));
    }
    let (status, upstream, message) = match error {
        RouterError::NoRoute { .. } => (StatusCode::NOT_FOUND, String::new(), error.to_string()),
        RouterError::PreviousResponseNotFound { .. } => unreachable!(),
        RouterError::Ambiguous { .. } => {
            (StatusCode::BAD_REQUEST, String::new(), error.to_string())
        }
        RouterError::Upstream { upstream_id, error } => {
            (status_of(&error), upstream_id, error.message().to_owned())
        }
    };
    write_usage(
        &app.usage,
        &UsageMeta::new(request, &upstream, started),
        None,
        status,
    );
    let member = if upstream.is_empty() {
        None
    } else {
        Some(upstream)
    };
    track.end_with(member, status.as_u16(), None, None);
    coded(codec, status, &message)
}

fn status_of(error: &PluginError) -> StatusCode {
    let code = match error.kind() {
        PluginErrorKind::Status => error.status().unwrap_or(502),
        _ => 502,
    };
    StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_GATEWAY)
}

struct UsageMeta {
    model: String,
    upstream: String,
    started: Instant,
    input: Option<i64>,
    output: Option<i64>,
}

impl UsageMeta {
    fn new(request: &ModelRequest, upstream: &str, started: Instant) -> Self {
        Self {
            model: request.model.clone(),
            upstream: upstream.to_owned(),
            started,
            input: None,
            output: None,
        }
    }
}

struct RecordOnEnd {
    usage: Arc<Ledger>,
    meta: UsageMeta,
    input: Option<i64>,
    output: Option<i64>,
    track: Option<RequestTrack>,
    member: Option<String>,
}

type ErrorFrame = Box<dyn Fn(&PluginError) -> Vec<u8> + Send>;

fn capture_stream<S>(
    incoming: S,
    capture: Option<prev_response::ReplyCapture>,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send
where
    S: Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
{
    futures_util::stream::unfold(
        (Box::pin(incoming), capture, true),
        |(mut incoming, mut capture, mut normal)| async move {
            match incoming.next().await {
                Some(Ok(chunk)) => {
                    if let Some(capture) = capture.as_mut() {
                        capture.push(&chunk);
                    }
                    Some((Ok(chunk), (incoming, capture, normal)))
                }
                Some(Err(error)) => {
                    normal = false;
                    Some((Err(error), (incoming, capture, normal)))
                }
                None => {
                    if normal && let Some(capture) = capture.take() {
                        capture.finish();
                    }
                    None
                }
            }
        },
    )
}

fn sse_body(
    incoming: Pin<Box<dyn Stream<Item = Result<StreamEvent, PluginError>> + Send>>,
    encoder: Box<dyn StreamEncoder>,
    record: RecordOnEnd,
    on_error: ErrorFrame,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send {
    futures_util::stream::unfold(
        (incoming, encoder, record, on_error, true, 0u8),
        |(mut incoming, mut encoder, mut record, on_error, boundary, phase)| async move {
            if phase == 1 {
                finish_record(&mut record, StatusCode::OK);
                return None;
            }
            if phase == 2 {
                return None;
            }
            loop {
                match incoming.next().await {
                    None => match encoder.finish() {
                        Ok(bytes) if bytes.is_empty() => {
                            finish_record(&mut record, StatusCode::OK);
                            return None;
                        }
                        Ok(bytes) => {
                            let boundary = frame_closed(&bytes);
                            return Some((
                                Ok(Bytes::from(bytes)),
                                (incoming, encoder, record, on_error, boundary, 1),
                            ));
                        }
                        Err(_) => {
                            finish_record(&mut record, StatusCode::BAD_GATEWAY);
                            return None;
                        }
                    },
                    Some(Err(error)) => {
                        finish_record(&mut record, StatusCode::BAD_GATEWAY);
                        let mut bytes = on_error(&error);
                        if !boundary {
                            let mut prefixed = b"\n\n".to_vec();
                            prefixed.extend_from_slice(&bytes);
                            bytes = prefixed;
                        }
                        return Some((
                            Ok(Bytes::from(bytes)),
                            (incoming, encoder, record, on_error, true, 2),
                        ));
                    }
                    Some(Ok(event)) => {
                        note_usage(&event, &mut record.input, &mut record.output);
                        match encoder.push(&event) {
                            Ok(bytes) if bytes.is_empty() => {}
                            Ok(bytes) => {
                                let boundary = frame_closed(&bytes);
                                return Some((
                                    Ok(Bytes::from(bytes)),
                                    (incoming, encoder, record, on_error, boundary, 0),
                                ));
                            }
                            Err(_) => {
                                finish_record(&mut record, StatusCode::BAD_GATEWAY);
                                return None;
                            }
                        }
                    }
                }
            }
        },
    )
}

fn frame_closed(bytes: &[u8]) -> bool {
    bytes.ends_with(b"\n\n") || bytes.ends_with(b"\r\n\r\n") || bytes.ends_with(b"\r\r")
}

fn note_usage(event: &StreamEvent, input: &mut Option<i64>, output: &mut Option<i64>) {
    let usage = match event {
        StreamEvent::MessageStart(start) => start.usage.as_ref(),
        StreamEvent::MessageDelta(delta) => delta.usage.as_ref(),
        _ => None,
    };
    let Some(usage) = usage else {
        return;
    };
    if let Some(value) = as_i64(usage.input_tokens) {
        *input = Some(value);
    }
    if let Some(value) = as_i64(usage.output_tokens) {
        *output = Some(value);
    }
}

fn finish_record(row: &mut RecordOnEnd, status: StatusCode) {
    write_usage(&row.usage, &row.meta, Some((row.input, row.output)), status);
    if let Some(track) = row.track.as_mut() {
        track.end_with(
            row.member.clone(),
            status.as_u16(),
            row.input.and_then(|value| u64::try_from(value).ok()),
            row.output.and_then(|value| u64::try_from(value).ok()),
        );
    }
}

fn write_usage(
    usage: &Ledger,
    meta: &UsageMeta,
    tokens: Option<(Option<i64>, Option<i64>)>,
    status: StatusCode,
) {
    let (input_tokens, output_tokens) = tokens.unwrap_or((None, None));
    if let Some(tokens) = input_tokens.and_then(|value| u64::try_from(value).ok()) {
        metrics::counter!("hofund_tokens", "direction" => "input").increment(tokens);
    }
    if let Some(tokens) = output_tokens.and_then(|value| u64::try_from(value).ok()) {
        metrics::counter!("hofund_tokens", "direction" => "output").increment(tokens);
    }
    metrics::histogram!("hofund_member_latency_ms")
        .record(meta.started.elapsed().as_millis() as f64);
    let row = Row {
        created_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| {
                i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
            }),
        model: meta.model.clone(),
        upstream: meta.upstream.clone(),
        input_tokens,
        output_tokens,
        latency_ms: i64::try_from(meta.started.elapsed().as_millis()).unwrap_or(i64::MAX),
        status: status.as_u16().to_string(),
    };
    let _ = usage.record(&row);
}

fn as_i64(value: Option<u64>) -> Option<i64> {
    value.and_then(|value| i64::try_from(value).ok())
}

fn model_name(bytes: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .and_then(|value| {
            value
                .get("model")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default()
}

fn coded(codec: &dyn Codec, status: StatusCode, message: &str) -> Response {
    json_response(status, codec.encode_error(status.as_u16(), message))
}

fn list_error(message: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "error": {"message": message, "type": "authentication_error", "param": null, "code": null}
    }))
    .unwrap_or_else(|_| b"{\"error\":{\"message\":\"unauthorized\"}}".to_vec())
}

fn json_response(status: StatusCode, bytes: Vec<u8>) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(bytes))
        .unwrap_or_else(|_| text_response(StatusCode::INTERNAL_SERVER_ERROR, "response"))
}

fn text_response(status: StatusCode, message: &str) -> Response {
    Response::builder()
        .status(status)
        .body(Body::from(message.to_owned()))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

#[cfg(test)]
mod request_event_tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use super::{RequestEnd, RequestEvents, RequestStart, RequestTrack};

    struct Spy {
        starts: Mutex<Vec<RequestStart>>,
        ends: Mutex<Vec<RequestEnd>>,
    }

    impl RequestEvents for Spy {
        fn started(&self, start: &RequestStart) {
            self.starts.lock().expect("lock").push(start.clone());
        }

        fn finished(&self, end: &RequestEnd) {
            self.ends.lock().expect("lock").push(end.clone());
        }
    }

    #[test]
    fn in_flight_counts_and_returns_to_zero() {
        let counter = Arc::new(AtomicU64::new(0));
        let spy = Arc::new(Spy {
            starts: Mutex::new(Vec::new()),
            ends: Mutex::new(Vec::new()),
        });
        let mut track = RequestTrack {
            events: Some(spy.clone() as Arc<dyn RequestEvents>),
            in_flight: Arc::clone(&counter),
            request_id: "r1".into(),
            model: "demo".into(),
            started: std::time::Instant::now(),
            active: true,
            finished: false,
            drop_status: 500,
            cancel_usage: None,
        };
        // Mirror begin's increment (begin also calls started).
        counter.fetch_add(1, Ordering::SeqCst);
        spy.started(&RequestStart {
            request_id: "r1".into(),
            route: "demo".into(),
            inbound_protocol: "echo-chat".into(),
            started_at_ms: 1,
        });
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        track.end_with(None, 200, Some(1), Some(2));
        assert_eq!(counter.load(Ordering::SeqCst), 0);
        assert_eq!(spy.starts.lock().expect("lock").len(), 1);
        let ends = spy.ends.lock().expect("lock");
        assert_eq!(ends.len(), 1);
        assert_eq!(ends[0].status, 200);
        assert_eq!(ends[0].input_tokens, Some(1));
        // Drop after finish must not double-decrement.
        drop(track);
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn request_track_drop_decrements_without_finish() {
        let counter = Arc::new(AtomicU64::new(0));
        let spy = Arc::new(Spy {
            starts: Mutex::new(Vec::new()),
            ends: Mutex::new(Vec::new()),
        });
        {
            let track = RequestTrack {
                events: Some(spy.clone() as Arc<dyn RequestEvents>),
                in_flight: Arc::clone(&counter),
                request_id: "r-drop".into(),
                model: "demo".into(),
                started: std::time::Instant::now(),
                active: true,
                finished: false,
                drop_status: 500,
                cancel_usage: None,
            };
            counter.fetch_add(1, Ordering::SeqCst);
            assert_eq!(counter.load(Ordering::SeqCst), 1);
            drop(track);
        }
        assert_eq!(counter.load(Ordering::SeqCst), 0);
        let ends = spy.ends.lock().expect("lock");
        assert_eq!(ends.len(), 1);
        assert_eq!(ends[0].status, 500);
    }
}

#[cfg(test)]
mod forwarded_token_tests {
    use super::forwarded_tokens;

    #[test]
    fn tokens_are_read_from_the_first_field_that_has_a_positive_count() {
        let read = |text: &str| forwarded_tokens(text.as_bytes());
        assert_eq!(
            read(r#"{"usage":{"prompt_tokens":3,"input_tokens":4,"total_tokens":5}}"#),
            (Some(3), None)
        );
        assert_eq!(
            read(r#"{"usage":{"prompt_tokens":0,"input_tokens":4,"output_tokens":9}}"#),
            (Some(4), Some(9))
        );
        assert_eq!(
            read(r#"{"usage":{"input_tokens":0,"total_tokens":5}}"#),
            (Some(5), None)
        );
        assert_eq!(
            read(r#"{"usage":{"total_tokens":0},"meta":{"billed_units":{"input_tokens":6}}}"#),
            (Some(6), None)
        );
        assert_eq!(read(r#"{"usage":{"output_tokens":0}}"#), (None, None));
        assert_eq!(read(r#"{"usage":{"prompt_tokens":-1}}"#), (None, None));
        assert_eq!(read("not json"), (None, None));
        assert_eq!(read("{}"), (None, None));
    }
}
