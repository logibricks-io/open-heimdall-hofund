use serde::{Deserialize, Serialize};

use crate::{API_VERSION, FieldIssue};

/// Control health response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct HealthResponse {
    /// Body wire version.
    pub version: u32,
    /// Supported control API version.
    pub api_version: u32,
    /// Daemon package version.
    pub daemon_version: String,
}

impl Default for HealthResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            api_version: API_VERSION,
            daemon_version: String::new(),
        }
    }
}

/// Source of the effective model listen address.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(rename_all = "snake_case")]
pub enum ListenSource {
    /// An environment override supplied the address.
    Environment,
    /// The configuration file supplied the address.
    #[default]
    Config,
    /// A newer peer supplied an unknown source.
    #[serde(other)]
    Unknown,
}

/// Configured, effective, and actually running model listener labels.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelListenerStatus {
    /// Address written in configuration.
    pub configured: String,
    /// Address after environment precedence.
    pub effective: String,
    /// Bound address, or `None` when no model listener is running.
    pub running: Option<String>,
    /// Source of the effective address.
    pub effective_source: ListenSource,
}

/// Daemon shutdown state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(rename_all = "snake_case")]
pub enum ShutdownState {
    /// The daemon accepts work normally.
    #[default]
    Running,
    /// The daemon is draining in-flight work.
    Draining,
    /// A newer peer supplied an unknown state.
    #[serde(other)]
    Unknown,
}

/// Daemon status response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct StatusResponse {
    /// Body wire version.
    pub version: u32,
    /// Daemon process id.
    pub pid: u32,
    /// Unix milliseconds when the daemon started.
    pub started_at_ms: u64,
    /// Model listener state.
    pub model_listener: ModelListenerStatus,
    /// Publication generation.
    pub generation: u64,
    /// SHA-256 disk-content revision.
    pub disk_revision: String,
    /// SHA-256 published runtime revision.
    pub runtime_revision: String,
    /// Restart-only fields not yet active.
    pub pending_restart: Vec<String>,
    /// Live fields whose update was not applied.
    pub not_applied: Vec<FieldIssue>,
    /// Number of requests currently in flight.
    pub in_flight: u64,
    /// Shutdown state.
    pub shutdown: ShutdownState,
}

impl Default for StatusResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            pid: 0,
            started_at_ms: 0,
            model_listener: ModelListenerStatus::default(),
            generation: 0,
            disk_revision: String::new(),
            runtime_revision: String::new(),
            pending_restart: Vec::new(),
            not_applied: Vec::new(),
            in_flight: 0,
            shutdown: ShutdownState::Running,
        }
    }
}
