//! Handles a plugin receives. Construction uses `hofund_plugin_host::host_context`
//! or `hofund_plugin_host::host_context_for`.

#[cfg(feature = "kernel-internal")]
use std::any::Any;
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use crate::SecretString;
use crate::body::{Body, BodyStream};
use crate::error::PluginError;
#[cfg(feature = "kernel")]
use crate::id::PluginKind;
use crate::id::{AccountId, MemberId, PluginId, PluginKey, ProtocolId};
use crate::kernel::{
    AccountBackend, AccountLeaseBackend, ClockBackend, DirBackend, HttpBackend, LoopbackBackend,
    NoLoopback, ObserveBackend, PipedChildBackend, ProcessBackend, SchedulerBackend,
    SecretsBackend,
};
use crate::records::{Account, QuotaObservation, SignInCtx};
use bytes::Bytes;
use futures_util::future::BoxFuture;

pub use crate::kernel::Job;

/// HTTP on the kernel's pooled client.
#[derive(Clone)]
#[non_exhaustive]
pub struct HttpHandle {
    inner: Arc<dyn HttpBackend>,
}

impl HttpHandle {
    /// Sends `request`. The plugin does not build a client or pick a timeout.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the kernel client fails. A success body
    /// streams as it arrives, so a later read failure is an
    /// [`Interrupted`](crate::PluginErrorKind::Interrupted) chunk.
    pub async fn send(
        &self,
        request: http::Request<Body>,
    ) -> Result<http::Response<BodyStream>, PluginError> {
        self.inner.send(request).await
    }

    /// The pooled client behind this handle, when the kernel built one.
    ///
    /// Built-in adapters that have not moved to [`Self::send`] downcast this
    /// value and send on the same client. A recording test handle returns `None`.
    ///
    /// This is removed in row E, the first PR that moves an adapter into its
    /// own plugin crate (E3, Vertex), when built-ins stream through [`Self::send`].
    #[cfg(feature = "kernel-internal")]
    #[doc(hidden)]
    #[must_use]
    pub fn pooled_client(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        self.inner.pooled_client()
    }
}

/// A view of this plugin's secrets. Cloning the handle does not clone a secret.
#[derive(Clone)]
#[non_exhaustive]
pub struct SecretsHandle {
    plugin: PluginKey,
    inner: Arc<dyn SecretsBackend>,
}

impl SecretsHandle {
    /// The current secret for `id` in this context's plugin.
    ///
    /// The id is not global. Another plugin's secret is not returned, even
    /// when the same string is stored for that plugin. Read this on each
    /// call. A refresh is then visible without rebuilding the member. Do not
    /// store the returned value in a `Debug` field.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the store cannot be read.
    pub async fn get(&self, id: &str) -> Result<Option<SecretString>, PluginError> {
        if id.is_empty() {
            return Err(PluginError::config("secret id is empty"));
        }
        self.inner.get(&self.plugin, id).await
    }
}

/// The account store.
#[derive(Clone)]
#[non_exhaustive]
pub struct AccountsHandle {
    plugin: PluginKey,
    inner: Arc<dyn AccountBackend>,
}

impl AccountsHandle {
    /// An account owned by this context's plugin.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when `label` is empty.
    pub fn account(&self, id: AccountId, label: impl Into<String>) -> Result<Account, PluginError> {
        Account::new(self.plugin.clone(), id, label)
    }

    /// Accounts for this context's plugin.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the store cannot be read.
    pub async fn list(&self) -> Result<Vec<Account>, PluginError> {
        self.inner.list(&self.plugin).await
    }

    /// Inserts or replaces `account` and its secret, then drops the cache.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when `account` belongs to another plugin or the write fails.
    pub async fn upsert(&self, account: Account, secret: SecretString) -> Result<(), PluginError> {
        if account.key() != &self.plugin {
            return Err(PluginError::config("account belongs to another plugin"));
        }
        self.inner.upsert(account, secret).await
    }

    /// Removes one account of this context's plugin.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the write fails.
    pub async fn remove(&self, id: &AccountId) -> Result<(), PluginError> {
        self.inner.remove(&self.plugin, id).await
    }

    /// The current secret. This hits the cache until a write drops it.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the store cannot be read.
    pub async fn secret(&self, id: &AccountId) -> Result<Option<SecretString>, PluginError> {
        self.inner.secret(&self.plugin, id).await
    }

