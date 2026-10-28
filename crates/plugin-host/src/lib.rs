//! The only supported constructors for [`HostContext`] and [`SurfaceHost`].
//!
//! Plugins do not depend on this crate. A plugin reaches a [`HostContext`]
//! through [`TestHost`] in tests, and through `build` at runtime.
//! [`test_host`] returns the context alone. [`TestHost`] also exposes the
//! recording backends so a test can seed secrets and HTTP responses.
//!
//! The account backend shipped here is [`AccountStore`]: mode `0600` metadata
//! with secrets in the OS credential store or an authenticated encrypted file.
//! The scheduler records jobs and does not spawn tasks. The daemon builds one
//! context per provider with [`runtime_host`]. The gateway builds a
//! [`SurfaceHost`] per inbound request.

#![deny(missing_docs, unreachable_pub, rust_2018_idioms)]

mod account_secrets;
mod accounts;
mod pool;

use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use hofund_plugin_api::kernel::{
    AccountBackend, ClockBackend, DirBackend, HttpBackend, HttpRequest, HttpResponse, Job,
    ProcessBackend, SchedulerBackend, SecretsBackend,
};
use hofund_plugin_api::{
    AuthResult, Body, BodyStream, HostContext, MemberId, PluginError, PluginId, ProtocolId,
    QuotaObservation, SecretString, SpawnRequest, SurfaceHost,
};

pub use accounts::{AccountStorageKind, AccountStore, StoreError};
pub use hofund_plugin_api::kernel::ObserveBackend;
pub use pool::{HttpLimits, HttpProfile, PooledHttp};

/// Builds a [`HostContext`] bound to `plugin`.
///
/// This is the constructor. [`HostContext::from_backends`] exists so this
/// crate can fill the private handles. Plugins do not call either one.
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
    HostContext::from_backends(
        plugin, http, secrets, accounts, clock, scheduler, process, dirs, observe,
    )
}

/// Builds a [`SurfaceHost`]. `accepted` is the gateway's auth result for this request.
#[must_use]
pub fn surface_host(
    members: Vec<MemberId>,
    protocols: Vec<ProtocolId>,
    accepted: bool,
) -> SurfaceHost {
    SurfaceHost::from_parts(members, protocols, AuthResult::new(accepted))
}

/// A [`HostContext`] for one configured provider.
///
/// The HTTP handle is the pooled direct client with `limits`. `api_key` is
/// the secret `api_key` for this plugin. Accounts live at `root/accounts.json`.
/// The scheduler records jobs and does not run them.
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
    runtime_host_with_observe(
        plugin,
        api_key,
        root,
        limits,
        Arc::new(RecordingObserve::default()),
    )
}

/// Builds a runtime context with a production observation backend.
///
/// The daemon binds the backend to the configured provider's member. Plugins
/// only supply observations and cannot select which member receives them.
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
    let root = root.into();
    let accounts = AccountStore::open(root.join("accounts.json"))?;
    Ok(host_context(
        plugin.clone(),
        Arc::new(PooledHttp::with_limits(HttpProfile::direct(), limits)),
        Arc::new(KeySecrets {
            plugin,
            api_key: api_key.into(),
        }),
        Arc::new(accounts),
        Arc::new(SystemClock),
        Arc::new(RecordingScheduler::default()),
        Arc::new(StdProcess::default()),
        Arc::new(RootDirs { root }),
        observe,
    ))
}

/// One `api_key` secret. [`Debug`] is not implemented.
struct KeySecrets {
    plugin: PluginId,
    api_key: String,
}

#[async_trait]
impl SecretsBackend for KeySecrets {
    async fn get(&self, plugin: &PluginId, id: &str) -> Result<Option<SecretString>, PluginError> {
        if plugin.as_str() == self.plugin.as_str() && id == "api_key" && !self.api_key.is_empty() {
            Ok(Some(SecretString::from(self.api_key.clone())))
        } else {
            Ok(None)
        }
    }
}

