//! Per-request idle read budget on the host HTTP pool.

use std::time::Duration;

/// Replaces the pooled client's default idle read budget for one [`super::kernel::HttpRequest`].
///
/// Set through the plugin SDK's `Call::read_timeout`. The host pool honours a longer
/// or shorter budget than its process-wide default for that send only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostReadBudget(pub Duration);

/// Returns the idle read budget attached to `request`, if any.
#[must_use]
pub fn read_budget(request: &http::Request<crate::Body>) -> Option<Duration> {
    request
        .extensions()
        .get::<HostReadBudget>()
        .map(|budget| budget.0)
}

/// Attaches `budget` to `request`, replacing any previous budget.
pub fn set_read_budget(request: &mut http::Request<crate::Body>, budget: Duration) {
    request.extensions_mut().insert(HostReadBudget(budget));
}
