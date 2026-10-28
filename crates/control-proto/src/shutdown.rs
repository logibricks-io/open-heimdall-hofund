use serde::{Deserialize, Serialize};

use crate::API_VERSION;

/// Graceful shutdown request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(deny_unknown_fields)]
pub struct ShutdownRequest {
    /// Body wire version.
    pub version: u32,
}

impl ShutdownRequest {
    /// Constructs a version-current shutdown request.
    #[must_use]
    pub fn new() -> Self {
        Self {
            version: API_VERSION,
        }
    }
}

impl Default for ShutdownRequest {
    fn default() -> Self {
        Self::new()
    }
}

/// Shutdown acknowledgement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ShutdownResponse {
    /// Body wire version.
    pub version: u32,
    /// Whether the daemon is draining in-flight work.
    pub draining: bool,
}

impl Default for ShutdownResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            draining: false,
        }
    }
}

/// Unit-like marker for routes with no JSON body. It is never decoded from a
/// request body; path and query validation still apply.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct EmptyRequest;

impl EmptyRequest {
    /// Constructs the no-body marker.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}
