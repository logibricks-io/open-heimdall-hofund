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
    AccountBackend, ClockBackend, DirBackend, HttpBackend, ObserveBackend, ProcessBackend,
    SchedulerBackend, SecretsBackend,
};
use crate::records::{Account, QuotaObservation};

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
    /// Returns [`PluginError`] when the kernel client fails.
    pub async fn send(
        &self,
        request: http::Request<Body>,
    ) -> Result<http::Response<BodyStream>, PluginError> {
        self.inner.send(request).await
    }

    /// The pooled client behind this handle, when the kernel built one.
    ///
    /// Built-in adapters still stream. [`Self::send`] buffers the body, so
    /// those adapters downcast this value and send on the same client.
    /// A recording test handle returns `None`.
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
            key,
        }
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::SpawnRequest;

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
}
