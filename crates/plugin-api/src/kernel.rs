#![cfg_attr(not(feature = "kernel"), allow(dead_code, unreachable_pub))]

//! Kernel backends stored inside the handles.
//!
//! These traits are `doc(hidden)` because a plugin is not supposed to implement
//! them. They live in this crate so [`crate::HostContext`] can hold the trait
//! objects: another crate cannot fill a private field. The constructor the
//! kernel calls is `hofund_plugin_host::host_context`. `xtask` rejects
//! `from_backends` outside that crate.

#[cfg(feature = "kernel-internal")]
use std::any::Any;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use futures_util::future::BoxFuture;

use crate::body::{Body, BodyStream};
use crate::error::PluginError;
use crate::id::{AccountId, PluginKey};
use crate::records::{Account, QuotaObservation};
use secrecy::{ExposeSecret, SecretString};

/// HTTP request type used by [`HttpBackend`]. Re-exported so the host crate
/// does not depend on `http` itself.
#[doc(hidden)]
pub use http::Request as HttpRequest;

/// HTTP response type used by [`HttpBackend`].
#[doc(hidden)]
pub use http::Response as HttpResponse;

/// The secret text. Do not log the result.
#[doc(hidden)]
#[must_use]
pub fn expose_secret(secret: &SecretString) -> &str {
    secret.expose_secret()
}

/// Sends one HTTP call on the pooled client.
#[async_trait]
#[doc(hidden)]
pub trait HttpBackend: Send + Sync {
    /// Sends `request` and returns the response stream.
    ///
    /// A success body streams as the upstream sends it; a read failure is an
    /// [`Interrupted`](crate::PluginErrorKind::Interrupted) chunk.
    async fn send(
        &self,
        request: HttpRequest<Body>,
    ) -> Result<HttpResponse<BodyStream>, PluginError>;

    /// The process-wide `reqwest` client, when this backend is the pool.
    ///
    /// The default is `None`. Streaming adapters downcast the value so they
    /// use the same client as [`HttpBackend::send`], including its connect
    /// and idle-read limits.
    #[cfg(feature = "kernel-internal")]
    #[must_use]
    fn pooled_client(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        None
    }
}

/// Reads a secret for one plugin. The handle does not cache the secret itself.
#[async_trait]
#[doc(hidden)]
pub trait SecretsBackend: Send + Sync {
    /// The current secret for `plugin` and `id`, or `None` when it is unset.
    ///
    /// A secret stored for a different plugin is not returned.
    async fn get(&self, plugin: &PluginKey, id: &str) -> Result<Option<SecretString>, PluginError>;
}

/// The account store. Writes take a cross-process lock and drop the cache.
#[async_trait]
#[doc(hidden)]
pub trait AccountBackend: Send + Sync {
    /// Accounts stored for `plugin`.
    async fn list(&self, plugin: &PluginKey) -> Result<Vec<Account>, PluginError>;

    /// Inserts or replaces `account` and its secret.
    async fn upsert(&self, account: Account, secret: SecretString) -> Result<(), PluginError>;

    /// Removes one account.
    async fn remove(&self, plugin: &PluginKey, id: &AccountId) -> Result<(), PluginError>;

    /// The current secret, from the cache until a write drops it.
    async fn secret(
        &self,
        plugin: &PluginKey,
        id: &AccountId,
    ) -> Result<Option<SecretString>, PluginError>;

    /// Takes the exclusive lease on one account, waiting up to `wait`.
    ///
    /// The lease is held across processes until the returned value is
    /// committed or dropped. The secret it carries is read after the lease is
    /// held and does not come from a cache.
    ///
    /// # Errors
    ///
    /// The default returns an `Unsupported` error. A store that implements
    /// leases returns an `Io` error when `wait` runs out, and a `Config`
    /// error when the account is not stored.
    async fn lease(
        &self,
        plugin: &PluginKey,
        id: &AccountId,
        wait: Duration,
    ) -> Result<Box<dyn AccountLeaseBackend>, PluginError> {
        let _ = (plugin, id, wait);
        Err(PluginError::unsupported("this account store has no leases"))
    }
}

