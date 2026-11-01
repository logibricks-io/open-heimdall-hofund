//! HTTP edge of the local gateway.
//!
//! The process authenticates a request, decodes it with the codec it was
//! given for that path, asks the router, and writes the encoded body. Vendor
//! names do not belong in this crate. Generative Language paths are matched
//! in `endpoints`.

pub mod control;
mod endpoints;
mod issue_keys;
mod limits;
mod me;
mod prev_response;
mod quotas;
mod readonly;
mod refusal_handover;
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
use chrono::Local;
use endpoints::{
    GENERATIVE_CATCHALL, inject_target, match_count, match_generate, percent_decode, query_key,
};
use futures_util::{Stream, StreamExt, TryStreamExt};
use hofund_codec::{
    Codec, EndWatch, SESSION_HEADERS, SUBAGENT_HEADER, StreamEncoder, compacts_in_stream_client,
    conversation_key, turn_in,
};
use hofund_ir::{Request as ModelRequest, StreamEvent, too_long, words_of};
use hofund_plugin_api::{
    BodyStream, InboundSurface, Method, PluginError, PluginErrorKind, ProtocolId,
};
use hofund_plugin_host::HttpBackend;
use hofund_router::{Completion, RouteScope, Routed, RouterError, Snapshot, WaitHook};
use hofund_secrets::{Identity, KeyStore};
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
    keys: Option<Arc<KeyStore>>,
    codecs: HashMap<String, Arc<dyn Codec>>,
    usage: Arc<Ledger>,
    budget: limits::Budget,
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
    reservation: Option<limits::Reservation>,
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
            reservation: None,
        }
    }

    fn with_reservation(mut self, reservation: Option<limits::Reservation>) -> Self {
        self.reservation = reservation;
        self
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

    fn client_waiting(&mut self, usage: Arc<Ledger>, request: &ModelRequest, identity: &Identity) {
        self.drop_status = 499;
        self.cancel_usage = Some((usage, UsageMeta::new(request, "", self.started, identity)));
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
            reservation: self.reservation.take(),
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
                keys: None,
                codecs,
                budget: limits::Budget::new(Arc::clone(&usage)),
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

    /// Uses the named-key store for authentication and caller scopes.
    #[must_use]
    pub fn with_key_store(mut self, keys: Arc<KeyStore>) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("gateway is not shared yet")
            .keys = Some(keys);
        self
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
        let prune = self.inner.keys.clone().map(issue_keys::prune_task);
        let mut running = serve_relistenable(self.into_router(), listen, sockets).await?;
        if let Some(task) = prune {
            running = running.with_task(task);
        }
        Ok(running)
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
        axum::serve(
            accept,
            router.into_make_service_with_connect_info::<issue_keys::PeerAddr>(),
        )
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
    request_routes(app)
        .router
        .with_state(Arc::clone(app))
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(app),
            responses_ws::path_guard,
        ))
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(app),
            readonly::guard,
        ))
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(app),
            issue_keys::refused_guard,
        ))
}

fn request_routes(app: &Arc<Inner>) -> readonly::RouteSet {
    let mut routes = issue_keys::routes(me::routes(codec_routes(app)));
    if app.quota_source.is_some() {
        routes = quotas::routes(routes);
    }
    if !app.surfaces.is_empty() {
        routes.patterns.push("*".to_owned());
        routes.router = routes.router.fallback(dispatch_surface);
    }
    routes
}

/// `/health`, the model lists and every codec route. Surfaces are not here,
/// so the in-process gateway a surface calls cannot reach another surface.
fn codec_routes(inner: &Inner) -> readonly::RouteSet {
    let mut routes = readonly::RouteSet::new()
        .route(&["GET"], "/health", get(health))
        .route(&["GET"], "/v1/models", get(models))
        .route(&["GET"], "/v1/hofund/limit", get(limits::limit_route));
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
        routes = routes.route(&["POST"], &path, post(complete));
    }
    if responses {
        routes = routes
            .route(
                &["GET", "POST"],
                "/v1/responses",
                post(complete).get(responses_ws::get),
            )
            .route(
                &["GET", "POST"],
                "/responses",
                post(complete).get(responses_ws::get),
            );
    }
    if messages {
        routes = routes.route(&["POST"], "/v1/messages/count_tokens", post(complete));
    }
    if generative {
        routes = routes
            .route(&["POST"], GENERATIVE_CATCHALL, post(complete))
            .route(&["GET"], GENERATIVE_LIST, get(generative_models));
    }
    routes.patterns.push("*".to_owned());
    routes.router = routes.router.fallback(not_found);
    routes
}

