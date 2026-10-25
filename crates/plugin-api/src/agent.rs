//! Agent descriptions, the plan the engine applies, and the gateway target.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::{BuildError, ConfigError, RawTable, Schema, ValidConfig};
use crate::error::PluginError;
use crate::host::HostContext;
use crate::id::{PluginId, ProtocolId};
use secrecy::{ExposeSecret, SecretString};

use crate::records::AgentDescription;

/// Where the local gateway is listening, and the bearer token clients send.
///
/// [`Debug`] redacts the token. [`GatewayTarget::token`] returns the raw text
/// the engine writes into an agent file. Do not log that string.
#[derive(Clone)]
#[non_exhaustive]
pub struct GatewayTarget {
    base_url: String,
    token: SecretString,
    models: Vec<String>,
}

impl GatewayTarget {
    /// A target. `base_url` is the listener origin. `token` is the bearer token.
    #[must_use]
    pub fn new(base_url: impl Into<String>, token: impl Into<String>, models: Vec<String>) -> Self {
        Self {
            base_url: base_url.into(),
            token: token.into().into(),
            models,
        }
    }

    /// Replaces the token. The URL and models stay.
    #[must_use]
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = token.into().into();
        self
    }

    /// Replaces the route model ids.
    #[must_use]
    pub fn with_models(mut self, models: Vec<String>) -> Self {
        self.models = models;
        self
    }

    /// Origin such as `http://127.0.0.1:3425`. A trailing slash is ignored by the engine.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The bearer token. Do not log this.
    #[must_use]
    pub fn token(&self) -> &str {
        self.token.expose_secret()
    }

    /// Route model ids, in file order.
    #[must_use]
    pub fn models(&self) -> &[String] {
        &self.models
    }
}

impl std::fmt::Debug for GatewayTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GatewayTarget")
            .field("base_url", &self.base_url)
            .field("token", &self.token)
            .field("models", &self.models)
            .finish()
    }
}

impl PartialEq for GatewayTarget {
    fn eq(&self, other: &Self) -> bool {
        self.base_url == other.base_url
            && self.token.expose_secret() == other.token.expose_secret()
            && self.models == other.models
    }
}

impl Eq for GatewayTarget {}

/// One file in a [`Plan`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct FileChange {
    path: PathBuf,
    before: Vec<u8>,
    after: Vec<u8>,
    diff: String,
}

impl FileChange {
    /// A change. `before` is empty when the file does not exist yet.
    #[must_use]
    pub fn new(
        path: impl Into<PathBuf>,
        before: Vec<u8>,
        after: Vec<u8>,
        diff: impl Into<String>,
    ) -> Self {
        Self {
            path: path.into(),
            before,
            after,
            diff: diff.into(),
        }
    }

    /// Path that would be replaced.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Bytes before the edit.
    #[must_use]
    pub fn before(&self) -> &[u8] {
        &self.before
    }

    /// Bytes after the edit.
    #[must_use]
    pub fn after(&self) -> &[u8] {
        &self.after
    }

    /// A line diff of `before` and `after`.
    #[must_use]
    pub fn diff(&self) -> &str {
        &self.diff
    }
}

/// A planned edit that has not been written unless the engine applies it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Plan {
    changes: Vec<FileChange>,
}

impl Plan {
    /// A plan. Already-matching files are omitted by the caller.
    #[must_use]
    pub fn new(changes: Vec<FileChange>) -> Self {
        Self { changes }
    }

    /// Files whose bytes would change.
    #[must_use]
    pub fn changes(&self) -> &[FileChange] {
        &self.changes
    }

    /// Whether the plan writes nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }
}

/// What [`crate::AgentIntegration::custom_write`] did not need the engine to undo.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct DisconnectOutcome {
    changed: bool,
    warnings: Vec<String>,
}

impl DisconnectOutcome {
    /// The outcome of a disconnect.
    #[must_use]
    pub fn new(changed: bool, warnings: Vec<String>) -> Self {
        Self { changed, warnings }
    }

    /// Whether a backed-up file was merged or replaced.
    #[must_use]
    pub const fn changed(&self) -> bool {
        self.changed
    }

    /// Keys the user changed after connect. Those values were left in place.
    #[must_use]
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }
}

/// Whether an agent is installed and whether its files already match a target.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ConnectionStatus {
    id: &'static str,
    display_name: &'static str,
    installed: bool,
    connected: bool,
    paths: Vec<PathBuf>,
}

impl ConnectionStatus {
    /// A status row.
    #[must_use]
    pub fn new(
        id: &'static str,
        display_name: &'static str,
        installed: bool,
        connected: bool,
        paths: Vec<PathBuf>,
    ) -> Self {
        Self {
            id,
            display_name,
            installed,
            connected,
            paths,
        }
    }

    /// Stable id, such as `claude-code`.
    #[must_use]
    pub const fn id(&self) -> &'static str {
        self.id
    }

    /// Name printed by the CLI.
    #[must_use]
    pub const fn display_name(&self) -> &'static str {
        self.display_name
    }

    /// The binary is on `PATH`, or a managed config file already exists.
    #[must_use]
    pub const fn installed(&self) -> bool {
        self.installed
    }

    /// Every declared key equals the target.
    #[must_use]
    pub const fn connected(&self) -> bool {
        self.connected
    }

    /// Config paths this adapter would read or write.
    #[must_use]
    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }
}

