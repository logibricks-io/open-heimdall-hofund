//! Upstream factories and the provider a member adapts.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use hofund_ir::Request;

use crate::completion::{Completion, CountOutcome, ProbeReport};
use crate::config::{BuildError, ConfigError, RawTable, Schema, ValidConfig};
use crate::error::PluginError;
use crate::host::HostContext;
use crate::id::{PluginId, ProtocolId};
use crate::records::{ListQuery, ModelList, ProbeQuery, QuotaQuery, QuotaWindow};

/// Where the SDK places the credential. The header bytes stay the plugin's choice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CredentialStyle {
    /// `Authorization: Bearer`.
    Bearer,
    /// `api-key`.
    ApiKey,
    /// `x-api-key`.
    XApiKey,
    /// Both the Messages `x-api-key` header and a bearer token.
    MessagesBoth,
    /// AWS Signature Version 4, computed by the plugin rather than placed by the SDK.
    SigV4,
    /// A header name the plugin supplies, such as `x-acme-key`.
    NamedHeader(HeaderName),
}

/// A lowercase HTTP header name.
///
/// This is not `http::HeaderName`. [`HeaderName::as_str`] is restricted to
/// `[a-z0-9_-]` so `http::HeaderName::from_static` accepts it. Uppercase is
/// rejected here because `from_static` panics on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct HeaderName {
    value: &'static str,
}

impl HeaderName {
    /// A lowercase header name.
    ///
    /// # Panics
    ///
    /// Panics when `value` is empty or contains anything outside `[a-z0-9_-]`.
    #[must_use]
    pub const fn new(value: &'static str) -> Self {
        assert!(
            is_header(value),
            "header name must be a lowercase ASCII token"
        );
        Self { value }
    }

    /// The header name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.value
    }
}

const fn is_header(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        let ok = byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_';
        if !ok {
            return false;
        }
        index += 1;
    }
    true
}

/// Whether `list_models` is live, a static catalog, deployments, or absent.
///
/// Callers outside this crate must map unknown future variants to
/// [`ListKind::Unsupported`] when matching. [`ListKind::rank`] and
/// [`ListKind::within`] already apply that rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ListKind {
    /// The plugin calls the vendor and returns its general model list.
    Live,
    /// The plugin returns a compiled catalog.
    Catalog,
    /// The plugin dials the vendor but returns only the account's deployment names.
    Deployments,
    /// There is no list.
    Unsupported,
}

impl ListKind {
    /// The explicit capability rank, independent of declaration order.
    #[must_use]
    #[allow(unreachable_patterns)]
    pub const fn rank(self) -> u8 {
        match self {
            Self::Catalog => 1,
            Self::Deployments => 2,
            Self::Live => 3,
            Self::Unsupported | _ => 0,
        }
    }

    /// Whether this capability is no broader than `bound`.
    #[must_use]
    pub const fn within(self, bound: Self) -> bool {
        self.rank() <= bound.rank()
    }
}

/// Static metadata for one upstream implementation.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct UpstreamSpec {
    plugin: PluginId,
    protocols: &'static [ProtocolId],
    credential: CredentialStyle,
    list: ListKind,
}

impl UpstreamSpec {
    /// A spec. Generated code calls this instead of a struct literal.
    #[doc(hidden)]
    #[must_use]
    pub const fn from_static(
        plugin: PluginId,
        protocols: &'static [ProtocolId],
        credential: CredentialStyle,
        list: ListKind,
    ) -> Self {
        Self {
            plugin,
            protocols,
            credential,
            list,
        }
    }

    /// The implementation id.
    #[must_use]
    pub const fn plugin(&self) -> &PluginId {
        &self.plugin
    }

    /// Protocols this implementation speaks.
    #[must_use]
    pub const fn protocols(&self) -> &'static [ProtocolId] {
        self.protocols
    }

    /// How the SDK should place the credential.
    #[must_use]
    pub const fn credential(&self) -> CredentialStyle {
        self.credential
    }

    /// How model listing works.
    #[must_use]
    pub const fn list_kind(&self) -> ListKind {
        self.list
    }
}

/// Kernel-selected values for one upstream build.
///
/// These values are separate from [`RawTable`] so plugin configuration schemas
/// do not need to accept kernel-owned keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct BuildContext {
    protocol: ProtocolId,
}

impl BuildContext {
    /// A build context for `protocol`.
    #[must_use]
    pub const fn new(protocol: ProtocolId) -> Self {
        Self { protocol }
    }