/// A context whose HTTP handle is `http`. Tests use this to drive the pooled client.
#[cfg(test)]
pub(crate) fn context_for_http(http: Arc<dyn HttpBackend>) -> HostContext {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "hofund-pool-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&root).expect("root");
    let accounts = AccountStore::open_with_backend(
        root.join("accounts.json"),
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
        Arc::new(RootDirs { root }),
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
}

impl TestHost {
    /// Recording backends rooted at `root`, bound to `plugin`.
    ///
    /// # Panics
    ///
    /// Panics when the account file cannot be created.
    #[must_use]
    pub fn open(plugin: PluginId, root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let http = Arc::new(RecordingHttp::default());
        let secrets = Arc::new(RecordingSecrets::default());
        let scheduler = Arc::new(RecordingScheduler::default());
        let process = Arc::new(RecordingProcess::default());
        let observe = Arc::new(RecordingObserve::default());
        let accounts = AccountStore::open_with_backend(
            root.join("accounts.json"),
            Arc::new(account_secrets::MemoryAccountSecrets::default()),
        )
        .expect("test host account file");
        let context = host_context(
            plugin,
            Arc::clone(&http) as Arc<dyn HttpBackend>,
            Arc::clone(&secrets) as Arc<dyn SecretsBackend>,
            Arc::new(accounts),
            Arc::new(FixedClock::new(SystemTime::UNIX_EPOCH)),
            Arc::clone(&scheduler) as Arc<dyn SchedulerBackend>,
            Arc::clone(&process) as Arc<dyn ProcessBackend>,
            Arc::new(RootDirs { root }),
            Arc::clone(&observe) as Arc<dyn ObserveBackend>,
        );
        Self {
            context,
            http,
            secrets,
            scheduler,
            process,
            observe,
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
/// Standard input, output, and error are [`Stdio::null`]. The child is kept
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
            None => Ok(HttpResponse::new(BodyStream::from_bytes(b"ok".to_vec()))),
        }
    }
}

/// Secrets a test inserts, keyed by plugin. [`Debug`] is not implemented:
/// the map holds secret text.
#[derive(Default)]
pub struct RecordingSecrets {
    values: Mutex<BTreeMap<(String, String), String>>,
}

impl RecordingSecrets {
    /// Stores `secret` for `plugin` at `id`, replacing a previous value.
    pub fn insert(&self, plugin: &PluginId, id: impl Into<String>, secret: impl Into<String>) {
        self.values
            .lock()
            .expect("secrets lock")
            .insert((plugin.as_str().to_owned(), id.into()), secret.into());
    }
}

#[async_trait]
impl SecretsBackend for RecordingSecrets {
    async fn get(&self, plugin: &PluginId, id: &str) -> Result<Option<SecretString>, PluginError> {
        Ok(self
            .values
            .lock()
            .expect("secrets lock")
            .get(&(plugin.as_str().to_owned(), id.to_owned()))
            .cloned()
            .map(SecretString::from))
    }
}

struct ScheduledJob {
    name: String,
    every_ms: u64,
}

/// Jobs keyed by a stable id. Cancelling one id does not renumber the others.
#[derive(Default)]
pub struct RecordingScheduler {
    next: Mutex<u64>,
    jobs: Mutex<BTreeMap<u64, ScheduledJob>>,
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

    /// Drops every registered job. The kernel calls this on process shutdown.
    pub fn shutdown(&self) {
        self.clear();
    }

    fn clear(&self) {
        self.jobs.lock().expect("scheduler lock").clear();
    }
}

impl SchedulerBackend for RecordingScheduler {
    fn register(&self, name: &str, every_ms: u64, _job: Job) -> Result<u64, PluginError> {
        let mut next = self.next.lock().expect("scheduler lock");
        *next = next.saturating_add(1);
        let id = *next;
        drop(next);
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
    }

    fn shutdown(&self) {
        self.clear();
    }
}

/// Spawn requests the recording backend accepted. Nothing is executed.
#[derive(Default)]
pub struct RecordingProcess {
    programs: Mutex<Vec<PathBuf>>,
    next: Mutex<u32>,
}

impl RecordingProcess {
    /// Program paths in spawn order.
    #[must_use]
    pub fn programs(&self) -> Vec<PathBuf> {
        self.programs.lock().expect("process lock").clone()
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
        let mut next = self.next.lock().expect("process lock");
        *next = next.saturating_add(1);
        Ok(*next)
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
}

struct RootDirs {
    root: PathBuf,
}

impl DirBackend for RootDirs {
    fn cache_dir(&self, plugin: &PluginId) -> Result<PathBuf, PluginError> {
        mkdir(&self.root.join("cache").join(plugin.as_str()))
    }

    fn state_dir(&self, plugin: &PluginId) -> Result<PathBuf, PluginError> {
        mkdir(&self.root.join("state").join(plugin.as_str()))
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
    use hofund_plugin_api::{Account, AccountId, JobSpec, PluginId, SecretString, SpawnRequest};

    use super::{
        AccountStore, FixedClock, RecordingScheduler, StdProcess, SystemClock, host_context,
        surface_host, test_host,
    };

    fn plugin() -> PluginId {
        PluginId::from_static("example-agent")
    }

    fn root() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        // A wall-clock stamp is not unique across parallel tests. Windows
        // often returns the same `SystemTime` for the whole batch, so two
        // tests shared `accounts.json` and the second rename of `accounts.tmp`
        // failed with ERROR_FILE_NOT_FOUND.
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "hofund-host-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("root");
        path
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
        assert_ne!(root(), root());
    }

    fn idle_job() -> hofund_plugin_api::Job {
        std::sync::Arc::new(|| async {}.boxed())
    }

    #[tokio::test]
    async fn file_store_has_no_plaintext_secret_and_does_not_debug_it() {
        let path = root().join("accounts.json");
        let store = account_store(path.clone());
        let account = Account::from_stored(plugin(), AccountId::from_static("work"), "Work")
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
        assert!(on_disk.contains("os-keychain:v1"));
        let first = store
            .secret(&plugin(), &AccountId::from_static("work"))
            .await
            .expect("secret")
            .expect("present");
        assert_eq!(kernel::expose_secret(&first), "secret-a");
        store
            .upsert(
                Account::from_stored(plugin(), AccountId::from_static("work"), "Work")
                    .expect("account"),
                SecretString::from("secret-b"),
            )
            .await
            .expect("replace");
        let second = store
            .secret(&plugin(), &AccountId::from_static("work"))
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
        let host = test_host(plugin(), root());
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
        let build = |plugin: PluginId| {
            host_context(
                plugin,
                std::sync::Arc::new(super::RecordingHttp::default()),
                std::sync::Arc::new(super::RecordingSecrets::default()),
                std::sync::Arc::new(account_store(root().join("accounts.json"))),
                std::sync::Arc::new(FixedClock::new(SystemTime::UNIX_EPOCH)),
                std::sync::Arc::clone(&scheduler) as std::sync::Arc<dyn SchedulerBackend>,
                std::sync::Arc::new(super::RecordingProcess::default()),
                std::sync::Arc::new(super::RootDirs { root: root() }),
                std::sync::Arc::new(super::RecordingObserve::default()),
            )
        };
        let left = build(plugin_a);
        let right = build(plugin_b);
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
    async fn test_host_seeds_and_records() {
        let host = super::TestHost::open(plugin(), root());
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
                std::sync::Arc::new(super::RootDirs { root: root() }),
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
}
