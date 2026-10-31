//! Kernel member. Not part of the plugin contract.
//!
//! The daemon adapts an upstream into this trait and then into a
//! `tower::Service`. The router does not depend on `tower` or on the plugin
//! upstream trait.

use async_trait::async_trait;
use hofund_plugin_api::{
    Completion, CountOutcome, MemberId, PluginError, RawAnswer, RawBody, Request,
};

use crate::{AttemptHook, MemberAttempt};

/// One upstream the router may call.
///
/// `id` is stable across snapshots. `route_id` is the configured provider id
/// recorded on usage rows, which is not the [`MemberId`] display form.
#[async_trait]
pub trait Member: Send + Sync {
    /// Stable id. Cooldown and usage keep this across a snapshot swap.
    fn id(&self) -> &MemberId;

    /// Configured provider id. Usage rows and route errors use this text.
    fn route_id(&self) -> &str;

    /// Provider id used in model and account scopes. Account members override it.
    fn scope_provider(&self) -> &str {
        self.route_id()
    }

    /// Plugin that owns this member. Empty when the member is not a plugin.
    fn plugin_id(&self) -> &str {
        ""
    }

    /// Preset id from configuration. Empty when the provider has no preset.
    fn preset_id(&self) -> &str {
        ""
    }

    /// Codec id this member speaks. Compared with the inbound protocol.
    fn protocol(&self) -> &str;

    /// Stable account ids or provider-key fingerprints usable in account scopes.
    fn account_refs(&self) -> Vec<String> {
        vec![self.id().to_string()]
    }

    /// Per-attempt facts. Adapters with upstream route metadata override this.
    fn attempt(&self) -> MemberAttempt {
        MemberAttempt {
            provider_id: self.route_id().to_owned(),
            protocol: self.protocol().to_owned(),
            plugin: !self.plugin_id().is_empty(),
            ..MemberAttempt::default()
        }
    }

    /// Executes `request`.
    ///
    /// `inbound` is the client codec id. The daemon's layer chain applies the
    /// deadline, panic containment, and the span around this call when the
    /// member was built there. A member constructed in a test runs the body
    /// directly.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the upstream cannot produce a completion.
    async fn execute(&self, request: Request, inbound: &str) -> Result<Completion, PluginError>;

    /// Executes with a per-request attempt hook.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::execute`].
    async fn execute_with_attempt(
        &self,
        request: Request,
        inbound: &str,
        hook: Option<std::sync::Arc<dyn AttemptHook>>,
    ) -> Result<Completion, PluginError> {
        let _ = hook;
        self.execute(request, inbound).await
    }

    /// Counts input tokens.
    ///
    /// The default is [`CountOutcome::Unsupported`].
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the count is rejected for a reason other
    /// than "this endpoint does not exist".
    async fn count_input(
        &self,
        request: Request,
        inbound: &str,
    ) -> Result<CountOutcome, PluginError> {
        let _ = (request, inbound);
        Ok(CountOutcome::Unsupported)
    }

    /// Posts `body` to `path` under the upstream's base URL.
    ///
    /// The default is `unsupported`.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the upstream does not serve `path` or the
    /// call fails.
    async fn forward(&self, path: &str, body: RawBody) -> Result<RawAnswer, PluginError> {
        let _ = body;
        Err(PluginError::unsupported(format!(
            "this member does not serve {path}"
        )))
    }
}