    /// Takes the exclusive lease on one account of this context's plugin.
    ///
    /// Use this for a token refresh. Other processes and other tasks that
    /// lease, or upsert, the same account wait until this lease is committed
    /// or dropped. [`AccountLease::secret`] is read after the lease is held,
    /// so a refresh another process just wrote is visible.
    ///
    /// # Errors
    ///
    /// Returns an `Io` [`PluginError`] when `wait` runs out, a `Config` error
    /// when the account is not stored, and an `Unsupported` error when the
    /// store has no leases.
    pub async fn lease(&self, id: &AccountId, wait: Duration) -> Result<AccountLease, PluginError> {
        let inner = self.inner.lease(&self.plugin, id, wait).await?;
        if inner.account().key() != &self.plugin {
            return Err(PluginError::internal(
                "account store leased another plugin's account",
            ));
        }
        Ok(AccountLease { inner })
    }
}

/// An exclusive hold on one account. Dropping it releases the hold without a write.
///
/// It is not `Clone`. [`Debug`] does not print the secret.
#[non_exhaustive]
pub struct AccountLease {
    inner: Box<dyn AccountLeaseBackend>,
}

impl AccountLease {
    /// The account as stored when the lease was taken.
    #[must_use]
    pub fn account(&self) -> &Account {
        self.inner.account()
    }

    /// The secret read after the lease was taken. It is not from the cache.
    #[must_use]
    pub fn secret(&self) -> &SecretString {
        self.inner.secret()
    }

    /// Stores `secret`, and `label` when it is set, then releases the lease.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when `label` is empty or the write fails. The
    /// lease is released either way.
    pub async fn commit(
        self,
        label: Option<String>,
        secret: SecretString,
    ) -> Result<(), PluginError> {
        if label.as_deref().is_some_and(str::is_empty) {
            return Err(PluginError::config("account label is empty"));
        }
        self.inner.commit(label, secret).await
    }
}

impl std::fmt::Debug for AccountLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AccountLease")
            .field("account", self.inner.account())
            .field("secret", &"<redacted>")
            .finish()
    }
}

/// The kernel clock.
#[derive(Clone)]
#[non_exhaustive]
pub struct ClockHandle {
    inner: Arc<dyn ClockBackend>,
}

impl ClockHandle {
    /// The current wall time.
    #[must_use]
    pub fn now(&self) -> SystemTime {
        self.inner.now()
    }

    /// Elapsed time that does not go backwards.
    ///
    /// Use this for deadlines and intervals. [`ClockHandle::now`] is the wall clock.
    #[must_use]
    pub fn monotonic(&self) -> Duration {
        self.inner.monotonic()
    }
}

/// A periodic job. The kernel runs it. Dropping the plugin cancels it on shutdown.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct JobSpec {
    name: String,
    every: Duration,
}

impl JobSpec {
    /// A job named `name` that runs every `every`.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when `name` is empty or `every` is zero.
    pub fn new(name: impl Into<String>, every: Duration) -> Result<Self, PluginError> {
        let name = name.into();
        if name.is_empty() {
            return Err(PluginError::config("job name is empty"));
        }
        if every.is_zero() {
            return Err(PluginError::config("job interval is zero"));
        }
        Ok(Self { name, every })
    }

    /// The job name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// How often the kernel runs the job.
    #[must_use]
    pub const fn every(&self) -> Duration {
        self.every
    }
}

/// An id returned by [`SchedulerHandle::register`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct JobId {
    value: u64,
}

impl JobId {
    fn new(value: u64) -> Self {
        Self { value }
    }

    /// The numeric id.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.value
    }
}

/// Tracked periodic jobs.
#[derive(Clone)]
#[non_exhaustive]
pub struct SchedulerHandle {
    inner: Arc<dyn SchedulerBackend>,
    owned: Arc<Mutex<BTreeSet<u64>>>,
}

impl SchedulerHandle {
    /// Registers `job`. The closure returns a future. It must not spawn a task.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the scheduler rejects the job.
    pub fn register(&self, spec: &JobSpec, job: Job) -> Result<JobId, PluginError> {
        let every_ms = u64::try_from(spec.every.as_millis())
            .map_err(|_| PluginError::config("job interval is too large"))?;
        let id = self.inner.register(&spec.name, every_ms, job)?;
        match self.owned.lock() {
            Ok(mut owned) => {
                owned.insert(id);
                Ok(JobId::new(id))
            }
            Err(_poisoned) => {
                self.inner.cancel(id);
                Err(PluginError::internal("scheduler lock is poisoned"))
            }
        }
    }

    /// Runs `work` once, off the caller. The id cancels it.
    ///
    /// The kernel owns the future: dropping whatever awaited this call does
    /// not cancel it. [`Self::cancel`] and kernel shutdown do. Use it for a
    /// sign-in wait that outlives the request that started it.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when `name` is empty or the scheduler cannot
    /// run the work.
    pub fn run_once(&self, name: &str, work: BoxFuture<'static, ()>) -> Result<JobId, PluginError> {
        if name.is_empty() {
            return Err(PluginError::config("job name is empty"));
        }
        let mut owned = self
            .owned
            .lock()
            .map_err(|_| PluginError::internal("scheduler lock is poisoned"))?;
        let id = self.inner.run_once(name, work)?;
        owned.insert(id);
        Ok(JobId::new(id))
    }

