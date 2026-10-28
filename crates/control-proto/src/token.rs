use std::fmt;

use serde::{Deserialize, Serialize};

use crate::API_VERSION;

/// Token rotation request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(deny_unknown_fields)]
pub struct TokenRotateRequest {
    /// Body wire version.
    pub version: u32,
}

impl TokenRotateRequest {
    /// Constructs a version-current token rotation request.
    #[must_use]
    pub fn new() -> Self {
        Self {
            version: API_VERSION,
        }
    }
}

impl Default for TokenRotateRequest {
    fn default() -> Self {
        Self::new()
    }
}

/// Result of refreshing one connected agent after token rotation.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AgentRefreshResult {
    /// Agent id.
    pub id: String,
    /// Whether the agent files were refreshed.
    pub ok: bool,
    /// Optional sanitized failure or warning.
    pub message: Option<String>,
}

/// Token rotation response. The new token is returned exactly once.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TokenRotateResponse {
    /// Body wire version.
    pub version: u32,
    /// New bearer token, always redacted from `Debug`.
    pub token: String,
    /// Per-agent refresh outcomes.
    pub agents: Vec<AgentRefreshResult>,
}

impl Default for TokenRotateResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            token: String::new(),
            agents: Vec::new(),
        }
    }
}

impl fmt::Debug for TokenRotateResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TokenRotateResponse")
            .field("version", &self.version)
            .field("token", &"<redacted>")
            .field("agents", &self.agents)
            .finish()
    }
}
