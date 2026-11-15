//! Host-to-plugin routing hints on [`hofund_ir::Request::extensions`].
//!
//! These keys are never sent upstream. Codecs strip the extension bag on
//! cross-protocol encode, and subscription adapters shape native bodies from
//! decoded chat fields alone.

/// When `true`, the router still has at least one routable member after the
/// current attempt. Subscription queue waits should short-circuit so failover
/// can try the next member immediately.
pub const ANOTHER_ROUTING_CANDIDATE: &str = "hofund_another_routing_candidate";
