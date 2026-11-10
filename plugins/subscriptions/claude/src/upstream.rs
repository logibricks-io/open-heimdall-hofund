//! Placeholder for the Claude Code upstream provider.
//!
//! The provider type is not constructed in this slice, and
//! `Subscription::upstream` still returns
//! [`hofund_plugin_api::PluginErrorKind::Unsupported`]. Wiring it to a live
//! [`hofund_plugin_api::UpstreamProvider`] is a later slice, after a
//! text-only turn on the fake runner passes.

/// Marker that the provider module exists without enabling request serving.
#[allow(dead_code)]
pub(crate) struct ClaudeUpstream {
    _private: (),
}