    /// Cancels one job this context registered.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when `id` was not registered here, or the lock is poisoned.
    pub fn cancel(&self, id: JobId) -> Result<(), PluginError> {
        let mut owned = self
            .owned
            .lock()
            .map_err(|_| PluginError::internal("scheduler lock is poisoned"))?;
        if !owned.remove(&id.get()) {
            return Err(PluginError::config(
                "job id was not registered by this host",
            ));
        }
        drop(owned);
        self.inner.cancel(id.get());
        Ok(())
    }
}

/// A process spawn that cannot be a shell command line.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SpawnRequest {
    program: PathBuf,
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
    cwd: PathBuf,
}

impl SpawnRequest {
    /// An absolute `program` with separate `args`. Nothing is joined into a shell string.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when `program` or `cwd` is not absolute.
    pub fn new(
        program: impl Into<PathBuf>,
        args: Vec<OsString>,
        env: Vec<(OsString, OsString)>,
        cwd: impl Into<PathBuf>,
    ) -> Result<Self, PluginError> {
        let program = program.into();
        let cwd = cwd.into();
        if !program.is_absolute() {
            return Err(PluginError::config(
                "spawn program must be an absolute path",
            ));
        }
        if !cwd.is_absolute() {
            return Err(PluginError::config(
                "spawn working directory must be absolute",
            ));
        }
        Ok(Self {
            program,
            args,
            env,
            cwd,
        })
    }

    /// The program path.
    #[must_use]
    pub fn program(&self) -> &Path {
        &self.program
    }

    /// Arguments. They are not parsed by a shell.
    #[must_use]
    pub fn args(&self) -> &[OsString] {
        &self.args
    }

    /// Extra environment variables.
    #[must_use]
    pub fn env(&self) -> &[(OsString, OsString)] {
        &self.env
    }

    /// The working directory.
    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }
}

/// A spawned process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Spawned {
    id: u32,
}

impl Spawned {
    pub(crate) fn new(id: u32) -> Self {
        Self { id }
    }

    /// The process id.
    #[must_use]
    pub const fn id(self) -> u32 {
        self.id
    }
}

/// A child process with piped standard input and output.
///
/// Dropping it kills and reaps the child.
#[non_exhaustive]
pub struct PipedChild {
    inner: Box<dyn PipedChildBackend>,
}

impl std::fmt::Debug for PipedChild {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PipedChild")
            .field("id", &self.inner.id())
            .finish_non_exhaustive()
    }
}

impl PipedChild {
    /// The process id.
    #[must_use]
    pub fn id(&self) -> u32 {
        self.inner.id()
    }

    /// Writes `bytes` to standard input and flushes.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when standard input is closed or the write fails.
    pub async fn write_stdin(&self, bytes: Bytes) -> Result<(), PluginError> {
        self.inner.write_stdin(bytes).await
    }

    /// Closes standard input so the child reads end of file.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when closing fails. Closing twice is not an error.
    pub async fn close_stdin(&self) -> Result<(), PluginError> {
        self.inner.close_stdin().await
    }

    /// Standard output, one chunk per line with the newline kept.
    ///
    /// A line longer than 1 MiB ends the stream with an `Io` error.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when standard output was already taken.
    pub fn stdout(&self) -> Result<BodyStream, PluginError> {
        self.inner
            .take_stdout()
            .ok_or_else(|| PluginError::config("standard output was already taken"))
    }
}

/// Process spawn. The SDK resolves the binary. This handle does not invoke a shell.
#[derive(Clone)]
#[non_exhaustive]
pub struct ProcessHandle {
    inner: Arc<dyn ProcessBackend>,
}

impl ProcessHandle {
    /// Spawns `request`.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the process cannot be started.
    pub async fn spawn(&self, request: &SpawnRequest) -> Result<Spawned, PluginError> {
        let id = self
            .inner
            .spawn(
                request.program.clone(),
                request.args.clone(),
                request.env.clone(),
                request.cwd.clone(),
            )
            .await?;
        Ok(Spawned::new(id))
    }

    /// Spawns `request` with piped standard input and output.
    ///
    /// The child sees only an allowlist of the host environment (`PATH`,
    /// `HOME` or `USERPROFILE`, locale, and proxy variables), then the pairs
    /// in `request`. Standard error is discarded. Output lines are capped at
    /// 1 MiB. Dropping the [`PipedChild`] kills and reaps the child.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the process cannot be started, or with
    /// kind `Unsupported` when the host has no piped spawn.
    pub async fn spawn_piped(&self, request: &SpawnRequest) -> Result<PipedChild, PluginError> {
        let inner = self
            .inner
            .spawn_piped(
                request.program.clone(),
                request.args.clone(),
                request.env.clone(),
                request.cwd.clone(),
            )
            .await?;
        Ok(PipedChild { inner })
    }

