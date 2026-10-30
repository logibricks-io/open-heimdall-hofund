//! The only supported constructors for [`HostContext`] and [`SurfaceHost`].
//!
//! Plugins do not depend on this crate. A plugin reaches a [`HostContext`]
//! through [`TestHost`] in tests, and through `build` at runtime.
//! [`test_host`] returns the context alone. [`TestHost`] also exposes the
//! recording backends so a test can seed secrets and HTTP responses.
//!
//! The account backend shipped here is [`AccountStore`]: mode `0600` metadata
//! with secrets in the OS credential store or an authenticated encrypted file.
//! The scheduler records periodic jobs and does not run them; it runs
//! one-shot work on the current Tokio runtime. The daemon builds one
//! context per provider with [`runtime_host`]. The gateway builds a
//! [`SurfaceHost`] per inbound request.

#![deny(missing_docs, unreachable_pub, rust_2018_idioms)]

mod account_secrets;
mod accounts;
mod agent_credentials;
mod loopback;
mod piped;
mod pool;
mod scripted;

use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use hofund_plugin_api::kernel::{
    AccountBackend, ClockBackend, DirBackend, HttpRequest, HttpResponse, Job, LoopbackBackend,
    PipedChildBackend, ProcessBackend, SchedulerBackend, SecretsBackend,
};
use hofund_plugin_api::{
    AuthResult, Body, BodyStream, HostContext, MemberId, PluginError, PluginId, PluginKey,
    PluginKind, ProtocolId, QuotaObservation, SecretString, SignInCtx, SpawnRequest, SurfaceHost,
};

pub use accounts::{AccountStorageKind, AccountStore, StoreError};
pub use agent_credentials::native_credential_store;
pub use hofund_plugin_api::kernel::{GatewayBackend, HttpBackend, ObserveBackend};
pub use loopback::LoopbackRegistry;
pub use pool::{HttpLimits, HttpProfile, PooledHttp};
pub use scripted::{PipedScript, PipedSpawn};

/// The kernel side of a browser sign-in: the loopback listeners and the
/// pasted-address check.
pub mod sign_in {
    pub use crate::loopback::{LoopbackRegistry, parse_pasted_callback};
}

/// Builds the context for one sign-in attempt with a fresh random session id.
///
/// The session is 16 bytes from the OS random source, as 32 lowercase hex
/// digits. A plugin uses it to key in-flight sign-in state.
///
/// # Errors
///
/// Returns an internal [`PluginError`] when the OS random source fails.
pub fn sign_in_ctx(plugin: PluginId) -> Result<SignInCtx, PluginError> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut bytes = [0_u8; 16];
    getrandom::getrandom(&mut bytes)
        .map_err(|error| PluginError::internal(format!("sign-in session id: {error}")))?;
    let mut session = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        session.push(char::from(HEX[usize::from(byte >> 4)]));
        session.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    SignInCtx::new(plugin).with_session(session)
}

/// Builds a provider-scoped [`HostContext`] bound to `plugin`.
///
/// This compatibility constructor preserves the original provider-only API.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn host_context(
    plugin: PluginId,
    http: Arc<dyn HttpBackend>,
    secrets: Arc<dyn SecretsBackend>,
    accounts: Arc<dyn AccountBackend>,
    clock: Arc<dyn ClockBackend>,
    scheduler: Arc<dyn SchedulerBackend>,
    process: Arc<dyn ProcessBackend>,
    dirs: Arc<dyn DirBackend>,
    observe: Arc<dyn ObserveBackend>,
) -> HostContext {
    host_context_for(
        PluginKind::Provider,
        plugin,
        http,
        secrets,
        accounts,
        clock,
        scheduler,
        process,
        dirs,
        observe,
    )
}

/// Builds a [`HostContext`] for one plugin kind and id.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn host_context_for(
    kind: PluginKind,
    plugin: PluginId,
    http: Arc<dyn HttpBackend>,
    secrets: Arc<dyn SecretsBackend>,
    accounts: Arc<dyn AccountBackend>,
    clock: Arc<dyn ClockBackend>,
    scheduler: Arc<dyn SchedulerBackend>,
    process: Arc<dyn ProcessBackend>,
    dirs: Arc<dyn DirBackend>,
    observe: Arc<dyn ObserveBackend>,
) -> HostContext {
    HostContext::from_backends_for(
        PluginKey::new(kind, plugin),
        http,
        secrets,
        accounts,
        clock,
        scheduler,
        process,
        dirs,
        observe,
    )
}

/// Builds a [`SurfaceHost`]. `accepted` is the gateway's auth result for this request.
#[must_use]
pub fn surface_host(
    members: Vec<MemberId>,
    protocols: Vec<ProtocolId>,
    accepted: bool,
) -> SurfaceHost {
    SurfaceHost::from_parts(members, protocols, AuthResult::new(accepted), None, None)
}

/// Builds a [`SurfaceHost`] whose gateway handle runs `gateway`.
#[must_use]
pub fn surface_host_with_gateway(
    members: Vec<MemberId>,
    protocols: Vec<ProtocolId>,
    accepted: bool,
    gateway: Arc<dyn GatewayBackend>,
) -> SurfaceHost {
    SurfaceHost::from_parts(
        members,
        protocols,
        AuthResult::new(accepted),
        Some(gateway),
        None,
    )
}

/// Builds a [`SurfaceHost`] whose gateway handle runs `gateway` and whose
/// HTTP handle sends on `http`, the kernel's pooled client.
#[must_use]
pub fn surface_host_with_http(
    members: Vec<MemberId>,
    protocols: Vec<ProtocolId>,
    accepted: bool,
    gateway: Arc<dyn GatewayBackend>,
    http: Arc<dyn HttpBackend>,
) -> SurfaceHost {
    SurfaceHost::from_parts(
        members,
        protocols,
        AuthResult::new(accepted),
        Some(gateway),
        Some(http),
    )
}

/// A provider-scoped [`HostContext`] for one configured provider.
///
/// # Errors
///
/// Returns [`StoreError`] when the account file cannot be created.
pub fn runtime_host(
    plugin: PluginId,
    api_key: impl Into<String>,
    root: impl Into<PathBuf>,
    limits: HttpLimits,
) -> Result<HostContext, StoreError> {
    runtime_host_for(PluginKind::Provider, plugin, api_key, root, limits)
}

/// A [`HostContext`] for one plugin kind and id.
///
/// The HTTP handle is the pooled direct client with `limits`. `api_key` is
/// the secret `api_key` for this plugin. Accounts live at `root/accounts.json`.
///
/// # Errors
///
/// Returns [`StoreError`] when the account file cannot be created.
pub fn runtime_host_for(
    kind: PluginKind,
    plugin: PluginId,
    api_key: impl Into<String>,
    root: impl Into<PathBuf>,
    limits: HttpLimits,
) -> Result<HostContext, StoreError> {
    runtime_host_with_observe_for(
        kind,
        plugin,
        api_key,
        root,
        limits,
        Arc::new(RecordingObserve::default()),
    )
}

/// Builds a provider-scoped runtime context with an observation backend.
///
/// # Errors
///
/// Returns [`StoreError`] when the account file cannot be created.
pub fn runtime_host_with_observe(
    plugin: PluginId,
    api_key: impl Into<String>,
    root: impl Into<PathBuf>,
    limits: HttpLimits,
    observe: Arc<dyn ObserveBackend>,
) -> Result<HostContext, StoreError> {
    runtime_host_with_observe_for(PluginKind::Provider, plugin, api_key, root, limits, observe)
}

