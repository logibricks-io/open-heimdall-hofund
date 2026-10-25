//! Kernel member. Not part of the plugin contract.
//!
//! The daemon adapts an upstream into this trait and then into a
//! `tower::Service`. The router does not depend on `tower` or on the plugin
//! upstream trait.

use async_trait::async_trait;
use hofund_plugin_api::{Completion, CountOutcome, MemberId, PluginError, Request};

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
}