    /// Waits for `spawned` and reaps it.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the process is not one this handle spawned,
    /// or the wait fails.
    pub async fn wait(&self, spawned: Spawned) -> Result<i32, PluginError> {
        self.inner.wait(spawned.id()).await
    }

    /// Kills `spawned` and reaps it without waiting on the caller.
    ///
    /// The signal is sent here. The host reaps the child on a background
    /// thread, so this call does not block until the process exits.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the process is not one this handle spawned,
    /// or the kill fails.
    pub fn kill(&self, spawned: Spawned) -> Result<(), PluginError> {
        self.inner.kill(spawned.id())
    }
}

/// Per-plugin directories.
#[derive(Clone)]
#[non_exhaustive]
pub struct DirHandle {
    plugin: PluginKey,
    inner: Arc<dyn DirBackend>,
}

impl DirHandle {
    /// The cache directory for this context's plugin. The kernel creates it.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the directory cannot be created.
    pub fn cache_dir(&self) -> Result<PathBuf, PluginError> {
        self.inner.cache_dir(&self.plugin)
    }

    /// The state directory for this context's plugin. The kernel creates it.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the directory cannot be created.
    pub fn state_dir(&self) -> Result<PathBuf, PluginError> {
        self.inner.state_dir(&self.plugin)
    }
}

/// Quota observations the kernel turns into cooldown in PR K.
#[derive(Clone)]
#[non_exhaustive]
pub struct ObserveHandle {
    plugin: PluginId,
    inner: Arc<dyn ObserveBackend>,
}

impl ObserveHandle {
    /// An observation for this context's plugin.
    #[must_use]
    pub fn observation(&self) -> QuotaObservation {
        QuotaObservation::new(self.plugin.clone())
    }

    /// Records `observation`. This does not edit the router.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when `observation` belongs to another plugin,
    /// or the kernel rejects it.
    pub fn observe(&self, observation: QuotaObservation) -> Result<(), PluginError> {
        if observation.plugin() != &self.plugin {
            return Err(PluginError::config("observation belongs to another plugin"));
        }
        self.inner.observe(observation)
    }
}

/// What `build` and subscription calls receive.
///
/// Clone the handles. That clones an [`Arc`], not a secret. The only
/// constructors outside this crate are `hofund_plugin_host::host_context` and
/// `hofund_plugin_host::host_context_for`.
#[derive(Clone)]
#[non_exhaustive]
pub struct HostContext {
    key: PluginKey,
    http: HttpHandle,
    secrets: SecretsHandle,
    accounts: AccountsHandle,
    clock: ClockHandle,
    scheduler: SchedulerHandle,
    process: ProcessHandle,
    dirs: DirHandle,
    observe: ObserveHandle,
    loopback: LoopbackHandle,
}

