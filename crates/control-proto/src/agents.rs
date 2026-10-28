use serde::{Deserialize, Serialize};

use crate::{API_VERSION, ControlError, ControlErrorKind, FieldIssue};

/// Agent lifecycle action.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(rename_all = "snake_case")]
pub enum AgentAction {
    /// Connect an agent to the gateway.
    #[default]
    Connect,
    /// Disconnect an agent and restore managed state.
    Disconnect,
    /// A newer peer supplied an unknown action.
    #[serde(other)]
    Unknown,
}

/// Agent plan request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(deny_unknown_fields)]
pub struct AgentPlanRequest {
    /// Body wire version.
    pub version: u32,
    /// Requested lifecycle action.
    pub action: AgentAction,
    /// Whether to force the requested edit.
    pub force: bool,
}

impl AgentPlanRequest {
    /// Constructs a version-current plan request.
    #[must_use]
    pub fn new(action: AgentAction, force: bool) -> Self {
        Self {
            version: API_VERSION,
            action,
            force,
        }
    }

    /// Rejects an action this version does not understand, so a typo can
    /// never decode into a request with a different meaning.
    pub(crate) fn validate(&self) -> Result<(), ControlError> {
        if self.action != AgentAction::Unknown {
            return Ok(());
        }
        let message = "unknown agent action; expected connect or disconnect";
        let mut error = ControlError::new(ControlErrorKind::BadRequest, message, "validate");
        error
            .details
            .push(FieldIssue::new("action", "unknown_variant", message));
        Err(error)
    }
}

/// Agent connect request guarded by an optional plan revision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(deny_unknown_fields)]
pub struct AgentConnectRequest {
    /// Body wire version.
    pub version: u32,
    /// Expected revision returned by a prior plan, when checked.
    pub expected_plan_revision: Option<String>,
}

impl AgentConnectRequest {
    /// Constructs a version-current connect request.
    #[must_use]
    pub fn new(expected_plan_revision: Option<String>) -> Self {
        Self {
            version: API_VERSION,
            expected_plan_revision,
        }
    }
}

/// Agent disconnect request guarded by an optional plan revision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(deny_unknown_fields)]
pub struct AgentDisconnectRequest {
    /// Body wire version.
    pub version: u32,
    /// Whether to force disconnection.
    pub force: bool,
    /// Expected revision returned by a prior plan, when checked.
    pub expected_plan_revision: Option<String>,
}

impl AgentDisconnectRequest {
    /// Constructs a version-current disconnect request.
    #[must_use]
    pub fn new(force: bool, expected_plan_revision: Option<String>) -> Self {
        Self {
            version: API_VERSION,
            force,
            expected_plan_revision,
        }
    }
}

/// Minimal installed-agent summary.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AgentSummary {
    /// Agent id.
    pub id: String,
    /// Human-readable name.
    pub display_name: String,
    /// Whether the agent executable or installation was detected.
    pub installed: bool,
}

/// Agent catalog response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AgentListResponse {
    /// Body wire version.
    pub version: u32,
    /// Known agents.
    pub agents: Vec<AgentSummary>,
}

impl Default for AgentListResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            agents: Vec::new(),
        }
    }
}

/// Observed agent connection state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    /// Managed files select the gateway.
    Connected,
    /// Managed files do not select the gateway.
    #[default]
    Disconnected,
    /// Managed files differ from the expected state.
    Drifted,
    /// A newer peer supplied an unknown state.
    #[serde(other)]
    Unknown,
}

/// One agent status entry.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AgentStatusEntry {
    /// Agent id.
    pub id: String,
    /// Observed connection state.
    pub state: AgentState,
    /// Configured model when one is present.
    pub model: Option<String>,
    /// Managed paths inspected by the status operation.
    pub paths: Vec<String>,
}

/// Agent status response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AgentStatusResponse {
    /// Body wire version.
    pub version: u32,
    /// Per-agent status entries.
    pub agents: Vec<AgentStatusEntry>,
}

impl Default for AgentStatusResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            agents: Vec::new(),
        }
    }
}

/// One redacted planned file change.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PlannedChange {
    /// Managed file path.
    pub path: String,
    /// Redacted unified diff; token material must never appear here.
    pub diff: String,
}

/// Revisioned agent plan response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AgentPlanResponse {
    /// Body wire version.
    pub version: u32,
    /// Agent id.
    pub agent: String,
    /// Planned lifecycle action.
    pub action: AgentAction,
    /// Redacted planned changes.
    pub changes: Vec<PlannedChange>,
    /// Opaque plan revision for optimistic concurrency.
    pub plan_revision: String,
}

impl Default for AgentPlanResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            agent: String::new(),
            action: AgentAction::Connect,
            changes: Vec::new(),
            plan_revision: String::new(),
        }
    }
}

/// Applied agent change response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AgentChangeResponse {
    /// Body wire version.
    pub version: u32,
    /// Agent id.
    pub agent: String,
    /// Applied lifecycle action.
    pub action: AgentAction,
    /// Paths changed by the operation.
    pub changed_paths: Vec<String>,
    /// Nonfatal sanitized warnings.
    pub warnings: Vec<String>,
}

impl Default for AgentChangeResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            agent: String::new(),
            action: AgentAction::Connect,
            changed_paths: Vec::new(),
            warnings: Vec::new(),
        }
    }
}
