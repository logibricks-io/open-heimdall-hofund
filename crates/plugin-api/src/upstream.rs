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

/// Whether `list_models` is live, a static catalog, or absent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ListKind {
    /// The plugin calls the vendor.
    Live,
    /// The plugin returns a compiled catalog.
    Catalog,
    /// There is no list.
    Unsupported,
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
    /// # Errors
    ///
    /// Returns [`ConfigError`] when `raw` is not valid for this plugin.
    fn validate(&self, raw: &RawTable) -> Result<Self::Config, ConfigError>;

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
}

/// The factory the registry stores.
pub trait DynUpstreamFactory: Send + Sync + 'static {
    /// Static metadata.
    fn spec(&self) -> &'static UpstreamSpec;

    /// The form the desktop renders.
    fn config_schema(&self) -> Schema;

    /// Validates `raw` and builds the provider.
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

    fn validate_and_build(
        &self,
        raw: &RawTable,
        host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, BuildError> {
        let cfg = self.validate(raw)?;
        self.build(&cfg, host)
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
    use super::HeaderName;

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