    /// The protocol selected for this provider.
    #[must_use]
    pub const fn protocol(&self) -> ProtocolId {
        self.protocol
    }
}

/// Builds an [`UpstreamProvider`] from validated config.
///
/// The associated type is what the plugin validates. The registry stores
/// [`DynUpstreamFactory`]. The blanket impl calls `validate` and then `build`
/// on the typed value. There is no downcast.
pub trait UpstreamFactory: Send + Sync + 'static {
    /// The validated config.
    type Config: ValidConfig;

    /// Static metadata.
    fn spec(&self) -> &'static UpstreamSpec;

    /// The form the desktop renders.
    fn config_schema(&self) -> Schema;

    /// Checks `raw` before any provider exists.
    ///
    /// The daemon calls this twice: before resolving `env:` references, and again
    /// on the resolved table through `validate_and_build`. Both passes must check
    /// only shape: keys and value types. Value checks belong in `build`, which
    /// runs after the second pass.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when `raw` is not valid for this plugin.
    fn validate(&self, raw: &RawTable) -> Result<Self::Config, ConfigError>;

    /// Checks the environment that the resolved config names, such as files,
    /// for `hofund config check` only. Daemon startup never calls this method,
    /// so providers stay lazy about resources that may appear later.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when the named environment is not usable.
    fn preflight(&self, cfg: &Self::Config) -> Result<(), ConfigError> {
        let _ = cfg;
        Ok(())
    }

    /// Builds the provider. `cfg` has already been validated.
    ///
    /// # Errors
    ///
    /// Returns [`BuildError`] when the provider cannot be constructed.
    fn build(
        &self,
        cfg: &Self::Config,
        host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, BuildError>;

    /// Builds the provider for the protocol selected in `ctx`.
    ///
    /// The default delegates to [`UpstreamFactory::build`] for the spec's first
    /// protocol. A multi-protocol plugin overrides this method to support its
    /// other declared protocols.
    ///
    /// # Errors
    ///
    /// Returns [`BuildError`] when the provider cannot be constructed, or when
    /// the default implementation receives a protocol other than the first.
    fn build_with(
        &self,
        cfg: &Self::Config,
        host: &HostContext,
        ctx: &BuildContext,
    ) -> Result<Arc<dyn UpstreamProvider>, BuildError> {
        if Some(ctx.protocol()) == self.spec().protocols().first().copied() {
            return self.build(cfg, host);
        }
        Err(BuildError::new(format!(
            "plugin `{}` cannot build protocol `{}` without overriding `build_with`",
            self.spec().plugin(),
            ctx.protocol()
        )))
    }
}

/// The factory the registry stores.
pub trait DynUpstreamFactory: Send + Sync + 'static {
    /// Static metadata.
    fn spec(&self) -> &'static UpstreamSpec;

    /// The form the desktop renders.
    fn config_schema(&self) -> Schema;

    /// Validates `raw` without building a provider.
    ///
    /// The daemon calls this before resolving `env:` references.
    /// `validate_and_build` repeats validation on the resolved table before
    /// calling `build`. Both passes must check only shape: keys and value types.
    /// Value checks belong in `build`.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when the raw table fails the factory schema.
    fn validate(&self, raw: &RawTable) -> Result<(), ConfigError>;

    /// Validates resolved `raw` and checks the environment it names for
    /// `hofund config check` only. Daemon startup never calls this method.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when validation or the environment check fails.
    fn preflight(&self, raw: &RawTable) -> Result<(), ConfigError>;

    /// Validates `raw` and builds the provider for the spec's first protocol.
    ///
    /// # Errors
    ///
    /// Returns [`BuildError`] when validation or construction fails.
    /// [`ConfigError`] converts into [`BuildError`].
    fn validate_and_build(
        &self,
        raw: &RawTable,
        host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, BuildError>;

    /// Validates `raw` and builds the provider for the protocol in `ctx`.
    ///
    /// # Errors
    ///
    /// Returns [`BuildError`] when validation or construction fails, or when
    /// `ctx` names a protocol the factory does not declare.
    fn validate_and_build_with(
        &self,
        raw: &RawTable,
        host: &HostContext,
        ctx: &BuildContext,
    ) -> Result<Arc<dyn UpstreamProvider>, BuildError>;
}