/// Context for a custom write. The engine fills this. The API does not read the home directory.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct WriteCtx {
    home: PathBuf,
}

impl WriteCtx {
    /// A write rooted at `home`.
    #[must_use]
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }

    /// The home directory.
    #[must_use]
    pub fn home(&self) -> &Path {
        &self.home
    }
}

/// Context for importing providers from another app.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ImportCtx {
    link: String,
}

impl ImportCtx {
    /// An import link or file path the kernel already accepted.
    #[must_use]
    pub fn new(link: impl Into<String>) -> Self {
        Self { link: link.into() }
    }

    /// The link text.
    #[must_use]
    pub fn link(&self) -> &str {
        &self.link
    }
}

/// A provider discovered by [`AgentIntegration::import_providers`].
#[derive(Clone)]
#[non_exhaustive]
pub struct ImportedProvider {
    id: PluginId,
    base_url: String,
    protocol: ProtocolId,
    api_key: Option<SecretString>,
}

impl ImportedProvider {
    /// An imported provider. The protocol is already a codec id.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when `base_url` is empty.
    pub fn new(
        id: PluginId,
        base_url: impl Into<String>,
        protocol: ProtocolId,
        api_key: Option<SecretString>,
    ) -> Result<Self, PluginError> {
        let base_url = base_url.into();
        if base_url.is_empty() {
            return Err(PluginError::config("imported provider URL is empty"));
        }
        Ok(Self {
            id,
            base_url,
            protocol,
            api_key,
        })
    }

    /// Preset or plugin id.
    #[must_use]
    pub fn id(&self) -> &PluginId {
        &self.id
    }

    /// Base URL.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Codec id.
    #[must_use]
    pub const fn protocol(&self) -> ProtocolId {
        self.protocol
    }
}

impl std::fmt::Debug for ImportedProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ImportedProvider")
            .field("id", &self.id)
            .field("base_url", &self.base_url)
            .field("protocol", &self.protocol)
            .field("api_key", &self.api_key)
            .finish()
    }
}

/// A coding-agent plugin. The engine in `hofund-agent` still performs plan and apply.
pub trait AgentIntegration: Send + Sync + 'static {
    /// Compiled metadata.
    fn description(&self) -> &'static AgentDescription;

    /// `Some` when this agent writes a credential store, an OS environment, or a sealed file.
    ///
    /// The default is `None`, and the engine uses slots.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the custom write cannot be planned.
    fn custom_write(
        &self,
        ctx: &WriteCtx,
        target: &GatewayTarget,
    ) -> Result<Option<Plan>, PluginError> {
        let _ = (ctx, target);
        Ok(None)
    }

    /// Providers imported from another app. The default is an empty list.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the import source cannot be read.
    fn import_providers(&self, ctx: &ImportCtx) -> Result<Vec<ImportedProvider>, PluginError> {
        let _ = ctx;
        Ok(Vec::new())
    }
}

/// Builds an [`AgentIntegration`]. A data-only agent has no crate. The registry
/// synthesizes a factory whose schema is empty.
pub trait AgentFactory: Send + Sync + 'static {
    /// The validated config.
    type Config: ValidConfig;

    /// Compiled metadata.
    fn description(&self) -> &'static AgentDescription;

    /// The form the desktop renders.
    fn config_schema(&self) -> Schema;

    /// Checks `raw`.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when `raw` is not valid for this agent.
    fn validate(&self, raw: &RawTable) -> Result<Self::Config, ConfigError>;

    /// Builds the integration.
    ///
    /// # Errors
    ///
    /// Returns [`BuildError`] when the integration cannot be constructed.
    fn build(
        &self,
        cfg: &Self::Config,
        host: &HostContext,
    ) -> Result<Arc<dyn AgentIntegration>, BuildError>;
}

/// The factory the registry stores.
pub trait DynAgentFactory: Send + Sync + 'static {
    /// Compiled metadata.
    fn description(&self) -> &'static AgentDescription;

    /// The form the desktop renders.
    fn config_schema(&self) -> Schema;

    /// Validates `raw` and builds the integration.
    ///
    /// # Errors
    ///
    /// Returns [`BuildError`] when validation or construction fails.
    fn validate_and_build(
        &self,
        raw: &RawTable,
        host: &HostContext,
    ) -> Result<Arc<dyn AgentIntegration>, BuildError>;
}

impl<F> DynAgentFactory for F
where
    F: AgentFactory,
{
    fn description(&self) -> &'static AgentDescription {
        AgentFactory::description(self)
    }

    fn config_schema(&self) -> Schema {
        AgentFactory::config_schema(self)
    }

    fn validate_and_build(
        &self,
        raw: &RawTable,
        host: &HostContext,
    ) -> Result<Arc<dyn AgentIntegration>, BuildError> {
        let cfg = self.validate(raw)?;
        self.build(&cfg, host)
    }
}