/// One held account lease. Dropping it releases the lease without a write.
#[async_trait]
#[doc(hidden)]
pub trait AccountLeaseBackend: Send + Sync {
    /// The leased account as stored when the lease was taken.
    fn account(&self) -> &Account;

    /// The secret read after the lease was taken.
    fn secret(&self) -> &SecretString;

    /// Writes `secret`, and `label` when it is set, then releases the lease.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the write fails. The lease is released either way.
    async fn commit(
        self: Box<Self>,
        label: Option<String>,
        secret: SecretString,
    ) -> Result<(), PluginError>;
}

/// The kernel clock. Plugins do not call [`SystemTime::now`](SystemTime::now).
#[doc(hidden)]
pub trait ClockBackend: Send + Sync {
    /// The current wall time.
    fn now(&self) -> SystemTime;

    /// Time that does not go backwards for the life of this clock.
    fn monotonic(&self) -> Duration;
}

/// Work the kernel awaits.
///
/// The future is `'static` so the scheduler can hold it. The plugin does not
/// spawn a task to get there: list refresh and quota polling return this future.
pub type Job = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;

/// A job the scheduler will run. The kernel runs it. The plugin does not spawn.
#[doc(hidden)]
pub trait SchedulerBackend: Send + Sync {
    /// Registers `job` and returns an id the plugin can cancel.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the backend rejects the job.
    fn register(&self, name: &str, every_ms: u64, job: Job) -> Result<u64, PluginError>;

    /// Cancels one job. Unknown ids are not an error.
    ///
    /// The plugin-facing handle rejects an id it did not register. This method
    /// is the kernel side and does not know which context registered the id.
    fn cancel(&self, id: u64);

    /// Cancels every job. Kernel shutdown calls this. It is not on [`crate::SchedulerHandle`].
    ///
    /// One-shot work from [`SchedulerBackend::run_once`] is cancelled too.
    fn shutdown(&self);

    /// Runs `work` once and returns an id that [`SchedulerBackend::cancel`] accepts.
    ///
    /// The backend owns the future. Nothing the caller drops cancels it; only
    /// `cancel` and `shutdown` do.
    ///
    /// # Errors
    ///
    /// The default returns an `Unsupported` error. A backend returns an error
    /// when it cannot run work, for example outside a runtime.
    fn run_once(&self, name: &str, work: BoxFuture<'static, ()>) -> Result<u64, PluginError> {
        let _ = (name, work);
        Err(PluginError::unsupported(
            "this scheduler has no one-shot jobs",
        ))
    }
}

/// Spawns a process without a shell.
#[async_trait]
#[doc(hidden)]
pub trait ProcessBackend: Send + Sync {
    /// Spawns `program`. `program` is already an absolute path.
    async fn spawn(
        &self,
        program: PathBuf,
        args: Vec<std::ffi::OsString>,
        env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
        cwd: PathBuf,
    ) -> Result<u32, PluginError>;

    /// Waits for process `id` and reaps it.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when `id` is not a child this backend spawned,
    /// or the wait fails.
    async fn wait(&self, id: u32) -> Result<i32, PluginError>;

    /// Kills process `id` and reaps it without blocking the caller.
    ///
    /// Send the signal and return. Reaping happens off the caller so a stuck
    /// child cannot stall the runtime. [`ProcessBackend::wait`] is the call
    /// that reports the exit code.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when `id` is not a child this backend spawned,
    /// or the kill fails.
    fn kill(&self, id: u32) -> Result<(), PluginError>;
}

/// Per-plugin directories the kernel creates.
#[doc(hidden)]
pub trait DirBackend: Send + Sync {
    /// The cache directory for `plugin`.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the directory cannot be created.
    fn cache_dir(&self, plugin: &PluginKey) -> Result<PathBuf, PluginError>;

    /// The state directory for `plugin`.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the directory cannot be created.
    fn state_dir(&self, plugin: &PluginKey) -> Result<PathBuf, PluginError>;
}

/// Accepts a quota observation. The plugin does not edit router state.
#[doc(hidden)]
pub trait ObserveBackend: Send + Sync {
    /// Records `observation`.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the kernel rejects it.
    fn observe(&self, observation: QuotaObservation) -> Result<(), PluginError>;
}