/// Builds a kind-scoped runtime context with an observation backend.
///
/// # Errors
///
/// Returns [`StoreError`] when the account file cannot be created.
pub fn runtime_host_with_observe_for(
    kind: PluginKind,
    plugin: PluginId,
    api_key: impl Into<String>,
    root: impl Into<PathBuf>,
    limits: HttpLimits,
    observe: Arc<dyn ObserveBackend>,
) -> Result<HostContext, StoreError> {
    let root = root.into();
    let accounts = AccountStore::open(root.join("accounts.json"))?;
    let plugin = PluginKey::new(kind, plugin);
    Ok(host_context_for(
        kind,
        plugin.id().clone(),
        Arc::new(PooledHttp::with_limits(HttpProfile::direct(), limits)),
        Arc::new(KeySecrets {
            plugin,
            api_key: api_key.into(),
        }),
        Arc::new(accounts),
        Arc::new(SystemClock),
        Arc::new(RecordingScheduler::default()),
        Arc::new(StdProcess::default()),
        Arc::new(RootDirs {
            root,
            #[cfg(test)]
            _guard: None,
        }),
        observe,
    ))
}

/// One `api_key` secret. [`Debug`] is not implemented.
struct KeySecrets {
    plugin: PluginKey,
    api_key: String,
}

#[async_trait]
impl SecretsBackend for KeySecrets {
    async fn get(&self, plugin: &PluginKey, id: &str) -> Result<Option<SecretString>, PluginError> {
        if plugin == &self.plugin && id == "api_key" && !self.api_key.is_empty() {
            Ok(Some(SecretString::from(self.api_key.clone())))
        } else {
            Ok(None)
        }
    }
}

/// A context whose HTTP handle is `http`. Tests use this to drive the pooled client.
#[cfg(test)]
pub(crate) fn context_for_http(http: Arc<dyn HttpBackend>) -> HostContext {
    let dirs = RootDirs::test();
    let accounts = AccountStore::open_with_backend(
        dirs.root.join("accounts.json"),
        Arc::new(account_secrets::MemoryAccountSecrets::default()),
    )
    .expect("test accounts");
    host_context(
        PluginId::from_static("relay"),
        http,
        Arc::new(RecordingSecrets::default()),
        Arc::new(accounts),
        Arc::new(FixedClock::new(SystemTime::UNIX_EPOCH)),
        Arc::new(RecordingScheduler::default()),
        Arc::new(RecordingProcess::default()),
        Arc::new(dirs),
        Arc::new(RecordingObserve::default()),
    )
}

/// A [`HostContext`] plus the recording backends a plugin test seeds.
pub struct TestHost {
    context: HostContext,
    http: Arc<RecordingHttp>,
    secrets: Arc<RecordingSecrets>,
    scheduler: Arc<RecordingScheduler>,
    process: Arc<RecordingProcess>,
    observe: Arc<RecordingObserve>,
    loopback: Arc<LoopbackRegistry>,
}

impl TestHost {
    /// Provider-scoped recording backends rooted at `root`.
    ///
    /// # Panics
    ///
    /// Panics when the account file cannot be created.
    #[must_use]
    pub fn open(plugin: PluginId, root: impl Into<PathBuf>) -> Self {
        Self::open_for(PluginKind::Provider, plugin, root)
    }

    /// Recording backends rooted at `root`, bound to a kind and plugin id.
    ///
    /// # Panics
    ///
    /// Panics when the account file cannot be created.
    #[must_use]
    pub fn open_for(kind: PluginKind, plugin: PluginId, root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let http = Arc::new(RecordingHttp::default());
        let secrets = Arc::new(RecordingSecrets::default());
        let scheduler = Arc::new(RecordingScheduler::default());
        let process = Arc::new(RecordingProcess::default());
        let observe = Arc::new(RecordingObserve::default());
        let loopback = Arc::new(LoopbackRegistry::new());
        let accounts = AccountStore::open_with_backend(
            root.join("accounts.json"),
            Arc::new(account_secrets::MemoryAccountSecrets::default()),
        )
        .expect("test host account file");
        let context = host_context_for(
            kind,
            plugin,
            Arc::clone(&http) as Arc<dyn HttpBackend>,
            Arc::clone(&secrets) as Arc<dyn SecretsBackend>,
            Arc::new(accounts),
            Arc::new(FixedClock::new(SystemTime::UNIX_EPOCH)),
            Arc::clone(&scheduler) as Arc<dyn SchedulerBackend>,
            Arc::clone(&process) as Arc<dyn ProcessBackend>,
            Arc::new(RootDirs {
                root,
                #[cfg(test)]
                _guard: None,
            }),
            Arc::clone(&observe) as Arc<dyn ObserveBackend>,
        )
        .with_loopback(Arc::clone(&loopback) as Arc<dyn LoopbackBackend>);
        Self {
            context,
            http,
            secrets,
            scheduler,
            process,
            observe,
            loopback,
        }
    }

    /// The context a plugin method receives.
    #[must_use]
    pub fn context(&self) -> &HostContext {
        &self.context
    }

    /// Scripted HTTP responses and the calls the plugin made.
    #[must_use]
    pub fn http(&self) -> &RecordingHttp {
        &self.http
    }

    /// Secrets a test inserts before the plugin reads them.
    #[must_use]
    pub fn secrets(&self) -> &RecordingSecrets {
        &self.secrets
    }

    /// Jobs registered through the context.
    #[must_use]
    pub fn scheduler(&self) -> &RecordingScheduler {
        &self.scheduler
    }

    /// Programs the plugin asked to spawn.
    #[must_use]
    pub fn process(&self) -> &RecordingProcess {
        &self.process
    }

    /// Quota observations the plugin recorded.
    #[must_use]
    pub fn observe(&self) -> &RecordingObserve {
        &self.observe
    }

    /// The loopback listeners the plugin bound. A test awaits or releases a
    /// sign-in through it, as the kernel's sign-in driver does.
    #[must_use]
    pub fn loopback(&self) -> &LoopbackRegistry {
        &self.loopback
    }
}

/// A [`HostContext`] whose backends record calls and do not touch the network.
///
/// `root` holds the account file and the per-plugin directories. Kits re-export
/// this function. Use [`TestHost`] when the test needs to seed a secret or an
/// HTTP response. A failure to create the account file panics: a missing temp
/// directory is a test bug.
///
/// # Panics
///
/// Panics when the account file cannot be created.
#[must_use]
pub fn test_host(plugin: PluginId, root: impl Into<PathBuf>) -> HostContext {
    TestHost::open(plugin, root).context().clone()
}

/// A kind-scoped [`HostContext`] whose backends do not touch the network.
///
/// # Panics
///
/// Panics when the account file cannot be created.
#[must_use]
pub fn test_host_for(kind: PluginKind, plugin: PluginId, root: impl Into<PathBuf>) -> HostContext {
    TestHost::open_for(kind, plugin, root).context().clone()
}

/// A clock that returns one instant. Tests use this. Production passes [`SystemClock`].
#[derive(Clone, Debug)]
pub struct FixedClock {
    now: SystemTime,
    monotonic: Duration,
}

impl FixedClock {
    /// A clock stuck at `now`, with monotonic time at zero.
    #[must_use]
    pub fn new(now: SystemTime) -> Self {
        Self {
            now,
            monotonic: Duration::ZERO,
        }
    }

