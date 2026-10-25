//! Handles a plugin receives. Construction is `hofund_plugin_host::host_context`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::SecretString;
use crate::body::{Body, BodyStream};
use crate::error::PluginError;
use crate::id::{AccountId, MemberId, PluginId, ProtocolId};
use crate::kernel::{
    AccountBackend, ClockBackend, DirBackend, HttpBackend, ObserveBackend, ProcessBackend,
    SchedulerBackend, SecretsBackend,
};
use crate::records::{Account, QuotaObservation};

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
}

/// A view of the secret store. Cloning the handle does not clone a secret.
#[derive(Clone)]
#[non_exhaustive]
pub struct SecretsHandle {
    inner: Arc<dyn SecretsBackend>,
}

impl SecretsHandle {
    /// The current secret for `id`.
    ///
    /// Read this on each call. A refresh is then visible without rebuilding
    /// the member. Do not store the returned value in a `Debug` field.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the store cannot be read.
    pub async fn get(&self, id: &str) -> Result<Option<SecretString>, PluginError> {
        if id.is_empty() {
            return Err(PluginError::config("secret id is empty"));
        }
        self.inner.get(id).await
    }
}

/// The account store.
#[derive(Clone)]
#[non_exhaustive]
pub struct AccountsHandle {
    inner: Arc<dyn AccountBackend>,
}

impl AccountsHandle {
    /// Accounts for `plugin`.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the store cannot be read.
    pub async fn list(&self, plugin: &PluginId) -> Result<Vec<Account>, PluginError> {
        self.inner.list(plugin).await
    }

    /// Inserts or replaces `account` and its secret, then drops the cache.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the write fails.
    pub async fn upsert(&self, account: Account, secret: SecretString) -> Result<(), PluginError> {
        self.inner.upsert(account, secret).await
    }

    /// Removes one account.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the write fails.
    pub async fn remove(&self, plugin: &PluginId, id: &AccountId) -> Result<(), PluginError> {
        self.inner.remove(plugin, id).await
    }

    /// The current secret. This hits the cache until a write drops it.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the store cannot be read.
    pub async fn secret(
        &self,
        plugin: &PluginId,
        id: &AccountId,
    ) -> Result<Option<SecretString>, PluginError> {
        self.inner.secret(plugin, id).await
    }
}

/// The kernel clock.
#[derive(Clone)]
#[non_exhaustive]
pub struct ClockHandle {
    inner: Arc<dyn ClockBackend>,
}

impl ClockHandle {
    /// The current time.
    #[must_use]
    pub fn now(&self) -> SystemTime {
        self.inner.now()
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
}

impl SchedulerHandle {
    /// Registers `job`. The closure must not spawn its own task.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the scheduler rejects the job.
    pub fn register(
        &self,
        spec: &JobSpec,
        job: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<JobId, PluginError> {
        let every_ms = u64::try_from(spec.every.as_millis())
            .map_err(|_| PluginError::config("job interval is too large"))?;
        let id = self.inner.register(&spec.name, every_ms, job)?;
        Ok(JobId::new(id))
    }

    /// Cancels one job. An unknown id does nothing.
    pub fn cancel(&self, id: JobId) {
        self.inner.cancel(id.get());
    }

    /// Cancels every job this scheduler is holding.
    pub fn shutdown(&self) {
        self.inner.shutdown();
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
}

/// Per-plugin directories.
#[derive(Clone)]
#[non_exhaustive]
pub struct DirHandle {
    inner: Arc<dyn DirBackend>,
}

impl DirHandle {
    /// The cache directory for `plugin`. The kernel creates it.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the directory cannot be created.
    pub fn cache_dir(&self, plugin: &PluginId) -> Result<PathBuf, PluginError> {
        self.inner.cache_dir(plugin)
    }

    /// The state directory for `plugin`. The kernel creates it.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the directory cannot be created.
    pub fn state_dir(&self, plugin: &PluginId) -> Result<PathBuf, PluginError> {
        self.inner.state_dir(plugin)
    }
}

/// Quota observations the kernel turns into cooldown in PR K.
#[derive(Clone)]
#[non_exhaustive]
pub struct ObserveHandle {
    inner: Arc<dyn ObserveBackend>,
}

impl ObserveHandle {
    /// Records `observation`. This does not edit the router.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the kernel rejects the observation.
    pub fn observe(&self, observation: QuotaObservation) -> Result<(), PluginError> {
        self.inner.observe(observation)
    }
}

/// What `build` and subscription calls receive.
///
/// Clone the handles. That clones an [`Arc`], not a secret. The only
/// constructor outside this crate is `hofund_plugin_host::host_context`.
#[derive(Clone)]
#[non_exhaustive]
pub struct HostContext {
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
    #[doc(hidden)]
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn from_backends(
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
            secrets: SecretsHandle { inner: secrets },
            accounts: AccountsHandle { inner: accounts },
            clock: ClockHandle { inner: clock },
            scheduler: SchedulerHandle { inner: scheduler },
            process: ProcessHandle { inner: process },
            dirs: DirHandle { inner: dirs },
            observe: ObserveHandle { inner: observe },
        }
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
    /// An auth result.
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
        let error =
            SpawnRequest::new("claude", Vec::new(), Vec::new(), "/tmp").expect_err("relative");
        assert_eq!(error.kind(), crate::error::PluginErrorKind::Config);
        let program = if cfg!(windows) {
            PathBuf::from(r"C:\Tools\claude.exe")
        } else {
            PathBuf::from("/usr/bin/claude")
        };
        let cwd = if cfg!(windows) {
            PathBuf::from(r"C:\Tools")
        } else {
            PathBuf::from("/tmp")
        };
        assert!(SpawnRequest::new(program, Vec::new(), Vec::new(), cwd).is_ok());
    }
}
