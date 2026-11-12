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
    /// Effective socket-peer allow list after environment precedence.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_ips: Vec<String>,
    /// Whether forwarding headers on loopback retain local-only privileges.
    #[serde(default, skip_serializing_if = "is_false")]
    pub trust_proxy: bool,
    /// Whether a malformed environment allow list currently refuses every remote peer.
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_ips_invalid: bool,
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

/// Process role that started this daemon, when set by the parent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(rename_all = "snake_case")]
pub enum ServeRole {
    /// Started by the terminal UI.
    Tui,
    /// Started by `hofund serve` or an equivalent launcher.
    Serve,
    /// Started by the menu-bar app.
    Desktop,
    /// A newer peer supplied an unknown role.
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
    /// Parent-declared serve role, when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serves: Option<ServeRole>,
    /// Parent process id that spawned this daemon, or zero when unknown.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub owned_by_pid: u32,
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_zero(value: &u32) -> bool {
    *value == 0
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
            serves: None,
            owned_by_pid: 0,
        }
    }
}