    /// Sets the monotonic reading.
    #[must_use]
    pub fn with_monotonic(mut self, monotonic: Duration) -> Self {
        self.monotonic = monotonic;
        self
    }
}

impl ClockBackend for FixedClock {
    fn now(&self) -> SystemTime {
        self.now
    }

    fn monotonic(&self) -> Duration {
        self.monotonic
    }
}

/// The process clock.
///
/// [`ClockBackend::monotonic`] is time since the first call in this process.
/// It does not follow wall-clock steps.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl ClockBackend for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }

    fn monotonic(&self) -> Duration {
        static ORIGIN: OnceLock<Instant> = OnceLock::new();
        ORIGIN.get_or_init(Instant::now).elapsed()
    }
}

/// Spawns with [`Command::new`]. It never invokes a shell.
///
/// Standard input, output, and error are [`Stdio::null`] for
/// [`ProcessBackend::spawn`]. [`ProcessBackend::spawn_piped`] pipes stdin and
/// stdout, clears the environment down to an allowlist, and kills the child
/// when the returned handle is dropped. A spawned child is kept
/// until [`ProcessBackend::wait`] or [`ProcessBackend::kill`]. `kill` signals
/// the child and reaps it on a background thread, so the caller does not wait.
/// Any child still held is killed and reaped when the last handle is dropped.
#[derive(Clone, Default)]
pub struct StdProcess {
    children: Arc<ChildSet>,
}

impl std::fmt::Debug for StdProcess {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("StdProcess").finish_non_exhaustive()
    }
}

struct ChildSet {
    inner: Mutex<BTreeMap<u32, Child>>,
}

impl Default for ChildSet {
    fn default() -> Self {
        Self {
            inner: Mutex::new(BTreeMap::new()),
        }
    }
}