impl HostContext {
    /// Kernel-only. Plugins do not call this.
    ///
    /// This compatibility constructor scopes `plugin` to the provider kind.
    /// `plugin` is stored in the handles. Account, secret, directory, and
    /// observation calls use it. A plugin cannot pass a different id.
    #[cfg(feature = "kernel")]
    #[doc(hidden)]
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn from_backends(
        plugin: PluginId,
        http: Arc<dyn HttpBackend>,
        secrets: Arc<dyn SecretsBackend>,
        accounts: Arc<dyn AccountBackend>,
        clock: Arc<dyn ClockBackend>,
        scheduler: Arc<dyn SchedulerBackend>,
        process: Arc<dyn ProcessBackend>,
        dirs: Arc<dyn DirBackend>,
        observe: Arc<dyn ObserveBackend>,
    ) -> Self {
        Self::from_backends_for(
            PluginKey::new(PluginKind::Provider, plugin),
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

    /// Kernel-only constructor for a kind-qualified plugin identity.
    #[cfg(feature = "kernel")]
    #[doc(hidden)]
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn from_backends_for(
        key: PluginKey,
        http: Arc<dyn HttpBackend>,
        secrets: Arc<dyn SecretsBackend>,
        accounts: Arc<dyn AccountBackend>,
        clock: Arc<dyn ClockBackend>,
        scheduler: Arc<dyn SchedulerBackend>,
        process: Arc<dyn ProcessBackend>,
        dirs: Arc<dyn DirBackend>,
        observe: Arc<dyn ObserveBackend>,
    ) -> Self {
        Self {
            http: HttpHandle { inner: http },
            secrets: SecretsHandle {
                plugin: key.clone(),
                inner: secrets,
            },
            accounts: AccountsHandle {
                plugin: key.clone(),
                inner: accounts,
            },
            clock: ClockHandle { inner: clock },
            scheduler: SchedulerHandle {
                inner: scheduler,
                owned: Arc::new(Mutex::new(BTreeSet::new())),
            },
            process: ProcessHandle { inner: process },
            dirs: DirHandle {
                plugin: key.clone(),
                inner: dirs,
            },
            observe: ObserveHandle {
                plugin: key.id().clone(),
                inner: observe,
            },
            loopback: LoopbackHandle {
                plugin: key.clone(),
                inner: Arc::new(NoLoopback),
            },
            key,
        }
    }

    /// Kernel-only. Plugins do not call this.
    ///
    /// Gives this context a loopback callback listener. Without one,
    /// [`LoopbackHandle::bind`] returns [`crate::PluginErrorKind::Unsupported`].
    #[cfg(feature = "kernel")]
    #[doc(hidden)]
    #[must_use]
    pub fn with_loopback(mut self, loopback: Arc<dyn LoopbackBackend>) -> Self {
        self.loopback = LoopbackHandle {
            plugin: self.key.clone(),
            inner: loopback,
        };
        self
    }

    /// The plugin this context was built for.
    #[must_use]
    pub const fn plugin(&self) -> &PluginId {
        self.key.id()
    }

    /// The kind-qualified identity used by host storage.
    #[must_use]
    pub const fn key(&self) -> &PluginKey {
        &self.key
    }

    /// The pooled HTTP client.
    #[must_use]
    pub fn http(&self) -> &HttpHandle {
        &self.http
    }

    /// Secrets. Read them on each call.
    #[must_use]
    pub fn secrets(&self) -> &SecretsHandle {
        &self.secrets
    }

    /// The account store.
    #[must_use]
    pub fn accounts(&self) -> &AccountsHandle {
        &self.accounts
    }

    /// The kernel clock.
    #[must_use]
    pub fn clock(&self) -> &ClockHandle {
        &self.clock
    }

    /// Periodic jobs.
    #[must_use]
    pub fn scheduler(&self) -> &SchedulerHandle {
        &self.scheduler
    }

    /// Process spawn. No shell.
    #[must_use]
    pub fn spawn_process(&self) -> &ProcessHandle {
        &self.process
    }

    /// Per-plugin directories.
    #[must_use]
    pub fn dirs(&self) -> &DirHandle {
        &self.dirs
    }

    /// Quota observations.
    #[must_use]
    pub fn observe(&self) -> &ObserveHandle {
        &self.observe
    }

    /// Loopback callback listener for browser sign-in.
    #[must_use]
    pub fn loopback(&self) -> &LoopbackHandle {
        &self.loopback
    }
}

/// Whether the gateway accepted the inbound request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct AuthResult {
    accepted: bool,
}

impl AuthResult {
    /// Kernel-only. Plugins read [`AuthResult::accepted`] on the [`SurfaceHost`] they are given.
    #[cfg(feature = "kernel")]
    #[doc(hidden)]
    #[must_use]
    pub const fn new(accepted: bool) -> Self {
        Self { accepted }
    }

    /// Whether the gateway accepted the request.
    #[must_use]
    pub const fn accepted(self) -> bool {
        self.accepted
    }
}

/// What an inbound surface may ask the kernel. Constructed in `hofund-plugin-host`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SurfaceHost {
    members: Vec<MemberId>,
    protocols: Vec<ProtocolId>,
    auth: AuthResult,
}

impl SurfaceHost {
    /// Kernel-only. Plugins do not call this.
    #[cfg(feature = "kernel")]
    #[doc(hidden)]
    #[must_use]
    pub fn from_parts(
        members: Vec<MemberId>,
        protocols: Vec<ProtocolId>,
        auth: AuthResult,
    ) -> Self {
        Self {
            members,
            protocols,
            auth,
        }
    }

    /// Member ids on the snapshot this request is using.
    #[must_use]
    pub fn members(&self) -> &[MemberId] {
        &self.members
    }

    /// Codec ids the kernel can speak.
    #[must_use]
    pub fn protocols(&self) -> &[ProtocolId] {
        &self.protocols
    }

    /// The auth result for this request.
    #[must_use]
    pub const fn auth(&self) -> AuthResult {
        self.auth
    }
}

/// The host name a loopback redirect URI names. The listener always binds
/// `127.0.0.1`; this only changes the URI the provider sees.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum RedirectHost {
    /// `http://localhost:<port><path>`, the form most providers register.
    #[default]
    Localhost,
    /// `http://127.0.0.1:<port><path>`.
    Ipv4,
}

/// Why a fixed loopback port could not be bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BindBlocked {
    /// Another process holds the port.
    Busy,
    /// The system refused the port, such as a Windows excluded range.
    Reserved,
}