impl<F> DynUpstreamFactory for F
where
    F: UpstreamFactory,
{
    fn spec(&self) -> &'static UpstreamSpec {
        UpstreamFactory::spec(self)
    }

    fn config_schema(&self) -> Schema {
        UpstreamFactory::config_schema(self)
    }

    fn validate(&self, raw: &RawTable) -> Result<(), ConfigError> {
        UpstreamFactory::validate(self, raw).map(drop)
    }

    fn preflight(&self, raw: &RawTable) -> Result<(), ConfigError> {
        let cfg = UpstreamFactory::validate(self, raw)?;
        UpstreamFactory::preflight(self, &cfg)
    }

    fn validate_and_build(
        &self,
        raw: &RawTable,
        host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, BuildError> {
        let cfg = UpstreamFactory::validate(self, raw)?;
        self.build(&cfg, host)
    }

    fn validate_and_build_with(
        &self,
        raw: &RawTable,
        host: &HostContext,
        ctx: &BuildContext,
    ) -> Result<Arc<dyn UpstreamProvider>, BuildError> {
        if !self.spec().protocols().contains(&ctx.protocol()) {
            return Err(BuildError::new(format!(
                "plugin `{}` does not declare protocol `{}`",
                self.spec().plugin(),
                ctx.protocol()
            )));
        }
        let cfg = UpstreamFactory::validate(self, raw)?;
        self.build_with(&cfg, host, ctx)
    }
}

/// One upstream endpoint.
#[async_trait]
pub trait UpstreamProvider: Send + Sync + 'static {
    /// Executes `request`.
    ///
    /// `inbound` is the codec id of the client protocol. When it differs from
    /// this provider's protocol, extension bags are dropped. The same id keeps
    /// them, which is how same-protocol passthrough round-trips unknown fields.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the upstream cannot be reached, rejects
    /// the call, or returns a body this provider cannot decode.
    async fn execute(
        &self,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError>;

    /// Counts input tokens.
    ///
    /// The default is [`CountOutcome::Unsupported`].
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the count fails for a reason other than
    /// "this endpoint does not exist".
    async fn count_input(
        &self,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<CountOutcome, PluginError> {
        let _ = (request, inbound);
        Ok(CountOutcome::Unsupported)
    }

    /// Lists models.
    ///
    /// The default rejects the call. [`CountOutcome::Unsupported`] is not used
    /// here: a missing list is an error the caller can show.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the list fails or this upstream has none.
    async fn list_models(&self, query: &ListQuery) -> Result<ModelList, PluginError> {
        let _ = query;
        Err(PluginError::unsupported("this upstream has no model list"))
    }

    /// Sends one minimal request and records a short result.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when [`UpstreamProvider::execute`] fails.
    async fn probe(&self, query: &ProbeQuery) -> Result<ProbeReport, PluginError> {
        let completion = self
            .execute(query.minimal_request(), query.protocol())
            .await?;
        ProbeReport::from_completion(completion).await
    }

    /// Polls quota windows.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the poll fails or this upstream has no
    /// quota endpoint.
    async fn quota(&self, query: &QuotaQuery) -> Result<Vec<QuotaWindow>, PluginError> {
        let _ = query;
        Err(PluginError::unsupported(
            "this upstream has no quota endpoint",
        ))
    }

    /// Whether the provider can take a call. The default is success.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the provider should be left out.
    async fn health(&self) -> Result<(), PluginError> {
        Ok(())
    }

    /// Stops background work before `deadline`. The default does nothing.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when shutdown fails.
    async fn shutdown(&self, deadline: Duration) -> Result<(), PluginError> {
        let _ = deadline;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{HeaderName, ListKind};

    #[test]
    fn list_kind_rank_has_the_explicit_capability_order() {
        assert_eq!(ListKind::Unsupported.rank(), 0);
        assert_eq!(ListKind::Catalog.rank(), 1);
        assert_eq!(ListKind::Deployments.rank(), 2);
        assert_eq!(ListKind::Live.rank(), 3);
        assert!(ListKind::Unsupported.rank() < ListKind::Catalog.rank());
        assert!(ListKind::Catalog.rank() < ListKind::Deployments.rank());
        assert!(ListKind::Deployments.rank() < ListKind::Live.rank());
    }

    #[test]
    fn deployments_fit_live_but_not_catalog_bounds() {
        assert!(ListKind::Deployments.within(ListKind::Live));
        assert!(!ListKind::Deployments.within(ListKind::Catalog));
        assert!(ListKind::Catalog.within(ListKind::Deployments));
    }

    #[test]
    fn header_names_are_lowercase_tokens() {
        let name = HeaderName::new("x-acme-key");
        assert_eq!(name.as_str(), "x-acme-key");
    }

    #[test]
    #[should_panic(expected = "lowercase")]
    fn header_names_reject_uppercase() {
        let _ = HeaderName::new("X-Api-Key");
    }
}