impl Drop for ChildSet {
    fn drop(&mut self) {
        let Ok(mut children) = self.inner.lock() else {
            return;
        };
        for (_, mut child) in std::mem::take(&mut *children) {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl StdProcess {
    /// The command a spawn would run, before stdio is replaced with null.
    /// Tests check the program and the args.
    #[must_use]
    pub fn command(request: &SpawnRequest) -> Command {
        let mut command = Command::new(request.program());
        command.args(request.args());
        command.current_dir(request.cwd());
        command.env_clear();
        command.envs(request.env().iter().map(|(key, value)| (key, value)));
        command
    }

    fn insert(&self, child: Child) -> Result<u32, PluginError> {
        let id = child.id();
        self.children
            .inner
            .lock()
            .map_err(|_| PluginError::internal("process lock is poisoned"))?
            .insert(id, child);
        Ok(id)
    }

    fn take(&self, id: u32) -> Result<Child, PluginError> {
        self.children
            .inner
            .lock()
            .map_err(|_| PluginError::internal("process lock is poisoned"))?
            .remove(&id)
            .ok_or_else(|| PluginError::io("process id is not a child of this host"))
    }
}

#[async_trait]
impl ProcessBackend for StdProcess {
    async fn spawn(
        &self,
        program: PathBuf,
        args: Vec<OsString>,
        env: Vec<(OsString, OsString)>,
        cwd: PathBuf,
    ) -> Result<u32, PluginError> {
        let request = SpawnRequest::new(program, args, env, cwd)?;
        let process = self.clone();
        accounts::run_blocking(move || {
            let mut command = Self::command(&request);
            command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let child = command
                .spawn()
                .map_err(|error| PluginError::io(error.to_string()))?;
            process.insert(child)
        })
        .await
    }

    async fn wait(&self, id: u32) -> Result<i32, PluginError> {
        let mut child = self.take(id)?;
        accounts::run_blocking(move || {
            let status = child
                .wait()
                .map_err(|error| PluginError::io(error.to_string()))?;
            Ok(status.code().unwrap_or(-1))
        })
        .await
    }

    fn kill(&self, id: u32) -> Result<(), PluginError> {
        let mut child = self.take(id)?;
        let killed = child.kill();
        let reaped = std::thread::Builder::new()
            .name("hofund-reap".into())
            .spawn(move || {
                let _ = child.wait();
            });
        killed.map_err(|error| PluginError::io(error.to_string()))?;
        reaped.map_err(|error| PluginError::io(error.to_string()))?;
        Ok(())
    }

    async fn spawn_piped(
        &self,
        program: PathBuf,
        args: Vec<OsString>,
        env: Vec<(OsString, OsString)>,
        cwd: PathBuf,
    ) -> Result<Box<dyn PipedChildBackend>, PluginError> {
        let request = SpawnRequest::new(program, args, env, cwd)?;
        let child = accounts::run_blocking(move || piped::spawn(&request)).await?;
        Ok(Box::new(child))
    }
}

/// One HTTP call the recording client saw.
///
/// Header values are not stored. [`RecordedRequest::sensitive_headers`] is the
/// names of headers the caller marked sensitive.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct RecordedRequest {
    method: String,
    uri: String,
    body: Vec<u8>,
    sensitive_headers: Vec<String>,
}

impl RecordedRequest {
    /// The HTTP method.
    #[must_use]
    pub fn method(&self) -> &str {
        &self.method
    }

    /// The request URI.
    #[must_use]
    pub fn uri(&self) -> &str {
        &self.uri
    }

    /// The request body.
    #[must_use]
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// Sensitive header names, sorted. Values are not recorded.
    #[must_use]
    pub fn sensitive_headers(&self) -> &[String] {
        &self.sensitive_headers
    }
}

enum Scripted {
    Body(Vec<u8>),
    Response {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    Connect(String),
    Timeout(String),
}

/// An HTTP backend that records calls and replies with scripted bodies.
#[derive(Default)]
pub struct RecordingHttp {
    calls: Mutex<Vec<RecordedRequest>>,
    scripted: Mutex<VecDeque<Scripted>>,
}

impl RecordingHttp {
    /// Queues a 200 body. The next `send` pops it. An empty queue replies with `ok`.
    pub fn enqueue(&self, body: impl Into<Vec<u8>>) {
        self.push(Scripted::Body(body.into()));
    }

    /// Queues a reply with `status`, `headers`, and `body`.
    ///
    /// # Panics
    ///
    /// The next `send` panics when `status` or a header is not valid HTTP.
    pub fn enqueue_response(
        &self,
        status: u16,
        headers: &[(&str, &str)],
        body: impl Into<Vec<u8>>,
    ) {
        self.push(Scripted::Response {
            status,
            headers: headers
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
            body: body.into(),
        });
    }

    /// The next `send` fails as [`PluginErrorKind::Connect`](hofund_plugin_api::PluginErrorKind::Connect).
    pub fn enqueue_connect(&self, message: impl Into<String>) {
        self.push(Scripted::Connect(message.into()));
    }

    /// The next `send` fails as [`PluginErrorKind::Timeout`](hofund_plugin_api::PluginErrorKind::Timeout).
    pub fn enqueue_timeout(&self, message: impl Into<String>) {
        self.push(Scripted::Timeout(message.into()));
    }

    fn push(&self, scripted: Scripted) {
        self.scripted.lock().expect("http lock").push_back(scripted);
    }

    /// Calls in the order they were sent.
    #[must_use]
    pub fn calls(&self) -> Vec<RecordedRequest> {
        self.calls.lock().expect("http lock").clone()
    }
}

fn recorded(request: &HttpRequest<Body>) -> RecordedRequest {
    let mut sensitive_headers = Vec::new();
    for (name, value) in request.headers() {
        if value.is_sensitive() {
            sensitive_headers.push(name.as_str().to_owned());
        }
    }
    sensitive_headers.sort();
    RecordedRequest {
        method: request.method().to_string(),
        uri: request.uri().to_string(),
        body: request.body().bytes().to_vec(),
        sensitive_headers,
    }
}

#[async_trait]
impl HttpBackend for RecordingHttp {
    async fn send(
        &self,
        request: HttpRequest<Body>,
    ) -> Result<HttpResponse<BodyStream>, PluginError> {
        self.calls
            .lock()
            .expect("http lock")
            .push(recorded(&request));
        match self.scripted.lock().expect("http lock").pop_front() {
            Some(Scripted::Connect(message)) => Err(PluginError::connect(message)),
            Some(Scripted::Timeout(message)) => Err(PluginError::timeout(message)),
            Some(Scripted::Body(body)) => Ok(HttpResponse::new(BodyStream::from_bytes(body))),
            Some(Scripted::Response {
                status,
                headers,
                body,
            }) => {
                let mut builder = HttpResponse::builder().status(status);
                for (name, value) in headers {
                    builder = builder.header(name, value);
                }
                Ok(builder
                    .body(BodyStream::from_bytes(body))
                    .expect("scripted response is valid HTTP"))
            }
            None => Ok(HttpResponse::new(BodyStream::from_bytes(b"ok".to_vec()))),
        }
    }
}

/// Secrets a test inserts, keyed by plugin kind and id. [`Debug`] is not implemented:
/// the map holds secret text.
#[derive(Default)]
pub struct RecordingSecrets {
    values: Mutex<BTreeMap<(PluginKind, String, String), String>>,
}

impl RecordingSecrets {
    /// Stores a provider secret for `plugin` at `id`, replacing a previous value.
    pub fn insert(&self, plugin: &PluginId, id: impl Into<String>, secret: impl Into<String>) {
        self.insert_for(
            &PluginKey::new(PluginKind::Provider, plugin.clone()),
            id,
            secret,
        );
    }

    /// Stores `secret` for a kind-qualified `plugin` at `id`.
    pub fn insert_for(&self, plugin: &PluginKey, id: impl Into<String>, secret: impl Into<String>) {
        self.values.lock().expect("secrets lock").insert(
            (plugin.kind(), plugin.id().as_str().to_owned(), id.into()),
            secret.into(),
        );
    }
}

#[async_trait]
impl SecretsBackend for RecordingSecrets {
    async fn get(&self, plugin: &PluginKey, id: &str) -> Result<Option<SecretString>, PluginError> {
        Ok(self
            .values
            .lock()
            .expect("secrets lock")
            .get(&(
                plugin.kind(),
                plugin.id().as_str().to_owned(),
                id.to_owned(),
            ))
            .cloned()
            .map(SecretString::from))
    }
}

struct ScheduledJob {
    name: String,
    every_ms: u64,
}

/// One-shot work still running, keyed by job id.
type OnceJobs = Arc<Mutex<BTreeMap<u64, (String, tokio::task::AbortHandle)>>>;

/// Jobs keyed by a stable id. Cancelling one id does not renumber the others.
///
/// Periodic jobs are recorded and not run. One-shot work from
/// [`SchedulerBackend::run_once`] runs on the current Tokio runtime until it
/// finishes, is cancelled, or [`RecordingScheduler::shutdown`] aborts it.
#[derive(Default)]
pub struct RecordingScheduler {
    next: Mutex<u64>,
    jobs: Mutex<BTreeMap<u64, ScheduledJob>>,
    once: OnceJobs,
}

impl RecordingScheduler {
    /// Registered jobs as `(id, name, interval_ms)`.
    #[must_use]
    pub fn jobs(&self) -> Vec<(u64, String, u64)> {
        self.jobs
            .lock()
            .expect("scheduler lock")
            .iter()
            .map(|(id, job)| (*id, job.name.clone(), job.every_ms))
            .collect()
    }

    /// Whether `id` is still registered.
    #[must_use]
    pub fn contains(&self, id: u64) -> bool {
        self.jobs.lock().expect("scheduler lock").contains_key(&id)
    }

    /// One-shot work still running, as `(id, name)`.
    #[must_use]
    pub fn running_once(&self) -> Vec<(u64, String)> {
        self.once
            .lock()
            .expect("scheduler lock")
            .iter()
            .map(|(id, (name, _))| (*id, name.clone()))
            .collect()
    }

    /// Drops every registered job and aborts one-shot work. The kernel calls this on process shutdown.
    pub fn shutdown(&self) {
        self.clear();
    }

    fn clear(&self) {
        self.jobs.lock().expect("scheduler lock").clear();
        let once = std::mem::take(&mut *self.once.lock().expect("scheduler lock"));
        for (_, (_, handle)) in once {
            handle.abort();
        }
    }

    fn next_id(&self) -> u64 {
        let mut next = self.next.lock().expect("scheduler lock");
        *next = next.saturating_add(1);
        *next
    }
}

impl SchedulerBackend for RecordingScheduler {
    fn register(&self, name: &str, every_ms: u64, _job: Job) -> Result<u64, PluginError> {
        let id = self.next_id();
        self.jobs.lock().expect("scheduler lock").insert(
            id,
            ScheduledJob {
                name: name.to_owned(),
                every_ms,
            },
        );
        Ok(id)
    }

    fn cancel(&self, id: u64) {
        self.jobs.lock().expect("scheduler lock").remove(&id);
        let once = self.once.lock().expect("scheduler lock").remove(&id);
        if let Some((_, handle)) = once {
            handle.abort();
        }
    }

    fn shutdown(&self) {
        self.clear();
    }

    fn run_once(
        &self,
        name: &str,
        work: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
    ) -> Result<u64, PluginError> {
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| PluginError::internal("one-shot job needs a Tokio runtime"))?;
        let id = self.next_id();
        let once = Arc::clone(&self.once);
        // The entry is inserted while this lock is held, so the task's own
        // removal below runs after the insert even when the work is instant.
        let mut running = self.once.lock().expect("scheduler lock");
        let task = runtime.spawn(async move {
            work.await;
            once.lock().expect("scheduler lock").remove(&id);
        });
        running.insert(id, (name.to_owned(), task.abort_handle()));
        Ok(id)
    }
}

/// Spawn requests the recording backend accepted. Nothing is executed.
///
/// A piped spawn plays the next [`PipedScript`] queued with
/// [`RecordingProcess::script_piped`], and is refused when none is queued.
#[derive(Default)]
pub struct RecordingProcess {
    programs: Mutex<Vec<PathBuf>>,
    next: Mutex<u32>,
    piped: scripted::Piped,
}

impl RecordingProcess {
    /// Program paths in spawn order.
    #[must_use]
    pub fn programs(&self) -> Vec<PathBuf> {
        self.programs.lock().expect("process lock").clone()
    }

    /// Queues what the next piped child does.
    pub fn script_piped(&self, script: PipedScript) {
        self.piped
            .scripts
            .lock()
            .expect("process lock")
            .push_back(script);
    }

    /// Piped spawns in order.
    #[must_use]
    pub fn piped_spawns(&self) -> Vec<PipedSpawn> {
        self.piped.spawns.lock().expect("process lock").clone()
    }

    /// Every write to a piped child's input, in order.
    #[must_use]
    pub fn stdin_writes(&self) -> Vec<Vec<u8>> {
        self.piped.shared.stdin.lock().expect("stdin lock").clone()
    }

    /// How many piped children were dropped, which kills a real child.
    #[must_use]
    pub fn piped_dropped(&self) -> usize {
        self.piped
            .shared
            .dropped
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    fn next_id(&self) -> u32 {
        let mut next = self.next.lock().expect("process lock");
        *next = next.saturating_add(1);
        *next
    }
}

#[async_trait]
impl ProcessBackend for RecordingProcess {
    async fn spawn(
        &self,
        program: PathBuf,
        _args: Vec<OsString>,
        _env: Vec<(OsString, OsString)>,
        _cwd: PathBuf,
    ) -> Result<u32, PluginError> {
        self.programs.lock().expect("process lock").push(program);
        Ok(self.next_id())
    }

    async fn wait(&self, id: u32) -> Result<i32, PluginError> {
        let next = *self.next.lock().expect("process lock");
        if id == 0 || id > next {
            return Err(PluginError::io("process id is not a child of this host"));
        }
        Ok(0)
    }

    fn kill(&self, id: u32) -> Result<(), PluginError> {
        let next = *self.next.lock().expect("process lock");
        if id == 0 || id > next {
            return Err(PluginError::io("process id is not a child of this host"));
        }
        Ok(())
    }

    async fn spawn_piped(
        &self,
        program: PathBuf,
        args: Vec<OsString>,
        env: Vec<(OsString, OsString)>,
        cwd: PathBuf,
    ) -> Result<Box<dyn PipedChildBackend>, PluginError> {
        let Some(script) = self.piped.scripts.lock().expect("process lock").pop_front() else {
            return Err(PluginError::unsupported("no piped child is scripted"));
        };
        let spawn = PipedSpawn {
            program,
            args,
            env,
            cwd,
        };
        self.piped
            .spawns
            .lock()
            .expect("process lock")
            .push(spawn.clone());
        Ok(Box::new(scripted::ScriptedChild::start(
            self.next_id(),
            spawn,
            script,
            Arc::clone(&self.piped.shared),
        )))
    }
}

struct RootDirs {
    root: PathBuf,
    #[cfg(test)]
    _guard: Option<tempfile::TempDir>,
}

#[cfg(test)]
impl RootDirs {
    fn test() -> Self {
        let guard = tempfile::tempdir().expect("test root");
        let root = guard.path().to_path_buf();
        Self {
            root,
            _guard: Some(guard),
        }
    }
}

impl DirBackend for RootDirs {
    fn cache_dir(&self, plugin: &PluginKey) -> Result<PathBuf, PluginError> {
        mkdir(
            &self
                .root
                .join("cache")
                .join(plugin.kind().as_str())
                .join(plugin.id().as_str()),
        )
    }

    fn state_dir(&self, plugin: &PluginKey) -> Result<PathBuf, PluginError> {
        mkdir(
            &self
                .root
                .join("state")
                .join(plugin.kind().as_str())
                .join(plugin.id().as_str()),
        )
    }
}

fn mkdir(path: &Path) -> Result<PathBuf, PluginError> {
    std::fs::create_dir_all(path).map_err(|error| PluginError::io(error.to_string()))?;
    Ok(path.to_path_buf())
}

/// Quota observations recorded by a test host.
#[derive(Default)]
pub struct RecordingObserve {
    seen: Mutex<Vec<QuotaObservation>>,
}

impl RecordingObserve {
    /// Observations in arrival order.
    #[must_use]
    pub fn observations(&self) -> Vec<QuotaObservation> {
        self.seen.lock().expect("observe lock").clone()
    }
}

impl ObserveBackend for RecordingObserve {
    fn observe(&self, observation: QuotaObservation) -> Result<(), PluginError> {
        self.seen.lock().expect("observe lock").push(observation);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use futures_util::FutureExt;
    use hofund_plugin_api::kernel::{
        self, AccountBackend, ClockBackend, ProcessBackend, SchedulerBackend, SecretsBackend,
    };
    use hofund_plugin_api::{
        Account, AccountId, JobSpec, PluginId, PluginKey, PluginKind, SecretString, SpawnRequest,
    };

    use super::{
        AccountStore, FixedClock, RecordingScheduler, StdProcess, SystemClock, host_context,
        host_context_for, surface_host, test_host,
    };

    fn plugin() -> PluginId {
        PluginId::from_static("example-agent")
    }

    fn key() -> PluginKey {
        PluginKey::new(PluginKind::Provider, plugin())
    }

    struct Scratch(tempfile::TempDir);

    impl std::ops::Deref for Scratch {
        type Target = std::path::Path;

        fn deref(&self) -> &Self::Target {
            self.0.path()
        }
    }

    fn root() -> Scratch {
        Scratch(tempfile::tempdir().expect("root"))
    }

    fn account_store(path: std::path::PathBuf) -> AccountStore {
        AccountStore::open_with_backend(
            path,
            std::sync::Arc::new(super::account_secrets::MemoryAccountSecrets::default()),
        )
        .expect("store")
    }

    #[test]
    fn test_roots_do_not_share_a_directory() {
        assert_ne!(&*root(), &*root());
    }

    fn idle_job() -> hofund_plugin_api::Job {
        std::sync::Arc::new(|| async {}.boxed())
    }

    #[tokio::test]
    async fn file_store_has_no_plaintext_secret_and_does_not_debug_it() {
        let root = root();
        let path = root.join("accounts.json");
        let store = account_store(path.clone());
        let account = Account::from_stored_scoped(key(), AccountId::from_static("work"), "Work")
            .expect("account");
        store
            .upsert(account, SecretString::from("secret-a"))
            .await
            .expect("upsert");
        let rendered = format!("{store:?}");
        assert!(!rendered.contains("secret-a"));
        assert!(rendered.contains("AccountStore"));
        let on_disk = std::fs::read_to_string(&path).expect("file");
        assert!(!on_disk.contains("secret-a"));
        assert!(on_disk.contains("os-keychain:v2"));
        let first = store
            .secret(&key(), &AccountId::from_static("work"))
            .await
            .expect("secret")
            .expect("present");
        assert_eq!(kernel::expose_secret(&first), "secret-a");
        store
            .upsert(
                Account::from_stored_scoped(key(), AccountId::from_static("work"), "Work")
                    .expect("account"),
                SecretString::from("secret-b"),
            )
            .await
            .expect("replace");
        let second = store
            .secret(&key(), &AccountId::from_static("work"))
            .await
            .expect("secret")
            .expect("present");
        assert_eq!(kernel::expose_secret(&second), "secret-b");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[tokio::test]
    async fn the_handle_rejects_another_plugins_account() {
        let root = root();
        let host = test_host(plugin(), root.to_path_buf());
        let foreign = Account::from_stored(
            PluginId::from_static("other-plugin"),
            AccountId::from_static("work"),
            "Work",
        )
        .expect("account");
        let error = host
            .accounts()
            .upsert(foreign, SecretString::from("nope"))
            .await
            .expect_err("foreign");
        assert_eq!(error.kind(), hofund_plugin_api::PluginErrorKind::Config);
        let account = host
            .accounts()
            .account(AccountId::from_static("work"), "Work")
            .expect("account");
        host.accounts()
            .upsert(account, SecretString::from("kept"))
            .await
            .expect("upsert");
        let loaded = host
            .accounts()
            .secret(&AccountId::from_static("work"))
            .await
            .expect("secret")
            .expect("present");
        assert_eq!(kernel::expose_secret(&loaded), "kept");
        assert_eq!(
            host.dirs().cache_dir().expect("cache").file_name(),
            Some(std::ffi::OsStr::new("example-agent"))
        );
    }

    #[test]
    fn std_process_does_not_use_a_shell() {
        let program = if cfg!(windows) {
            std::path::PathBuf::from(r"C:\Windows\System32\cmd.exe")
        } else {
            std::path::PathBuf::from("/bin/true")
        };
        let cwd = if cfg!(windows) {
            std::path::PathBuf::from(r"C:\Windows")
        } else {
            std::path::PathBuf::from("/tmp")
        };
        let request = SpawnRequest::new(program.clone(), vec!["--version".into()], Vec::new(), cwd)
            .expect("spawn");
        let command = StdProcess::command(&request);
        assert_eq!(command.get_program(), program.as_os_str());
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, ["--version"]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn std_process_reaps_a_child() {
        let process = StdProcess::default();
        let id = process
            .spawn(
                std::path::PathBuf::from("/usr/bin/true"),
                Vec::new(),
                Vec::new(),
                std::path::PathBuf::from("/tmp"),
            )
            .await
            .expect("spawn");
        let code = ProcessBackend::wait(&process, id).await.expect("wait");
        assert_eq!(code, 0);
        assert!(ProcessBackend::wait(&process, id).await.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn std_process_kill_does_not_wait_for_the_child() {
        let process = StdProcess::default();
        // macOS keeps `sleep` in `/bin`. A merged `/usr` keeps it in `/usr/bin`.
        // The macOS GitHub image has `/usr/bin/true` and not `/bin/true`.
        let program = ["/usr/bin/sleep", "/bin/sleep"]
            .into_iter()
            .map(std::path::PathBuf::from)
            .find(|path| path.is_file())
            .expect("sleep");
        let id = process
            .spawn(
                program,
                vec!["30".into()],
                Vec::new(),
                std::path::PathBuf::from("/tmp"),
            )
            .await
            .expect("spawn");
        let started = std::time::Instant::now();
        ProcessBackend::kill(&process, id).expect("kill");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(ProcessBackend::kill(&process, id).is_err());
    }

    #[tokio::test]
    async fn cancel_rejects_a_foreign_job_and_ids_stay_stable() {
        let scheduler = std::sync::Arc::new(RecordingScheduler::default());
        let plugin_a = plugin();
        let plugin_b = PluginId::from_static("other-plugin");
        let left_root = root();
        let right_root = root();
        let build = |plugin: PluginId, root: &std::path::Path| {
            host_context(
                plugin,
                std::sync::Arc::new(super::RecordingHttp::default()),
                std::sync::Arc::new(super::RecordingSecrets::default()),
                std::sync::Arc::new(account_store(root.join("accounts.json"))),
                std::sync::Arc::new(FixedClock::new(SystemTime::UNIX_EPOCH)),
                std::sync::Arc::clone(&scheduler) as std::sync::Arc<dyn SchedulerBackend>,
                std::sync::Arc::new(super::RecordingProcess::default()),
                std::sync::Arc::new(super::RootDirs::test()),
                std::sync::Arc::new(super::RecordingObserve::default()),
            )
        };
        let left = build(plugin_a, &left_root);
        let right = build(plugin_b, &right_root);
        let spec = JobSpec::new("refresh", Duration::from_secs(60)).expect("job");
        let first = left.scheduler().register(&spec, idle_job()).expect("first");
        let second = left
            .scheduler()
            .register(&spec, idle_job())
            .expect("second");
        assert!(right.scheduler().cancel(first).is_err());
        assert!(scheduler.contains(first.get()));
        left.scheduler().cancel(first).expect("cancel");
        assert!(!scheduler.contains(first.get()));
        assert!(scheduler.contains(second.get()));
        assert_eq!(second.get(), 2);
        scheduler.shutdown();
        assert!(scheduler.jobs().is_empty());
        let host = surface_host(
            Vec::new(),
            vec![hofund_plugin_api::ProtocolId::OPENAI_CHAT],
            true,
        );
        assert!(host.auth().accepted());
        let _ = SystemClock.monotonic();
    }

    #[tokio::test]
    async fn a_surface_host_sends_on_the_http_it_was_given() {
        struct NoGateway;
        #[async_trait::async_trait]
        impl super::GatewayBackend for NoGateway {
            async fn call(
                &self,
                _request: super::HttpRequest<super::Body>,
            ) -> Result<super::HttpResponse<super::BodyStream>, super::PluginError> {
                Err(super::PluginError::unsupported("none"))
            }
        }
        let bare = surface_host(Vec::new(), Vec::new(), true);
        assert!(bare.http().is_none());
        let gateway: std::sync::Arc<dyn super::GatewayBackend> = std::sync::Arc::new(NoGateway);
        let joined = super::surface_host_with_gateway(
            Vec::new(),
            Vec::new(),
            true,
            std::sync::Arc::clone(&gateway),
        );
        assert!(joined.http().is_none());
        let recording = std::sync::Arc::new(super::RecordingHttp::default());
        recording.enqueue("sent");
        let host = super::surface_host_with_http(
            Vec::new(),
            Vec::new(),
            false,
            gateway,
            std::sync::Arc::clone(&recording) as std::sync::Arc<dyn super::HttpBackend>,
        );
        assert!(!host.auth().accepted());
        assert!(host.gateway().is_available());
        let handle = host.http().expect("http");
        assert_eq!(handle, host.clone().http().expect("clone"));
        assert_eq!(format!("{handle:?}"), "HttpHandle { .. }");
        let other = super::surface_host_with_http(
            Vec::new(),
            Vec::new(),
            false,
            std::sync::Arc::new(NoGateway),
            std::sync::Arc::new(super::RecordingHttp::default()),
        );
        assert_ne!(handle, other.http().expect("other"));
        let request = super::HttpRequest::builder()
            .uri("https://upstream.test/x")
            .body(super::Body::from_bytes(b"in".to_vec()))
            .expect("request");
        let mut response = handle.send(request).await.expect("send");
        let chunk = response.body_mut().next_chunk().await.expect("chunk");
        assert_eq!(chunk.expect("bytes").as_ref(), b"sent");
        let calls = recording.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].uri(), "https://upstream.test/x");
        assert_eq!(calls[0].body(), b"in");
    }

    #[tokio::test]
    async fn scripted_response_keeps_status_headers_and_body() {
        let root = root();
        let host = super::TestHost::open(plugin(), root.to_path_buf());
        host.http()
            .enqueue_response(401, &[("retry-after", "7")], b"denied".to_vec());
        let send = || {
            host.context().http().send(
                hofund_plugin_api::kernel::HttpRequest::builder()
                    .uri("https://example.test/v1")
                    .body(hofund_plugin_api::Body::from_bytes(b"".as_slice()))
                    .expect("request"),
            )
        };
        let response = send().await.expect("send");
        assert_eq!(response.status().as_u16(), 401);
        assert_eq!(
            response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok()),
            Some("7")
        );
        let chunk = response
            .into_body()
            .next_chunk()
            .await
            .expect("chunk")
            .expect("bytes");
        assert_eq!(&chunk[..], b"denied");
        let fallback = send().await.expect("send");
        assert_eq!(fallback.status().as_u16(), 200);
    }

    #[tokio::test]
    async fn test_host_seeds_and_records() {
        let root = root();
        let host = super::TestHost::open(plugin(), root.to_path_buf());
        host.secrets()
            .insert(host.context().plugin(), "token", "value");
        let secret = host
            .context()
            .secrets()
            .get("token")
            .await
            .expect("get")
            .expect("present");
        assert_eq!(kernel::expose_secret(&secret), "value");
        host.http().enqueue(b"pong".to_vec());
        let response = host
            .context()
            .http()
            .send(
                hofund_plugin_api::kernel::HttpRequest::builder()
                    .uri("https://example.test/v1")
                    .body(hofund_plugin_api::Body::from_bytes(b"gpt".as_slice()))
                    .expect("request"),
            )
            .await
            .expect("send");
        let chunk = response
            .into_body()
            .next_chunk()
            .await
            .expect("chunk")
            .expect("bytes");
        assert_eq!(&chunk[..], b"pong");
        assert_eq!(host.http().calls().len(), 1);
        assert_eq!(host.http().calls()[0].body(), b"gpt");
        assert!(host.http().calls()[0].sensitive_headers().is_empty());
        host.context()
            .observe()
            .observe(host.context().observe().observation())
            .expect("observe");
        assert_eq!(host.observe().observations().len(), 1);
        let clock =
            FixedClock::new(SystemTime::UNIX_EPOCH).with_monotonic(Duration::from_millis(5));
        assert_eq!(clock.monotonic(), Duration::from_millis(5));
    }

    #[tokio::test]
    async fn equal_ids_in_different_kinds_have_isolated_host_storage() {
        let root = root();
        let secrets = std::sync::Arc::new(super::RecordingSecrets::default());
        let accounts = std::sync::Arc::new(account_store(root.join("accounts.json")));
        let build = |kind: PluginKind| {
            host_context_for(
                kind,
                PluginId::from_static("shared"),
                std::sync::Arc::new(super::RecordingHttp::default()),
                std::sync::Arc::clone(&secrets) as std::sync::Arc<dyn SecretsBackend>,
                std::sync::Arc::clone(&accounts) as std::sync::Arc<dyn AccountBackend>,
                std::sync::Arc::new(FixedClock::new(SystemTime::UNIX_EPOCH)),
                std::sync::Arc::new(super::RecordingScheduler::default()),
                std::sync::Arc::new(super::RecordingProcess::default()),
                std::sync::Arc::new(super::RootDirs {
                    root: root.to_path_buf(),
                    _guard: None,
                }),
                std::sync::Arc::new(super::RecordingObserve::default()),
            )
        };
        let provider = build(PluginKind::Provider);
        let subscription = build(PluginKind::Subscription);
        secrets.insert_for(provider.key(), "token", "provider-secret");
        secrets.insert_for(subscription.key(), "token", "subscription-secret");
        for (host, expected) in [
            (&provider, "provider-secret"),
            (&subscription, "subscription-secret"),
        ] {
            let secret = host
                .secrets()
                .get("token")
                .await
                .expect("get")
                .expect("present");
            assert_eq!(kernel::expose_secret(&secret), expected);
            host.accounts()
                .upsert(
                    host.accounts()
                        .account(AccountId::from_static("work"), "Work")
                        .expect("account"),
                    SecretString::from(expected),
                )
                .await
                .expect("upsert");
        }
        assert_eq!(provider.accounts().list().await.expect("list").len(), 1);
        assert_eq!(subscription.accounts().list().await.expect("list").len(), 1);
        let provider_account = provider
            .accounts()
            .secret(&AccountId::from_static("work"))
            .await
            .expect("secret")
            .expect("present");
        let subscription_account = subscription
            .accounts()
            .secret(&AccountId::from_static("work"))
            .await
            .expect("secret")
            .expect("present");
        assert_eq!(kernel::expose_secret(&provider_account), "provider-secret");
        assert_eq!(
            kernel::expose_secret(&subscription_account),
            "subscription-secret"
        );
        assert_eq!(
            provider.dirs().cache_dir().expect("cache"),
            root.join("cache/provider/shared")
        );
        assert_eq!(
            subscription.dirs().cache_dir().expect("cache"),
            root.join("cache/subscription/shared")
        );
        assert_ne!(
            provider.dirs().state_dir().expect("state"),
            subscription.dirs().state_dir().expect("state")
        );
    }

    #[tokio::test]
    async fn account_rows_without_a_kind_remain_subscription_rows() {
        let root = root();
        let path = root.join("accounts.json");
        std::fs::write(
            &path,
            r#"[{"plugin":"shared","account":"work","label":"Work","secret":"stored"}]"#,
        )
        .expect("legacy rows");
        let store = account_store(path);
        let subscription =
            PluginKey::new(PluginKind::Subscription, PluginId::from_static("shared"));
        assert_eq!(store.list(&subscription).await.expect("list").len(), 1);
        let provider = PluginKey::new(PluginKind::Provider, PluginId::from_static("shared"));
        assert!(store.list(&provider).await.expect("list").is_empty());
    }

    #[tokio::test]
    async fn a_plugin_cannot_read_another_plugins_secret() {
        let secrets = std::sync::Arc::new(super::RecordingSecrets::default());
        let plugin_a = plugin();
        let plugin_b = PluginId::from_static("other-plugin");
        secrets.insert(&plugin_a, "token", "value");
        let build = |plugin: PluginId| {
            host_context(
                plugin,
                std::sync::Arc::new(super::RecordingHttp::default()),
                std::sync::Arc::clone(&secrets) as std::sync::Arc<dyn SecretsBackend>,
                std::sync::Arc::new(account_store(root().join("accounts.json"))),
                std::sync::Arc::new(FixedClock::new(SystemTime::UNIX_EPOCH)),
                std::sync::Arc::new(super::RecordingScheduler::default()),
                std::sync::Arc::new(super::RecordingProcess::default()),
                std::sync::Arc::new(super::RootDirs::test()),
                std::sync::Arc::new(super::RecordingObserve::default()),
            )
        };
        let owned = build(plugin_a)
            .secrets()
            .get("token")
            .await
            .expect("get")
            .expect("present");
        assert_eq!(kernel::expose_secret(&owned), "value");
        assert!(
            build(plugin_b)
                .secrets()
                .get("token")
                .await
                .expect("get")
                .is_none()
        );
    }

    fn lease_host(root: &std::path::Path) -> super::TestHost {
        super::TestHost::open_for(PluginKind::Subscription, plugin(), root.to_path_buf())
    }

    #[tokio::test]
    async fn run_once_completes_after_the_waiter_is_dropped() {
        let root = root();
        let host = lease_host(&root);
        let (go_tx, go_rx) = tokio::sync::oneshot::channel::<()>();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        let context = host.context().clone();
        let waiter = tokio::spawn(async move {
            let id = context
                .scheduler()
                .run_once(
                    "sign-in-wait",
                    async move {
                        let _ = go_rx.await;
                        let _ = done_tx.send(());
                    }
                    .boxed(),
                )
                .expect("run_once");
            std::future::pending::<()>().await;
            id
        });
        tokio::task::yield_now().await;
        assert_eq!(host.scheduler().running_once().len(), 1);
        assert_eq!(host.scheduler().running_once()[0].1, "sign-in-wait");
        waiter.abort();
        assert!(waiter.await.expect_err("aborted").is_cancelled());
        go_tx.send(()).expect("work still waiting");
        tokio::time::timeout(Duration::from_secs(5), done_rx)
            .await
            .expect("work finished")
            .expect("done");
        for _ in 0..100 {
            if host.scheduler().running_once().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(host.scheduler().running_once().is_empty());
        assert!(
            host.scheduler().jobs().is_empty(),
            "one-shot is not periodic"
        );
    }

    struct DropSignal(Option<tokio::sync::oneshot::Sender<()>>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }

    #[tokio::test]
    async fn run_once_is_cancelled_on_shutdown() {
        let root = root();
        let host = lease_host(&root);
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel::<()>();
        let guard = DropSignal(Some(dropped_tx));
        host.context()
            .scheduler()
            .run_once(
                "forever",
                async move {
                    let _guard = guard;
                    std::future::pending::<()>().await;
                }
                .boxed(),
            )
            .expect("run_once");
        tokio::task::yield_now().await;
        assert_eq!(host.scheduler().running_once().len(), 1);
        host.scheduler().shutdown();
        assert!(host.scheduler().running_once().is_empty());
        tokio::time::timeout(Duration::from_secs(5), dropped_rx)
            .await
            .expect("work was dropped")
            .expect("signal");
    }

    #[tokio::test]
    async fn run_once_cancel_aborts_the_work() {
        let root = root();
        let host = lease_host(&root);
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel::<()>();
        let guard = DropSignal(Some(dropped_tx));
        let id = host
            .context()
            .scheduler()
            .run_once(
                "forever",
                async move {
                    let _guard = guard;
                    std::future::pending::<()>().await;
                }
                .boxed(),
            )
            .expect("run_once");
        host.context().scheduler().cancel(id).expect("cancel");
        assert!(host.scheduler().running_once().is_empty());
        tokio::time::timeout(Duration::from_secs(5), dropped_rx)
            .await
            .expect("work was dropped")
            .expect("signal");
        let error = host
            .context()
            .scheduler()
            .run_once("", async {}.boxed())
            .expect_err("empty name");
        assert_eq!(error.kind(), hofund_plugin_api::PluginErrorKind::Config);
    }

    #[test]
    fn run_once_without_a_runtime_is_an_internal_error() {
        let scheduler = RecordingScheduler::default();
        let error = scheduler
            .run_once("no-runtime", async {}.boxed())
            .expect_err("no runtime");
        assert_eq!(error.kind(), hofund_plugin_api::PluginErrorKind::Internal);
        assert!(scheduler.running_once().is_empty());
    }

    #[tokio::test]
    async fn lease_through_the_handle_redacts_and_commits() {
        let root = root();
        let host = lease_host(&root);
        let accounts = host.context().accounts();
        let account = accounts
            .account(AccountId::from_static("work"), "Work")
            .expect("account");
        accounts
            .upsert(account, SecretString::from("lease-secret-marker-41"))
            .await
            .expect("upsert");
        let lease = accounts
            .lease(&AccountId::from_static("work"), Duration::from_secs(1))
            .await
            .expect("lease");
        let rendered = format!("{lease:?}");
        assert!(!rendered.contains("lease-secret-marker-41"), "{rendered}");
        assert!(rendered.contains("<redacted>"));
        assert_eq!(
            kernel::expose_secret(lease.secret()),
            "lease-secret-marker-41"
        );
        let error = lease
            .commit(Some(String::new()), SecretString::from("x"))
            .await
            .expect_err("empty label");
        assert_eq!(error.kind(), hofund_plugin_api::PluginErrorKind::Config);
        let lease = accounts
            .lease(&AccountId::from_static("work"), Duration::ZERO)
            .await
            .expect("the failed commit released the lease");
        lease
            .commit(None, SecretString::from("refreshed"))
            .await
            .expect("commit");
        let secret = accounts
            .secret(&AccountId::from_static("work"))
            .await
            .expect("secret")
            .expect("present");
        assert_eq!(kernel::expose_secret(&secret), "refreshed");
    }

    #[test]
    fn sign_in_ctx_has_a_fresh_hex_session() {
        let first = super::sign_in_ctx(plugin()).expect("ctx");
        let second = super::sign_in_ctx(plugin()).expect("ctx");
        assert_eq!(first.session().len(), 32);
        assert!(
            first
                .session()
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        );
        assert_ne!(first.session(), second.session());
        assert_eq!(first.plugin(), &plugin());
    }

    struct MinimalSubscription(hofund_plugin_api::SignInFlow);

    #[async_trait::async_trait]
    impl hofund_plugin_api::Subscription for MinimalSubscription {
        fn id(&self) -> &PluginId {
            static ID: PluginId = PluginId::from_static("example-agent");
            &ID
        }

        fn flow(&self) -> &hofund_plugin_api::SignInFlow {
            &self.0
        }

        async fn start_sign_in(
            &self,
            _ctx: &hofund_plugin_api::SignInCtx,
            _host: &hofund_plugin_api::HostContext,
        ) -> Result<hofund_plugin_api::SignInChallenge, hofund_plugin_api::PluginError> {
            Err(hofund_plugin_api::PluginError::unsupported("test"))
        }

        async fn complete_sign_in(
            &self,
            _ctx: &hofund_plugin_api::SignInCtx,
            _proof: hofund_plugin_api::SignInProof,
            _host: &hofund_plugin_api::HostContext,
        ) -> Result<AccountId, hofund_plugin_api::PluginError> {
            Err(hofund_plugin_api::PluginError::unsupported("test"))
        }

        async fn refresh(
            &self,
            _account: &AccountId,
            _host: &hofund_plugin_api::HostContext,
        ) -> Result<(), hofund_plugin_api::PluginError> {
            Ok(())
        }

        async fn quota(
            &self,
            _account: &AccountId,
            _host: &hofund_plugin_api::HostContext,
        ) -> Result<Vec<hofund_plugin_api::QuotaWindow>, hofund_plugin_api::PluginError> {
            Ok(Vec::new())
        }

        async fn list_models(
            &self,
            _account: &AccountId,
            _host: &hofund_plugin_api::HostContext,
        ) -> Result<hofund_plugin_api::ModelList, hofund_plugin_api::PluginError> {
            Err(hofund_plugin_api::PluginError::unsupported("test"))
        }

        fn upstream(
            &self,
            _account: &AccountId,
            _host: &hofund_plugin_api::HostContext,
        ) -> Result<
            std::sync::Arc<dyn hofund_plugin_api::UpstreamProvider>,
            hofund_plugin_api::PluginError,
        > {
            Err(hofund_plugin_api::PluginError::unsupported("test"))
        }
    }

    #[tokio::test]
    async fn subscription_defaults_are_ready_and_cancel_is_ok() {
        use hofund_plugin_api::Subscription as _;
        let root = root();
        let host = lease_host(&root);
        let subscription = MinimalSubscription(hofund_plugin_api::SignInFlow::TokenFile);
        let status = subscription
            .account_status(&AccountId::from_static("work"), host.context())
            .await
            .expect("status");
        assert_eq!(status, hofund_plugin_api::AccountStatus::Ready);
        assert_eq!(
            hofund_plugin_api::AccountStatus::default(),
            hofund_plugin_api::AccountStatus::Ready
        );
        let ctx = super::sign_in_ctx(plugin()).expect("ctx");
        subscription
            .cancel_sign_in(&ctx, host.context())
            .await
            .expect("cancel");
    }
}
