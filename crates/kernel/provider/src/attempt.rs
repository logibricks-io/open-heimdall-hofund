//! Per-attempt metadata and encoded-body hooks.

/// Route facts that a caller may use when preparing one upstream attempt.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AttemptRoute {
    /// Configured Responses base URL, when the adapter has one.
    pub base_url: String,
    /// Provider id and a one-way credential fingerprint. Never the credential.
    pub route: String,
    /// Whether this adapter can retain Responses and continue them by id.
    pub keeps_responses: bool,
}

/// Observes and may rewrite one encoded upstream request.
///
/// Returning a body from [`Self::prepare`] marks it as a shortened continuation.
/// [`Self::refused`] is then consulted for a 400, 404 or 422 before the caller decides
/// whether to resend the original, whole body once on the same adapter.
pub trait AttemptHook: Send + Sync {
    /// Returns the encoded body to send, or `None` to leave it unchanged.
    fn prepare(&self, route: &AttemptRoute, body: &[u8]) -> Option<Vec<u8>> {
        let _ = (route, body);
        None
    }

    /// Whether an upstream rejection means its previous response is unavailable.
    fn refused(&self, route: &AttemptRoute, status: u16, body: &[u8]) -> bool {
        let _ = (route, status, body);
        false
    }
}