/// The gateway's codec routes, run in process for an inbound surface.
///
/// The request presents the current gateway token as its bearer and inherits
/// the authenticated caller's identity. Any other credential the surface sets
/// is ignored, so it can neither help nor hurt.
struct InProcess {
    app: std::sync::Weak<Inner>,
    identity: Identity,
    body_bytes: usize,
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
        parts.extensions.insert(self.identity.clone());
        let request = Request::from_parts(parts, Body::from(body.bytes().clone()));
        let router = codec_routes(&app).router.with_state(Arc::clone(&app));
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
        track.reservation = match app
            .budget
            .reserve(&self.identity, self.body_bytes, Local::now())
        {
            Ok(reservation) => reservation,
            Err(limits::ReserveFailure::Refused(refusal)) => {
                let status = StatusCode::TOO_MANY_REQUESTS;
                write_usage(
                    &app.usage,
                    &UsageMeta::new(
                        &ModelRequest::new(call.model()),
                        "",
                        started,
                        &self.identity,
                    ),
                    None,
                    status,
                );
                track.end_with(None, status.as_u16(), None, None);
                return Err(PluginError::status_error(
                    status.as_u16(),
                    refusal.message(),
                ));
            }
            Err(limits::ReserveFailure::Unavailable) => {
                let status = StatusCode::SERVICE_UNAVAILABLE;
                write_usage(
                    &app.usage,
                    &UsageMeta::new(
                        &ModelRequest::new(call.model()),
                        "",
                        started,
                        &self.identity,
                    ),
                    None,
                    status,
                );
                track.end_with(None, status.as_u16(), None, None);
                return Err(PluginError::status_error(
                    status.as_u16(),
                    "gateway key limits are temporarily unavailable",
                ));
            }
        };
        let router = app.snapshot.load();
        let usage = |upstream: &str| UsageMeta {
            model: call.model().to_owned(),
            upstream: upstream.to_owned(),
            started,
            identity: self.identity.clone(),
            input: None,
            output: None,
            ttft_ms: None,
            details: hofund_usage::UsageDetails::default(),
        };
        match router
            .forward_scoped(&call, Some(&route_scope(&self.identity)))
            .await
        {
            Ok(forwarded) => {
                let answer = forwarded.answer;
                let status =
                    StatusCode::from_u16(answer.status()).unwrap_or(StatusCode::BAD_GATEWAY);
                let (input, output) = forwarded_tokens(answer.body());
                let mut meta = usage(&forwarded.upstream_id);
                meta.details = forwarded_details(answer.body(), input);
                write_usage(&app.usage, &meta, Some((input, output)), status);
                track.end_with(
                    Some(forwarded.upstream_id),
                    status.as_u16(),
                    input.and_then(|value| u64::try_from(value).ok()),
                    output.and_then(|value| u64::try_from(value).ok()),
                );
                Ok(answer)
            }
            Err(error) => {
                let scope_message = scope_error(&error, &self.identity);
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
                    RouterError::ModelScope { .. } | RouterError::AccountScope { .. } => {
                        (String::new(), PluginError::status_error(403, scope_message))
                    }
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

fn forwarded_details(body: &[u8], input: Option<i64>) -> hofund_usage::UsageDetails {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return hofund_usage::UsageDetails {
            uncached_input_tokens: input,
            ..hofund_usage::UsageDetails::default()
        };
    };
    let count = |pointer: &str| {
        value
            .pointer(pointer)
            .and_then(serde_json::Value::as_i64)
            .filter(|tokens| *tokens >= 0)
    };
    let cache_read = count("/usage/prompt_tokens_details/cached_tokens")
        .or_else(|| count("/usage/input_tokens_details/cached_tokens"))
        .or_else(|| count("/usage/cache_read_input_tokens"))
        .or_else(|| count("/usage/cachedContentTokenCount"));
    let cache_write = count("/usage/cache_creation_input_tokens")
        .or_else(|| count("/usage/input_tokens_details/cache_write_tokens"));
    let includes_cache = value.pointer("/usage/prompt_tokens").is_some()
        || value.pointer("/usage/input_tokens_details").is_some()
        || value.pointer("/usage/cachedContentTokenCount").is_some();
    hofund_usage::UsageDetails {
        uncached_input_tokens: input.map(|input| {
            if includes_cache {
                input.saturating_sub(cache_read.unwrap_or(0))
            } else {
                input
            }
        }),
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
    }
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
    let identity = match authenticate_request(&app, &headers, &uri, None, None) {
        Ok(identity) => identity,
        Err(response) => return *response,
    };
    let Some(route) =
        surface.routes().iter().copied().find(|route| {
            method_is(route.method(), &method) && path_matches(route.prefix(), &path)
        })
    else {
        return text_response(StatusCode::NOT_FOUND, "not found");
    };
    let (mut parts, body) = request.into_parts();
    without_own_token(&app, &mut parts);
    let bytes = match axum::body::to_bytes(body, SURFACE_BODY_LIMIT).await {
        Ok(bytes) => bytes,
        Err(_) => return text_response(StatusCode::PAYLOAD_TOO_LARGE, "request body is too large"),
    };
    let body_bytes = bytes.len();
    let inbound =
        axum::http::Request::from_parts(parts, hofund_plugin_api::Body::from_bytes(bytes));
    let gateway = Arc::new(InProcess {
        app: Arc::downgrade(&app),
        identity,
        body_bytes,
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
    let inherited = request.extensions().get::<Identity>().cloned();
    let identity =
        match authenticate_request(&app, request.headers(), request.uri(), inherited, None) {
            Ok(identity) => identity,
            Err(response) => return *response,
        };
    let data: Vec<_> = app
        .snapshot
        .load()
        .models_scoped(Some(&route_scope(&identity)))
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
    let inherited = request.extensions().get::<Identity>().cloned();
    let identity =
        match authenticate_request(&app, request.headers(), request.uri(), inherited, codec) {
            Ok(identity) => identity,
            Err(response) => return *response,
        };
    let models: Vec<_> = app
        .snapshot
        .load()
        .models_scoped(Some(&route_scope(&identity)))
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

fn reserve_for_codec(
    app: &Inner,
    codec: &dyn Codec,
    identity: &Identity,
    request: &ModelRequest,
    body_bytes: usize,
    started: Instant,
) -> Result<Option<limits::Reservation>, Box<Response>> {
    let now = Local::now();
    match app.budget.reserve(identity, body_bytes, now) {
        Ok(reservation) => Ok(reservation),
        Err(limits::ReserveFailure::Refused(refusal)) => {
            write_usage(
                &app.usage,
                &UsageMeta::new(request, "", started, identity),
                None,
                StatusCode::TOO_MANY_REQUESTS,
            );
            Err(Box::new(limits::refusal_response(codec, &refusal, now)))
        }
        Err(limits::ReserveFailure::Unavailable) => {
            write_usage(
                &app.usage,
                &UsageMeta::new(request, "", started, identity),
                None,
                StatusCode::SERVICE_UNAVAILABLE,
            );
            Err(Box::new(coded(
                codec,
                StatusCode::SERVICE_UNAVAILABLE,
                "gateway key limits are temporarily unavailable",
            )))
        }
    }
}

async fn complete(State(app): State<Arc<Inner>>, request: Request) -> Response {
    let path = request.uri().path().to_owned();
    let uri = request.uri().clone();
    let headers = request.headers().clone();
    let inherited = request.extensions().get::<Identity>().cloned();
    if path == "/v1/messages/count_tokens" {
        return count_tokens(app, headers, uri, request, inherited, "/v1/messages", None).await;
    }
    if let Some(model) = match_count(&path) {
        return count_tokens(
            app,
            headers,
            uri,
            request,
            inherited,
            GENERATIVE_UNARY,
            Some(model),
        )
        .await;
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
    let identity = match authenticate_request(&app, &headers, &uri, inherited, Some(codec.as_ref()))
    {
        Ok(identity) => identity,
        Err(response) => return *response,
    };
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
                &UsageMeta::new(
                    &ModelRequest::new(model_name(&bytes)),
                    "",
                    started,
                    &identity,
                ),
                None,
                StatusCode::BAD_REQUEST,
            );
            return coded(codec.as_ref(), StatusCode::BAD_REQUEST, &error.to_string());
        }
    };
    let reservation = match reserve_for_codec(
        &app,
        codec.as_ref(),
        &identity,
        &model_request,
        bytes.len(),
        started,
    ) {
        Ok(reservation) => reservation,
        Err(response) => return *response,
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
    )
    .with_reservation(reservation);
    let router = app.snapshot.load();
    let attempt = ask.as_ref().map(|ask| ask.attempt());
    let mut identity_headers = Vec::new();
    for name in SESSION_HEADERS.iter().copied().chain([SUBAGENT_HEADER]) {
        if let Some(value) = headers.get(name).and_then(|value| value.to_str().ok()) {
            identity_headers.push((name, value));
        }
    }
    let turn = turn_in(&model_request);
    let conversation = hofund_router::affinity::Conversation::new(
        conversation_key(&model_request.model, &identity_headers, &bytes),
        turn.number,
        turn.within,
    )
    .with_body(bytes.to_vec());
    let (wait, notice) = GatewayWait::channel();
    let mut dispatch: DispatchFuture = Box::pin({
        let request = model_request.clone();
        let protocol = codec.id().to_owned();
        let scope = route_scope(&identity);
        async move {
            router
                .dispatch_scoped_conversation_with_wait(
                    &request,
                    &protocol,
                    Some(&scope),
                    Some(&conversation),
                    attempt,
                    Some(wait),
                )
                .await
        }
    });
    let result = tokio::select! {
        result = &mut dispatch => result,
        _ = notice => {
            track.client_waiting(Arc::clone(&app.usage), &model_request, &identity);
            if model_request.stream {
                return held_response(
                    app,
                    codec,
                    model_request,
                    started,
                    identity,
                    track,
                    capture,
                    dispatch,
                );
            }
            dispatch.await
        }
    };
    match result {
        Ok(mut routed) => {
            let continuation = held_continuation(codec.as_ref(), &model_request, &mut routed);
            success(
                app,
                codec,
                &model_request,
                &routed.upstream_id,
                &routed.upstream_model,
                routed.completion,
                continuation,
                started,
                &identity,
                &mut track,
                capture,
            )
            .await
        }
        Err(error) => failure(
            app,
            codec.as_ref(),
            &headers,
            &model_request,
            error,
            started,
            &identity,
            &mut track,
        ),
    }
}

/// The untried remainder of the route when the stream can still hand over.
fn held_continuation(
    codec: &dyn Codec,
    request: &ModelRequest,
    routed: &mut Routed,
) -> Option<hofund_router::RouteContinuation> {
    (request.stream
        && matches!(
            codec.id(),
            id if id == ProtocolId::ANTHROPIC_MESSAGES.as_str()
                || id == ProtocolId::OPENAI_CHAT.as_str()
                || id == ProtocolId::OPENAI_RESPONSES.as_str()
        ))
    .then(|| routed.take_continuation())
    .flatten()
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
    identity: Identity,
    track: Option<RequestTrack>,
    capture: Option<prev_response::Capture>,
    incoming: Option<BytesStream>,
    keepalive: Pin<Box<tokio::time::Sleep>>,
}

// The caller identity rides along for key usage attribution.
#[allow(clippy::too_many_arguments)]
fn held_response(
    app: Arc<Inner>,
    codec: Arc<dyn Codec>,
    request: ModelRequest,
    started: Instant,
    identity: Identity,
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
        identity,
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
                        Ok(mut routed) => {
                            let continuation = held_continuation(
                                state.codec.as_ref(),
                                &state.request,
                                &mut routed,
                            );
                            let response = success(
                                Arc::clone(&state.app),
                                Arc::clone(&state.codec),
                                &state.request,
                                &routed.upstream_id,
                                &routed.upstream_model,
                                routed.completion,
                                continuation,
                                state.started,
                                &state.identity,
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
                            // Only the usage row and the track are kept: the
                            // reply already went out as a stream, so the error
                            // goes as a stream error frame whatever the client.
                            let _ = failure(
                                Arc::clone(&state.app),
                                state.codec.as_ref(),
                                &HeaderMap::new(),
                                &state.request,
                                error,
                                state.started,
                                &state.identity,
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
        RouterError::ModelScope { .. } | RouterError::AccountScope { .. } => {
            (StatusCode::FORBIDDEN, error.to_string())
        }
        RouterError::Upstream { error, .. } => (status_of(error), broken_off(error)),
    }
}

async fn count_tokens(
    app: Arc<Inner>,
    headers: HeaderMap,
    uri: Uri,
    request: Request,
    inherited: Option<Identity>,
    codec_key: &str,
    path_model: Option<String>,
) -> Response {
    let Some(codec) = app.codecs.get(codec_key).cloned() else {
        return text_response(StatusCode::NOT_FOUND, "not found");
    };
    let identity = match authenticate_request(&app, &headers, &uri, inherited, Some(codec.as_ref()))
    {
        Ok(identity) => identity,
        Err(response) => return *response,
    };
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
    match router
        .count_scoped(&model_request, codec.id(), Some(&route_scope(&identity)))
        .await
    {
        Ok(count) => json_response(
            StatusCode::OK,
            codec.encode_token_count(count.tokens, count.estimated),
        ),
        Err(error) => {
            let (status, message) = match &error {
                RouterError::NoRoute { .. } => (StatusCode::NOT_FOUND, error.to_string()),
                RouterError::Ambiguous { .. } | RouterError::PreviousResponseNotFound { .. } => {
                    (StatusCode::BAD_REQUEST, error.to_string())
                }
                RouterError::ModelScope { .. } | RouterError::AccountScope { .. } => {
                    (StatusCode::FORBIDDEN, scope_error(&error, &identity))
                }
                RouterError::Upstream { error, .. } => {
                    (status_of(error), error.message().to_owned())
                }
            };
            coded(codec.as_ref(), status, &message)
        }
    }
}

fn authenticate_request(
    app: &Inner,
    headers: &HeaderMap,
    uri: &Uri,
    inherited: Option<Identity>,
    codec: Option<&dyn Codec>,
) -> Result<Identity, Box<Response>> {
    if let Some(identity) = inherited {
        return Ok(identity);
    }
    let mut refused = None;
    if let Some(keys) = &app.keys {
        let query = uri.query().and_then(query_key);
        for secret in [
            bearer(headers),
            header_secret(headers, "x-api-key"),
            header_secret(headers, "x-goog-api-key"),
        ]
        .into_iter()
        .flatten()
        .chain(query.as_deref())
        {
            match keys.check(secret) {
                Ok(hofund_secrets::KeyAuthentication::Enabled(identity)) => return Ok(identity),
                Ok(hofund_secrets::KeyAuthentication::Refused(message)) => {
                    refused.get_or_insert(message);
                }
                Ok(hofund_secrets::KeyAuthentication::Unknown) | Err(_) => {}
            }
        }
    } else if authorized(&current_token(app), headers, uri) {
        return Ok(Identity::default());
    }
    let message = refused.as_deref().unwrap_or("unauthorized");
    let body = match codec {
        Some(codec) => codec.encode_error(StatusCode::UNAUTHORIZED.as_u16(), message),
        None => list_error(message),
    };
    Err(Box::new(json_response(StatusCode::UNAUTHORIZED, body)))
}

fn route_scope(identity: &Identity) -> RouteScope {
    RouteScope {
        models: identity.models.clone(),
        accounts: identity.accounts.clone(),
    }
}

fn scope_error(error: &RouterError, identity: &Identity) -> String {
    match error {
        RouterError::ModelScope { model, patterns } => format!(
            "gateway key {:?} is not allowed to use model `{model}`; allowed models: {}",
            identity.key_name,
            patterns.join(", ")
        ),
        RouterError::AccountScope { model, accounts } => format!(
            "gateway key {:?} is not allowed to use the accounts behind model `{model}`; allowed accounts: {}",
            identity.key_name,
            accounts.join(", ")
        ),
        other => other.to_string(),
    }
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
fn without_own_token(app: &Inner, parts: &mut axum::http::request::Parts) {
    if bearer(&parts.headers).is_some_and(|token| own_secret(app, token)) {
        parts.headers.remove(header::AUTHORIZATION);
    }
    for name in ["x-api-key", "x-goog-api-key"] {
        if header_secret(&parts.headers, name).is_some_and(|token| own_secret(app, token)) {
            parts.headers.remove(name);
        }
    }
    let Some(query) = parts.uri.query() else {
        return;
    };
    let kept: Vec<&str> = query
        .split('&')
        .filter(|pair| !is_own_key(app, pair))
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

/// Whether the query pair `pair` is `key=` with one of the gateway's keys.
fn is_own_key(app: &Inner, pair: &str) -> bool {
    pair.split_once('=')
        .is_some_and(|(name, value)| name == "key" && own_secret(app, &percent_decode(value)))
}

fn own_secret(app: &Inner, secret: &str) -> bool {
    app.keys.as_ref().map_or_else(
        || tokens_match(&current_token(app), secret),
        |keys| keys.contains(secret).unwrap_or(false),
    )
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
    continuation: Option<hofund_router::RouteContinuation>,
    started: Instant,
    identity: &Identity,
    track: &mut RequestTrack,
    capture: Option<prev_response::Capture>,
) -> Response {
    let mut meta = UsageMeta::new(request, upstream, started, identity);
    let member = Some(upstream.to_owned());
    let capture = prev_response::ReplyCapture::new(capture);
    match completion {
        Completion::Unary(response) => {
            if let Some(usage) = response.usage.as_ref() {
                limits::observe_usage(&mut meta, usage, codec.id());
            }
            let input = meta.input;
            let output = meta.output;
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
                    codec.encode_stream_error(status_of(error).as_u16(), &broken_off(error))
                }
            };
            // Move the track into the stream so finish runs when the body ends.
            let record = RecordOnEnd {
                usage: Arc::clone(&app.usage),
                meta,
                input: None,
                output: None,
                protocol: codec.id().to_owned(),
                track: Some(track.take_for_stream()),
                member,
            };
            let body: Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>> =
                if let Some(continuation) = continuation {
                    Box::pin(refusal_handover::body(
                        events,
                        continuation,
                        Arc::clone(&codec),
                        request.clone(),
                        encoder,
                        record,
                        Box::new(on_error),
                    ))
                } else {
                    Box::pin(sse_body(
                        refusal_handover::refusals_fail(events),
                        encoder,
                        record,
                        Box::new(on_error),
                    ))
                };
            let body = capture_stream(body, capture);
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, codec.stream_content_type())
                .body(Body::from_stream(body))
                .unwrap_or_else(|_| text_response(StatusCode::INTERNAL_SERVER_ERROR, "stream"))
        }
        Completion::Raw(body) => {
            if let Some(continuation) = continuation {
                match refusal_handover::events(Completion::Raw(body), &codec, &request.model) {
                    Err(error) => {
                        let status = status_of(&error);
                        write_usage(&app.usage, &meta, None, status);
                        track.end_with(member, status.as_u16(), None, None);
                        coded(codec.as_ref(), status, error.message())
                    }
                    Ok(events) => {
                        let encoder = codec.stream_encoder_for(request);
                        let on_error = {
                            let codec = Arc::clone(&codec);
                            move |error: &PluginError| {
                                codec.encode_stream_error(
                                    status_of(error).as_u16(),
                                    &broken_off(error),
                                )
                            }
                        };
                        let record = RecordOnEnd {
                            usage: Arc::clone(&app.usage),
                            meta,
                            input: None,
                            output: None,
                            protocol: codec.id().to_owned(),
                            track: Some(track.take_for_stream()),
                            member,
                        };
                        let body = refusal_handover::body(
                            events,
                            continuation,
                            Arc::clone(&codec),
                            request.clone(),
                            encoder,
                            record,
                            Box::new(on_error),
                        );
                        let body = capture_stream(body, capture);
                        Response::builder()
                            .status(StatusCode::OK)
                            .header(header::CONTENT_TYPE, codec.stream_content_type())
                            .body(Body::from_stream(body))
                            .unwrap_or_else(|_| {
                                text_response(StatusCode::INTERNAL_SERVER_ERROR, "stream")
                            })
                    }
                }
            } else {
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
            move |message: &str| codec.encode_stream_failure(message)
        };
        let owned = track.take_for_stream();
        let event_stream = content_type == "text/event-stream";
        let relay = RawRelay {
            body,
            meta,
            usage,
            tap,
            input_includes_cache: limits::input_includes_cache(codec.id()),
            on_error,
            severed_end: codec.severed_stream_end(),
            lines: event_stream.then(LineHold::default),
            ends: event_stream
                .then(|| EndWatch::new(codec.stream_end()))
                .flatten(),
            sent_tail: Vec::new(),
            phase: 0,
            track: Some(owned),
            member,
        };
        let stream = futures_util::stream::unfold(relay, |mut relay| async move {
            let next = relay.next().await?;
            Some((Ok::<Bytes, std::io::Error>(next), relay))
        });
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
    if let Ok(response) = codec.decode_response(&bytes)
        && let Some(usage) = response.usage.as_ref()
    {
        limits::observe_usage(&mut meta, usage, codec.id());
    }
    let (input, output) = codec.response_tokens(&bytes);
    meta.input = input.or(meta.input);
    meta.output = output.or(meta.output);
    write_usage(
        &app.usage,
        &meta,
        Some((meta.input, meta.output)),
        StatusCode::OK,
    );
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

// The caller identity rides along for key usage attribution.
#[allow(clippy::too_many_arguments)]
fn failure(
    app: Arc<Inner>,
    codec: &dyn Codec,
    headers: &HeaderMap,
    request: &ModelRequest,
    error: RouterError,
    started: Instant,
    identity: &Identity,
    track: &mut RequestTrack,
) -> Response {
    if let RouterError::PreviousResponseNotFound { id } = &error {
        write_usage(
            &app.usage,
            &UsageMeta::new(request, "", started, identity),
            None,
            StatusCode::BAD_REQUEST,
        );
        track.end_with(None, StatusCode::BAD_REQUEST.as_u16(), None, None);
        return json_response(StatusCode::BAD_REQUEST, prev_response::not_found_body(id));
    }
    let (status, upstream, message) = match &error {
        RouterError::NoRoute { .. } => (StatusCode::NOT_FOUND, String::new(), error.to_string()),
        RouterError::PreviousResponseNotFound { .. } => unreachable!(),
        RouterError::Ambiguous { .. } => {
            (StatusCode::BAD_REQUEST, String::new(), error.to_string())
        }
        RouterError::ModelScope { .. } | RouterError::AccountScope { .. } => (
            StatusCode::FORBIDDEN,
            String::new(),
            scope_error(&error, identity),
        ),
        RouterError::Upstream { upstream_id, error } => (
            status_of(error),
            upstream_id.clone(),
            error.message().to_owned(),
        ),
    };
    write_usage(
        &app.usage,
        &UsageMeta::new(request, &upstream, started, identity),
        None,
        status,
    );
    let member = if upstream.is_empty() {
        None
    } else {
        Some(upstream)
    };
    track.end_with(member, status.as_u16(), None, None);
    if compacts_in_stream_client(|name| headers.get(name).and_then(|value| value.to_str().ok()))
        && request.stream
        && codec.id() == ProtocolId::OPENAI_RESPONSES.as_str()
        && (status == StatusCode::PAYLOAD_TOO_LARGE
            || too_long(status.as_u16(), &words_of(&message)))
    {
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, codec.stream_content_type())
            .body(Body::from(
                codec.encode_stream_error(status.as_u16(), &message),
            ))
            .unwrap_or_else(|_| text_response(StatusCode::INTERNAL_SERVER_ERROR, "stream"));
    }
    coded(codec, status, &message)
}

/// Text that ends a stream that broke off after it began; an inbound
/// catalog's compaction reads the same words.
const ENDED_SHORT: &str = "the reply ended before it was complete";

/// What a stream that failed after it began says: a reply whose body
/// broke off says it ended before it was complete, so a client (a catalog's
/// compaction) can tell a cut from a vendor's own failure.
fn broken_off(error: &PluginError) -> String {
    let message = error.message();
    if error.kind() == PluginErrorKind::Interrupted && !message.ends_with(ENDED_SHORT) {
        format!("{message}: {ENDED_SHORT}")
    } else {
        message.to_owned()
    }
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
    identity: Identity,
    input: Option<i64>,
    output: Option<i64>,
    ttft_ms: Option<i64>,
    details: hofund_usage::UsageDetails,
}

impl UsageMeta {
    fn new(request: &ModelRequest, upstream: &str, started: Instant, identity: &Identity) -> Self {
        Self {
            model: request.model.clone(),
            upstream: upstream.to_owned(),
            started,
            identity: identity.clone(),
            input: None,
            output: None,
            ttft_ms: None,
            details: hofund_usage::UsageDetails::default(),
        }
    }

    fn note_first_content(&mut self) {
        if self.ttft_ms.is_none() {
            self.ttft_ms =
                Some(i64::try_from(self.started.elapsed().as_millis()).unwrap_or(i64::MAX));
        }
    }
}

struct RecordOnEnd {
    usage: Arc<Ledger>,
    meta: UsageMeta,
    input: Option<i64>,
    output: Option<i64>,
    protocol: String,
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
                        note_usage(&event, &mut record);
                        if matches!(
                            event,
                            StreamEvent::TextDelta(_)
                                | StreamEvent::ThinkingDelta(_)
                                | StreamEvent::ToolArgumentsDelta(_)
                        ) {
                            record.meta.note_first_content();
                        }
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

/// State of a relayed raw stream.
struct RawRelay<E> {
    body: BodyStream,
    meta: UsageMeta,
    usage: Arc<Ledger>,
    tap: Box<dyn hofund_codec::RawTap>,
    input_includes_cache: bool,
    on_error: E,
    severed_end: &'static [u8],
    /// Holds an unfinished last line of an event stream. `None` for other
    /// stream framings.
    lines: Option<LineHold>,
    /// Watches an event stream for its last event. `None` when the framing
    /// or the protocol has none to check.
    ends: Option<EndWatch>,
    /// The last few bytes the client got, to place an appended event.
    sent_tail: Vec<u8>,
    /// 0 relays, 1 records a clean end, 2 is done.
    phase: u8,
    track: Option<RequestTrack>,
    member: Option<String>,
}

impl<E> RawRelay<E>
where
    E: Fn(&str) -> Vec<u8>,
{
    fn finish(&mut self, status: StatusCode) {
        write_usage(
            &self.usage,
            &self.meta,
            Some((self.meta.input, self.meta.output)),
            status,
        );
        if let Some(track) = self.track.as_mut() {
            track.end_with(
                self.member.clone(),
                status.as_u16(),
                self.meta.input.and_then(|value| u64::try_from(value).ok()),
                self.meta.output.and_then(|value| u64::try_from(value).ok()),
            );
        }
    }

    fn note_tokens(&mut self) {
        let (input, output) = self.tap.tokens();
        let (cache_read, cache_write) = self.tap.cache_tokens();
        self.meta.input = input.or(self.meta.input);
        self.meta.output = output.or(self.meta.output);
        self.meta.details.cache_read_tokens = cache_read.or(self.meta.details.cache_read_tokens);
        self.meta.details.cache_write_tokens = cache_write.or(self.meta.details.cache_write_tokens);
        if let Some(input) = self.meta.input {
            let cached = if self.input_includes_cache {
                self.meta.details.cache_read_tokens.unwrap_or(0)
            } else {
                0
            };
            self.meta.details.uncached_input_tokens = Some(input.saturating_sub(cached));
        }
    }

    /// The last bytes of an event stream that ended cleanly before its last
    /// event: what it sent without a half line, then the protocol's failure
    /// event, and a 502 row (`reference: internal/gateway/gateway.go:5955-5990`,
    /// `internal/gateway/codex_backend.go:1490-1500`).
    fn ended_short(&mut self, mut out: Vec<u8>) -> Bytes {
        self.phase = 2;
        self.finish(StatusCode::BAD_GATEWAY);
        drop_half_line(&mut out);
        let before = match (out.is_empty(), self.sent_tail.is_empty()) {
            (true, true) => &b""[..],
            (true, false) => event_gap(&self.sent_tail),
            (false, _) => event_gap(&out),
        };
        out.extend_from_slice(before);
        let message = match self.member.as_deref() {
            Some(member) => format!("{member}: {ENDED_SHORT}"),
            None => ENDED_SHORT.to_owned(),
        };
        out.extend((self.on_error)(&message));
        Bytes::from(out)
    }

    /// The next bytes for the client, or `None` when the stream is over.
    /// An empty chunk is a step that forwarded nothing.
    async fn next(&mut self) -> Option<Bytes> {
        match self.phase {
            1 => {
                self.phase = 2;
                self.finish(StatusCode::OK);
                return None;
            }
            2 => return None,
            _ => {}
        }
        match self.body.next_chunk().await {
            None => {
                let mut tail = self.tap.finish();
                self.note_tokens();
                if let Some(lines) = self.lines.as_mut() {
                    tail = lines.release(&tail);
                }
                if let Some(ends) = self.ends.as_mut() {
                    ends.lines(&tail);
                    if !ends.reached() {
                        return Some(self.ended_short(tail));
                    }
                }
                if tail.is_empty() {
                    self.phase = 2;
                    self.finish(StatusCode::OK);
                    None
                } else {
                    self.phase = 1;
                    Some(Bytes::from(tail))
                }
            }
            Some(Err(error)) => {
                let held = self.tap.abandon();
                self.note_tokens();
                self.phase = 2;
                self.finish(StatusCode::BAD_GATEWAY);
                let mut out = match self.lines.as_mut() {
                    Some(lines) => lines.drop_half(&held),
                    None => held,
                };
                let before = match (out.is_empty(), self.sent_tail.is_empty()) {
                    (true, true) => &b""[..],
                    (true, false) => event_gap(&self.sent_tail),
                    (false, _) => event_gap(&out),
                };
                out.extend_from_slice(before);
                out.extend((self.on_error)(&broken_off(&error)));
                if self.lines.is_some() {
                    out.extend_from_slice(self.severed_end);
                }
                Some(Bytes::from(out))
            }
            Some(Ok(chunk)) => {
                self.meta.note_first_content();
                let mut forwarded = self.tap.push(&chunk);
                self.note_tokens();
                if let Some(lines) = self.lines.as_mut() {
                    forwarded = lines.push(&forwarded);
                }
                if let Some(ends) = self.ends.as_mut() {
                    ends.lines(&forwarded);
                }
                if !forwarded.is_empty() {
                    let keep = forwarded.len().saturating_sub(4);
                    self.sent_tail.clear();
                    self.sent_tail.extend_from_slice(&forwarded[keep..]);
                }
                Some(Bytes::from(forwarded))
            }
        }
    }
}

/// Holds the line an event stream's last read left unfinished.
///
/// Only ended lines reach the client as they arrive. The unfinished line
/// goes out when the stream ends cleanly and is dropped when the read fails,
/// unless it is a whole `data:` line short of its line end
/// (`reference: internal/gateway/gateway.go:5709-5760`).
#[derive(Default)]
struct LineHold {
    pending: Vec<u8>,
}

impl LineHold {
    /// Adds `bytes` and returns everything up to the last line end.
    fn push(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.pending.extend_from_slice(bytes);
        match self.pending.iter().rposition(|byte| *byte == b'\n') {
            Some(index) => {
                let rest = self.pending.split_off(index + 1);
                std::mem::replace(&mut self.pending, rest)
            }
            None => Vec::new(),
        }
    }

    /// The held line and `tail`, as they came, for a stream that ended cleanly.
    fn release(&mut self, tail: &[u8]) -> Vec<u8> {
        let mut out = std::mem::take(&mut self.pending);
        out.extend_from_slice(tail);
        out
    }

    /// The held bytes and `tail` for a stream whose read failed, without a
    /// half line no client can parse.
    fn drop_half(&mut self, tail: &[u8]) -> Vec<u8> {
        let mut out = self.release(tail);
        drop_half_line(&mut out);
        out
    }
}

/// Drops an unfinished last line no client can parse, unless it is a whole
/// `data:` line short of its line end.
fn drop_half_line(out: &mut Vec<u8>) {
    let start = out
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    if !whole_data_line(&out[start..]) {
        out.truncate(start);
    }
}

/// Whether `line` is a `data:` line whose payload is `[DONE]` or valid JSON.
fn whole_data_line(line: &[u8]) -> bool {
    let line = line.trim_ascii();
    let Some(data) = line.strip_prefix(b"data:") else {
        return false;
    };
    let data = data.trim_ascii();
    data == b"[DONE]" || serde_json::from_slice::<serde_json::Value>(data).is_ok()
}

/// What goes between bytes already sent and an appended event: nothing after
/// a closed frame, a blank line after an ended line, otherwise a line end and
/// a blank line.
fn event_gap(bytes: &[u8]) -> &'static [u8] {
    if frame_closed(bytes) {
        b""
    } else if bytes.ends_with(b"\n") || bytes.ends_with(b"\r") {
        b"\n"
    } else {
        b"\n\n"
    }
}

fn frame_closed(bytes: &[u8]) -> bool {
    bytes.ends_with(b"\n\n") || bytes.ends_with(b"\r\n\r\n") || bytes.ends_with(b"\r\r")
}

fn note_usage(event: &StreamEvent, record: &mut RecordOnEnd) {
    let usage = match event {
        StreamEvent::MessageStart(start) => start.usage.as_ref(),
        StreamEvent::MessageDelta(delta) => delta.usage.as_ref(),
        _ => None,
    };
    let Some(usage) = usage else {
        return;
    };
    limits::observe_usage(&mut record.meta, usage, &record.protocol);
    record.input = record.meta.input;
    record.output = record.meta.output;
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
        caller_key_id: meta.identity.key_id.clone(),
        caller_key_name: meta.identity.key_name.clone(),
        caller_issuer_id: meta.identity.issuer.clone(),
    };
    let mut details = meta.details;
    if details.uncached_input_tokens.is_none() {
        details.uncached_input_tokens = input_tokens;
    }
    let _ = usage.record_with_details(&row, meta.ttft_ms, details);
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
            reservation: None,
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
                reservation: None,
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
    use super::{forwarded_details, forwarded_tokens};

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

    #[test]
    fn l3_forwarded_cache_details_follow_the_protocol_shape() {
        let chat = forwarded_details(
            br#"{"usage":{"prompt_tokens":10,"prompt_tokens_details":{"cached_tokens":4}}}"#,
            Some(10),
        );
        assert_eq!(chat.uncached_input_tokens, Some(6));
        assert_eq!(chat.cache_read_tokens, Some(4));
        let messages = forwarded_details(
            br#"{"usage":{"input_tokens":10,"cache_read_input_tokens":4,"cache_creation_input_tokens":2}}"#,
            Some(10),
        );
        assert_eq!(messages.uncached_input_tokens, Some(10));
        assert_eq!(messages.cache_read_tokens, Some(4));
        assert_eq!(messages.cache_write_tokens, Some(2));
    }
}

#[cfg(test)]
mod line_hold_tests {
    use super::{LineHold, event_gap, whole_data_line};

    #[test]
    fn d23_4_a_stream_that_broke_off_says_it_ended_short() {
        let cut = hofund_plugin_api::PluginError::interrupted("error decoding response body");
        assert_eq!(
            super::broken_off(&cut),
            format!("error decoding response body: {}", super::ENDED_SHORT)
        );
        let again =
            hofund_plugin_api::PluginError::interrupted(format!("relay: {}", super::ENDED_SHORT));
        assert_eq!(
            super::broken_off(&again),
            format!("relay: {}", super::ENDED_SHORT)
        );
        let vendor = hofund_plugin_api::PluginError::status_error(502, "upstream said no");
        assert_eq!(super::broken_off(&vendor), "upstream said no");
    }

    #[test]
    fn d21_only_ended_lines_pass_the_hold() {
        let mut hold = LineHold::default();
        assert!(hold.push(b"data: {\"a\"").is_empty());
        assert_eq!(hold.push(b":1}\n\ndata: {"), b"data: {\"a\":1}\n\n");
        assert_eq!(hold.push(b"}\n"), b"data: {}\n");
        assert!(hold.push(b"").is_empty());
    }

    #[test]
    fn d21_a_clean_end_releases_the_held_line_as_it_came() {
        let mut hold = LineHold::default();
        let _ = hold.push(b"data: {\"a\":1}\n\ndata: {\"b\"");
        assert_eq!(hold.release(b""), b"data: {\"b\"");
        assert!(hold.release(b"").is_empty());
    }

    #[test]
    fn d21_a_failed_read_drops_the_half_line() {
        let mut hold = LineHold::default();
        let _ = hold.push(b"event: x\ndata: {\"b\"");
        assert!(hold.drop_half(b"").is_empty());
        let mut hold = LineHold::default();
        assert_eq!(
            hold.drop_half(b"data: {\"a\":1}\n\ndata: {\"b\""),
            b"data: {\"a\":1}\n\n"
        );
        assert!(hold.release(b"").is_empty());
    }

    #[test]
    fn d21_a_whole_data_line_short_of_its_end_is_kept() {
        let mut hold = LineHold::default();
        let _ = hold.push(b"data: {\"a\":1}");
        assert_eq!(hold.drop_half(b""), b"data: {\"a\":1}");
        let mut hold = LineHold::default();
        assert_eq!(hold.drop_half(b"data: [DONE]"), b"data: [DONE]");
    }

    #[test]
    fn d21_whole_data_line_reads_done_and_json_only() {
        assert!(whole_data_line(b"data: [DONE]"));
        assert!(whole_data_line(b"data:{\"a\":1}\r"));
        assert!(whole_data_line(b"  data: 7 "));
        assert!(!whole_data_line(b"data: {\"a\":"));
        assert!(!whole_data_line(b"event: message_stop"));
        assert!(!whole_data_line(b"{\"a\":1}"));
        assert!(!whole_data_line(b"[DONE]"));
        assert!(!whole_data_line(b""));
    }

    #[test]
    fn d21_the_gap_before_an_appended_event_closes_the_frame_once() {
        assert_eq!(event_gap(b"data: {}\n\n"), b"");
        assert_eq!(event_gap(b"data: {}\n"), b"\n");
        assert_eq!(event_gap(b"data: {}\r"), b"\n");
        assert_eq!(event_gap(b"data: {}"), b"\n\n");
    }
}
