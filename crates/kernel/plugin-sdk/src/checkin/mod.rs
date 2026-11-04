//! Vendor daily check-in HTTP helpers plugins reuse.

mod minimax;
mod qoder;
mod trae;

pub use minimax::{MiniMaxSite, run as run_minimax};
pub use qoder::{QoderSite, run as run_qoder};
pub use trae::run as run_trae;

use crate::transport::{Call, Reply};
use hofund_plugin_api::{CheckinOutcome, CheckinResult, PluginError};

/// Sends one signed vendor request for a check-in flow.
pub trait CheckinTransport {
    /// Performs the HTTP round trip as the signed-in account.
    fn send(
        &self,
        call: Call,
    ) -> impl std::future::Future<Output = Result<Reply, PluginError>> + Send;
}

pub(crate) fn expired_sign_in(reply: &Reply) -> bool {
    reply.status() == 401 || reply.status() == 403
}

pub(crate) fn result_failed(message: impl Into<String>) -> CheckinResult {
    CheckinResult::new(CheckinOutcome::Failed, message)
}
