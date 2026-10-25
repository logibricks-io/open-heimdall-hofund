//! Kernel backends stored inside the handles.
//!
//! These traits are `doc(hidden)` because a plugin is not supposed to implement
//! them. They live in this crate so [`crate::HostContext`] can hold the trait
//! objects: another crate cannot fill a private field. The constructor the
//! kernel calls is `hofund_plugin_host::host_context`. `xtask` rejects
//! `from_backends` outside that crate.

use std::path::PathBuf;
use std::time::SystemTime;

use async_trait::async_trait;

use crate::body::{Body, BodyStream};
use crate::error::PluginError;
use crate::id::{AccountId, PluginId};
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
    async fn send(
        &self,
        request: HttpRequest<Body>,
    ) -> Result<HttpResponse<BodyStream>, PluginError>;
}

/// Reads a secret by id. The handle does not cache the secret itself.
#[async_trait]
#[doc(hidden)]
pub trait SecretsBackend: Send + Sync {
    /// The current secret, or `None` when it is unset.
    async fn get(&self, id: &str) -> Result<Option<SecretString>, PluginError>;
}

/// The account store. Writes take a cross-process lock and drop the cache.
#[async_trait]
#[doc(hidden)]
pub trait AccountBackend: Send + Sync {
    /// Accounts stored for `plugin`.
    async fn list(&self, plugin: &PluginId) -> Result<Vec<Account>, PluginError>;

    /// Inserts or replaces `account` and its secret.
    async fn upsert(&self, account: Account, secret: SecretString) -> Result<(), PluginError>;

    /// Removes one account.
    async fn remove(&self, plugin: &PluginId, id: &AccountId) -> Result<(), PluginError>;

    /// The current secret, from the cache until a write drops it.
    async fn secret(
        &self,
        plugin: &PluginId,
        id: &AccountId,
    ) -> Result<Option<SecretString>, PluginError>;
}

/// The kernel clock. Plugins do not call [`SystemTime::now`](SystemTime::now).
#[doc(hidden)]
pub trait ClockBackend: Send + Sync {
    /// The current time.
    fn now(&self) -> SystemTime;
}

/// A job the scheduler will run. The kernel runs it. The plugin does not spawn.
#[doc(hidden)]
pub trait SchedulerBackend: Send + Sync {
    /// Registers `job` and returns an id the plugin can cancel.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the backend rejects the job.
    fn register(
        &self,
        name: &str,
        every_ms: u64,
        job: std::sync::Arc<dyn Fn() + Send + Sync>,
    ) -> Result<u64, PluginError>;

    /// Cancels a job. Unknown ids are not an error.
    fn cancel(&self, id: u64);

    /// Cancels every job. Called from plugin shutdown.
    fn shutdown(&self);
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
}

/// Per-plugin directories the kernel creates.
#[doc(hidden)]
pub trait DirBackend: Send + Sync {
    /// The cache directory for `plugin`.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the directory cannot be created.
    fn cache_dir(&self, plugin: &PluginId) -> Result<PathBuf, PluginError>;

    /// The state directory for `plugin`.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the directory cannot be created.
    fn state_dir(&self, plugin: &PluginId) -> Result<PathBuf, PluginError>;
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