/// The path a loopback listener treats as the cancel link.
pub const LOOPBACK_CANCEL_PATH: &str = "/cancel";

/// What loopback listener a sign-in wants.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct LoopbackSpec {
    port: Option<u16>,
    path: &'static str,
    redirect_host: RedirectHost,
    state: Option<String>,
}

impl LoopbackSpec {
    /// A listener on `port`, for a provider that registered one redirect URI.
    #[must_use]
    pub const fn fixed(port: u16) -> Self {
        Self {
            port: Some(port),
            path: "/callback",
            redirect_host: RedirectHost::Localhost,
            state: None,
        }
    }

    /// A listener on a port the system picks.
    #[must_use]
    pub const fn ephemeral() -> Self {
        Self {
            port: None,
            path: "/callback",
            redirect_host: RedirectHost::Localhost,
            state: None,
        }
    }

    /// The callback path, such as `/auth/callback`. The default is `/callback`.
    #[must_use]
    pub const fn path(mut self, path: &'static str) -> Self {
        self.path = path;
        self
    }

    /// The host the redirect URI names.
    #[must_use]
    pub const fn redirect_host(mut self, host: RedirectHost) -> Self {
        self.redirect_host = host;
        self
    }

    /// The `state` value the callback must carry. Required.
    #[must_use]
    pub fn expect_state(mut self, state: impl Into<String>) -> Self {
        self.state = Some(state.into());
        self
    }

    /// The fixed port, or `None` for an ephemeral one.
    #[must_use]
    pub const fn requested_port(&self) -> Option<u16> {
        self.port
    }

    /// The callback path.
    #[must_use]
    pub const fn callback_path(&self) -> &'static str {
        self.path
    }

    /// The host the redirect URI names.
    #[must_use]
    pub const fn host(&self) -> RedirectHost {
        self.redirect_host
    }

    /// The expected `state`.
    #[must_use]
    pub fn state(&self) -> Option<&str> {
        self.state.as_deref()
    }

    /// Checks the path and state before a backend sees the spec.
    fn validate(&self) -> Result<(), PluginError> {
        let path = self.path;
        if !path.starts_with('/')
            || path == LOOPBACK_CANCEL_PATH
            || !path
                .bytes()
                .all(|byte| byte.is_ascii_graphic() && byte != b'?' && byte != b'#' && byte != b'%')
        {
            return Err(PluginError::config(
                "loopback callback path must be a plain absolute path other than /cancel",
            ));
        }
        match self.state.as_deref() {
            Some(state) if !state.is_empty() => Ok(()),
            _ => Err(PluginError::config(
                "loopback sign-in needs an expected state",
            )),
        }
    }
}

/// A loopback listener bound for one sign-in.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct LoopbackBinding {
    redirect_uri: String,
    port: u16,
    blocked: Option<BindBlocked>,
}

impl LoopbackBinding {
    /// Kernel-only. Plugins do not call this.
    #[cfg(feature = "kernel")]
    #[doc(hidden)]
    #[must_use]
    pub fn from_parts(redirect_uri: String, port: u16, blocked: Option<BindBlocked>) -> Self {
        Self {
            redirect_uri,
            port,
            blocked,
        }
    }

    /// The redirect URI to send in the authorize URL.
    #[must_use]
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// The port in the redirect URI.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// Set when the port could not be bound. The sign-in then completes only
    /// from a pasted address.
    #[must_use]
    pub const fn blocked(&self) -> Option<BindBlocked> {
        self.blocked
    }
}

/// Loopback callback listener for browser sign-in. The kernel awaits the
/// callback and hands the result to `complete_sign_in`.
#[derive(Clone)]
#[non_exhaustive]
pub struct LoopbackHandle {
    plugin: PluginKey,
    inner: Arc<dyn LoopbackBackend>,
}

impl LoopbackHandle {
    /// Binds a listener for the sign-in in `ctx`.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when `ctx` has no session id or belongs to
    /// another plugin, the spec has no state or a bad path, the session
    /// already has a listener, or this host has none.
    pub fn bind(
        &self,
        ctx: &SignInCtx,
        spec: &LoopbackSpec,
    ) -> Result<LoopbackBinding, PluginError> {
        if ctx.session().is_empty() {
            return Err(PluginError::config("loopback sign-in needs a session id"));
        }
        if ctx.plugin() != self.plugin.id() {
            return Err(PluginError::config(
                "loopback sign-in context belongs to another plugin",
            ));
        }
        spec.validate()?;
        self.inner.bind(&self.plugin, ctx.session(), spec)
    }

    /// Closes the listener for the sign-in in `ctx`, if any.
    pub fn release(&self, ctx: &SignInCtx) {
        if !ctx.session().is_empty() {
            self.inner.release(ctx.session());
        }
    }
}

impl std::fmt::Debug for LoopbackHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LoopbackHandle")
            .field("plugin", &self.plugin)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use std::ffi::OsString;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use bytes::Bytes;

    use super::{PipedChild, ProcessHandle, SpawnRequest};
    use crate::body::BodyStream;
    use crate::error::{PluginError, PluginErrorKind};
    use crate::kernel::{PipedChildBackend, ProcessBackend};

    struct NoPipes;

    #[async_trait]
    impl ProcessBackend for NoPipes {
        async fn spawn(
            &self,
            _program: PathBuf,
            _args: Vec<OsString>,
            _env: Vec<(OsString, OsString)>,
            _cwd: PathBuf,
        ) -> Result<u32, PluginError> {
            Ok(1)
        }

        async fn wait(&self, _id: u32) -> Result<i32, PluginError> {
            Ok(0)
        }

        fn kill(&self, _id: u32) -> Result<(), PluginError> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakePipe {
        written: Arc<Mutex<Vec<u8>>>,
        closed: Arc<Mutex<bool>>,
        stdout: Mutex<Option<BodyStream>>,
    }

    #[async_trait]
    impl PipedChildBackend for FakePipe {
        fn id(&self) -> u32 {
            42
        }

        async fn write_stdin(&self, bytes: Bytes) -> Result<(), PluginError> {
            self.written
                .lock()
                .expect("written")
                .extend_from_slice(&bytes);
            Ok(())
        }

        async fn close_stdin(&self) -> Result<(), PluginError> {
            *self.closed.lock().expect("closed") = true;
            Ok(())
        }

        fn take_stdout(&self) -> Option<BodyStream> {
            self.stdout.lock().expect("stdout").take()
        }
    }

    #[tokio::test]
    async fn the_default_backend_has_no_piped_spawn() {
        let handle = ProcessHandle {
            inner: Arc::new(NoPipes),
        };
        let program = if cfg!(windows) {
            r"C:\Tools\agent.exe"
        } else {
            "/usr/bin/agent"
        };
        let cwd = if cfg!(windows) { r"C:\Tools" } else { "/tmp" };
        let request = SpawnRequest::new(program, Vec::new(), Vec::new(), cwd).expect("request");
        let error = handle.spawn_piped(&request).await.expect_err("unsupported");
        assert_eq!(error.kind(), PluginErrorKind::Unsupported);
    }

    #[tokio::test]
    async fn piped_child_delegates_and_hands_out_stdout_once() {
        let pipe = FakePipe {
            stdout: Mutex::new(Some(BodyStream::from_bytes("line\n"))),
            ..FakePipe::default()
        };
        let written = Arc::clone(&pipe.written);
        let closed = Arc::clone(&pipe.closed);
        let child = PipedChild {
            inner: Box::new(pipe),
        };
        assert_eq!(child.id(), 42);
        assert_eq!(format!("{child:?}"), "PipedChild { id: 42, .. }");
        child
            .write_stdin(Bytes::from_static(b"hello"))
            .await
            .expect("write");
        assert_eq!(*written.lock().expect("written"), b"hello");
        child.close_stdin().await.expect("close");
        assert!(*closed.lock().expect("closed"));
        let mut stdout = child.stdout().expect("first stdout");
        let chunk = stdout.next_chunk().await.expect("a chunk").expect("ok");
        assert_eq!(&chunk[..], b"line\n");
        let error = child.stdout().expect_err("second stdout");
        assert_eq!(error.kind(), PluginErrorKind::Config);
    }

    #[test]
    fn spawn_rejects_a_relative_program() {
        let error = SpawnRequest::new("example-agent", Vec::new(), Vec::new(), "/tmp")
            .expect_err("relative");
        assert_eq!(error.kind(), crate::error::PluginErrorKind::Config);
        let program = if cfg!(windows) {
            PathBuf::from(r"C:\Tools\example-agent.exe")
        } else {
            PathBuf::from("/usr/bin/example-agent")
        };
        let cwd = if cfg!(windows) {
            PathBuf::from(r"C:\Tools")
        } else {
            PathBuf::from("/tmp")
        };
        assert!(SpawnRequest::new(program, Vec::new(), Vec::new(), cwd).is_ok());
    }

    mod loopback {
        use std::sync::{Arc, Mutex};

        use super::super::{
            BindBlocked, LoopbackBinding, LoopbackHandle, LoopbackSpec, RedirectHost,
        };
        use crate::error::{PluginError, PluginErrorKind};
        use crate::id::{PluginId, PluginKey, PluginKind};
        use crate::kernel::{LoopbackBackend, NoLoopback};
        use crate::records::SignInCtx;

        const SESSION: &str = "0123456789abcdef0123456789abcdef";

        #[derive(Default)]
        struct Fake {
            calls: Mutex<Vec<String>>,
        }

        impl LoopbackBackend for Fake {
            fn bind(
                &self,
                plugin: &PluginKey,
                session: &str,
                spec: &LoopbackSpec,
            ) -> Result<LoopbackBinding, PluginError> {
                self.calls.lock().expect("calls").push(format!(
                    "bind {} {session} {:?} {} {:?} {:?}",
                    plugin.id(),
                    spec.requested_port(),
                    spec.callback_path(),
                    spec.host(),
                    spec.state()
                ));
                Ok(LoopbackBinding {
                    redirect_uri: "http://localhost:1455/auth/callback".to_owned(),
                    port: 1455,
                    blocked: Some(BindBlocked::Busy),
                })
            }

            fn release(&self, session: &str) {
                self.calls
                    .lock()
                    .expect("calls")
                    .push(format!("release {session}"));
            }
        }

        fn handle(inner: Arc<dyn LoopbackBackend>) -> LoopbackHandle {
            LoopbackHandle {
                plugin: PluginKey::new(PluginKind::Subscription, PluginId::from_static("codex")),
                inner,
            }
        }

        fn ctx() -> SignInCtx {
            SignInCtx::new(PluginId::from_static("codex"))
                .with_session(SESSION)
                .expect("session")
        }

        #[test]
        fn bind_passes_the_session_and_spec_to_the_backend() {
            let fake = Arc::new(Fake::default());
            let handle = handle(Arc::clone(&fake) as Arc<dyn LoopbackBackend>);
            let binding = handle
                .bind(
                    &ctx(),
                    &LoopbackSpec::fixed(1455)
                        .path("/auth/callback")
                        .redirect_host(RedirectHost::Ipv4)
                        .expect_state("s"),
                )
                .expect("bind");
            assert_eq!(
                binding.redirect_uri(),
                "http://localhost:1455/auth/callback"
            );
            assert_eq!(binding.port(), 1455);
            assert_eq!(binding.blocked(), Some(BindBlocked::Busy));
            handle.release(&ctx());
            handle.release(&SignInCtx::new(PluginId::from_static("codex")));
            assert_eq!(
                *fake.calls.lock().expect("calls"),
                vec![
                    format!("bind codex {SESSION} Some(1455) /auth/callback Ipv4 Some(\"s\")"),
                    format!("release {SESSION}"),
                ]
            );
            let ephemeral = LoopbackSpec::ephemeral();
            assert_eq!(ephemeral.requested_port(), None);
            assert_eq!(ephemeral.callback_path(), "/callback");
            assert_eq!(ephemeral.host(), RedirectHost::Localhost);
        }

        #[test]
        fn bind_rejects_a_bad_context_or_spec_before_the_backend() {
            let fake = Arc::new(Fake::default());
            let handle = handle(Arc::clone(&fake) as Arc<dyn LoopbackBackend>);
            let state = |spec: LoopbackSpec| spec.expect_state("s");
            let cases = [
                (
                    SignInCtx::new(PluginId::from_static("codex")),
                    state(LoopbackSpec::ephemeral()),
                ),
                (
                    SignInCtx::new(PluginId::from_static("gemini"))
                        .with_session(SESSION)
                        .expect("session"),
                    state(LoopbackSpec::ephemeral()),
                ),
                (ctx(), LoopbackSpec::ephemeral()),
                (ctx(), LoopbackSpec::ephemeral().expect_state("")),
                (ctx(), state(LoopbackSpec::ephemeral().path("callback"))),
                (ctx(), state(LoopbackSpec::ephemeral().path("/cancel"))),
                (ctx(), state(LoopbackSpec::ephemeral().path("/cb?x=1"))),
                (ctx(), state(LoopbackSpec::ephemeral().path("/cb#x"))),
                (ctx(), state(LoopbackSpec::ephemeral().path("/c b"))),
                (ctx(), state(LoopbackSpec::ephemeral().path("/c%2f"))),
            ];
            for (ctx, spec) in cases {
                let error = handle.bind(&ctx, &spec).expect_err("rejected");
                assert_eq!(error.kind(), PluginErrorKind::Config, "{spec:?}");
            }
            assert!(fake.calls.lock().expect("calls").is_empty());
        }

        #[test]
        fn a_host_without_a_listener_is_unsupported() {
            let handle = handle(Arc::new(NoLoopback));
            let error = handle
                .bind(&ctx(), &LoopbackSpec::ephemeral().expect_state("s"))
                .expect_err("unsupported");
            assert_eq!(error.kind(), PluginErrorKind::Unsupported);
            handle.release(&ctx());
            assert!(format!("{handle:?}").contains("codex"));
        }
    }
}
