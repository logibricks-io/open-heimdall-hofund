//! Control calls the terminal UI wants the background worker to perform.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use hofund_control_client::hofund_control_proto::{
    AgentAction, GroupCreateRequest, GroupPatchRequest,
};

/// Credential text that debug output never prints.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wraps credential text.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrow the credential for the request body only.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Character count, for redacted diagnostics.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the credential is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "[redacted len={}]", self.0.len())
    }
}

/// One control operation produced by a key.
#[derive(Clone, PartialEq)]
pub enum ControlIntent {
    /// `GET /providers`, presets, and status.
    LoadProviders {
        /// Read generation.
        generation: u64,
    },
    /// `GET /agents` and `/agents/status`.
    LoadAgents {
        /// Read generation.
        generation: u64,
    },
    /// `POST .../models/refresh` for the selected provider.
    RefreshModels {
        /// Provider id.
        id: String,
        /// Read generation.
        generation: u64,
    },
    /// `POST .../test`.
    TestProvider {
        /// Provider id.
        id: String,
    },
    /// `GET` models and picks, then open the picker.
    OpenModels {
        /// Provider id.
        id: String,
        /// Read generation.
        generation: u64,
        /// Identifies this open so a slower response cannot replace it.
        token: u64,
    },
    /// `PUT` picks as a full replacement.
    ReplacePicks {
        /// Provider id.
        id: String,
        /// Expected picks revision.
        revision: String,
        /// Enabled model ids.
        ids: Vec<String>,
    },
    /// `PUT` picks reset (`edit: ["-"]`).
    ResetPicks {
        /// Provider id.
        id: String,
        /// Expected picks revision.
        revision: String,
    },
    /// `POST /providers`.
    CreateProvider {
        /// Preset id.
        preset: String,
        /// Expected disk revision.
        revision: String,
        /// Optional display name.
        name: Option<String>,
        /// Optional API root.
        base_url: Option<String>,
        /// Write-only credential.
        api_key: Option<Secret>,
        /// Preset option keys.
        options: BTreeMap<String, String>,
    },
    /// `PATCH /providers/{id}`.
    PatchProvider {
        /// Provider id.
        id: String,
        /// Expected disk revision.
        revision: String,
        /// New enabled bit, when changing it.
        enabled: Option<bool>,
        /// New listed bit, when changing it.
        listed: Option<bool>,
        /// New family tag. Empty clears it.
        family: Option<String>,
        /// New API root.
        base_url: Option<String>,
    },
    /// `DELETE /providers/{id}`.
    DeleteProvider {
        /// Provider id.
        id: String,
        /// Expected disk revision.
        revision: String,
    },
    /// `GET .../keys` before a replace.
    LoadKeys {
        /// Provider id.
        id: String,
        /// Identifies this read so a slower response cannot replace it.
        token: u64,
    },
    /// `POST .../keys` replace.
    ReplaceKey {
        /// Provider id.
        id: String,
        /// Fingerprint from `GET .../keys`.
        key_id: String,
        /// New credential.
        secret: Secret,
    },
    /// Fresh quota balances.
    LoadBalances,
    /// Period usage with the agent breakdown.
    LoadUsage {
        /// Read generation.
        generation: u64,
        /// Reporting period.
        period: hofund_control_client::hofund_control_proto::UsagePeriod,
    },
    /// G-5 Usage settings.
    LoadUsageSettings {
        /// Read generation.
        generation: u64,
    },
    /// Quota cards, optionally after the deferred-read retry delay.
    LoadQuotas {
        /// Quota generation, independent of the period read.
        token: u64,
        /// Read kept cards instead of asking vendors.
        kept: bool,
        /// Wait 1.5 seconds before polling a deferred read.
        delay: bool,
    },
    /// List and run every daily check-in.
    RunCheckins,
    /// Quota alert thresholds.
    LoadQuotaAlerts {
        /// Read generation.
        generation: u64,
    },
    /// Persist one quota alert threshold.
    SetQuotaAlert {
        /// Usage percent, when editing that threshold.
        usage: Option<u8>,
        /// Balance amount, when editing that threshold.
        balance: Option<f64>,
    },
    /// Persist the Usage used/left setting.
    SetQuotaLeft {
        /// Expected settings disk revision.
        revision: String,
        /// New value.
        value: bool,
        /// Value to restore if the write fails.
        previous: bool,
    },
    /// Route model ids for an agent picker.
    LoadRouteModels {
        /// Agent id.
        agent: String,
        /// Read generation.
        generation: u64,
    },
    /// Shown and hidden models for one agent.
    LoadAgentModels {
        /// Agent id.
        agent: String,
        /// Read generation.
        generation: u64,
        /// Whether this read may open the visibility picker.
        open: bool,
    },
    /// `POST .../model`.
    SetAgentModel {
        /// Agent id.
        agent: String,
        /// Route model id, or none to clear.
        model: Option<String>,
    },
    /// `POST .../hidden_models`.
    SetHiddenModels {
        /// Agent id.
        agent: String,
        /// Route ids to hide.
        ids: Vec<String>,
    },
    /// `POST .../picked_only`.
    SetPickedOnly {
        /// Agent id.
        agent: String,
        /// Whether only picked models are shown.
        only: bool,
    },
    /// `POST .../plan`.
    PlanAgent {
        /// Agent id.
        agent: String,
        /// Connect or disconnect.
        action: AgentAction,
    },
    /// `POST .../connect`.
    ConnectAgent {
        /// Agent id.
        agent: String,
        /// Plan revision, when the plan returned one.
        revision: Option<String>,
    },
    /// `POST .../disconnect`.
    DisconnectAgent {
        /// Agent id.
        agent: String,
        /// Plan revision, when the plan returned one.
        revision: Option<String>,
    },
    /// `GET /profiles`.
    LoadProfiles {
        /// Identifies this read so escape can ignore it.
        token: u64,
    },
    /// `POST /profiles/save` from the current agents.
    SaveProfile {
        /// Profile name.
        name: String,
    },
    /// `POST /profiles/switch`.
    SwitchProfile {
        /// Profile name.
        name: String,
    },
    /// `DELETE /profiles/{name}`.
    DeleteProfile {
        /// Profile name.
        name: String,
    },
    /// `POST /profiles/rename`.
    RenameProfile {
        /// Current name.
        from: String,
        /// New name.
        to: String,
    },
    /// `GET /groups`.
    LoadGroups {
        /// Read generation.
        generation: u64,
    },
    /// `GET /groups/member-candidates` for a member or classifier picker.
    LoadGroupCandidates {
        /// Identifies this picker read.
        token: u64,
    },
    /// Reads recent decisions, one session, and unrouted holds.
    LoadRouteView {
        /// Read generation.
        generation: u64,
    },
    /// `POST /groups`.
    CreateGroup {
        /// Create body.
        request: GroupCreateRequest,
    },
    /// `PATCH /groups/{id}`.
    PatchGroup {
        /// Group id before an optional rename.
        id: String,
        /// Single-field or ordered-list patch.
        request: GroupPatchRequest,
    },
    /// `DELETE /groups/{id}`.
    DeleteGroup {
        /// Group id.
        id: String,
        /// Expected disk revision.
        revision: String,
    },
    /// `POST /groups/{id}/restore`.
    RestoreGroup {
        /// Hidden found group id.
        id: String,
        /// Expected disk revision.
        revision: String,
    },
    /// `POST /catalog/sync`.
    CatalogSync {
        /// Fetch even when an index is already loaded.
        force: bool,
    },
}

impl fmt::Debug for ControlIntent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LoadProviders { generation } => formatter
                .debug_struct("LoadProviders")
                .field("generation", generation)
                .finish(),
            Self::LoadAgents { generation } => formatter
                .debug_struct("LoadAgents")
                .field("generation", generation)
                .finish(),
            Self::RefreshModels { id, generation } => formatter
                .debug_struct("RefreshModels")
                .field("id", id)
                .field("generation", generation)
                .finish(),
            Self::TestProvider { id } => formatter
                .debug_struct("TestProvider")
                .field("id", id)
                .finish(),
            Self::OpenModels {
                id,
                generation,
                token,
            } => formatter
                .debug_struct("OpenModels")
                .field("id", id)
                .field("generation", generation)
                .field("token", token)
                .finish(),
            Self::ReplacePicks { id, revision, ids } => formatter
                .debug_struct("ReplacePicks")
                .field("id", id)
                .field("revision", revision)
                .field("ids", ids)
                .finish(),
            Self::ResetPicks { id, revision } => formatter
                .debug_struct("ResetPicks")
                .field("id", id)
                .field("revision", revision)
                .finish(),
            Self::CreateProvider {
                preset,
                revision,
                name,
                base_url,
                api_key,
                options,
            } => formatter
                .debug_struct("CreateProvider")
                .field("preset", preset)
                .field("revision", revision)
                .field("name", name)
                .field("base_url", base_url)
                .field("api_key", api_key)
                .field("options", options)
                .finish(),
            Self::PatchProvider {
                id,
                revision,
                enabled,
                listed,
                family,
                base_url,
            } => formatter
                .debug_struct("PatchProvider")
                .field("id", id)
                .field("revision", revision)
                .field("enabled", enabled)
                .field("listed", listed)
                .field("family", family)
                .field("base_url", base_url)
                .finish(),
            Self::DeleteProvider { id, revision } => formatter
                .debug_struct("DeleteProvider")
                .field("id", id)
                .field("revision", revision)
                .finish(),
            Self::LoadKeys { id, token } => formatter
                .debug_struct("LoadKeys")
                .field("id", id)
                .field("token", token)
                .finish(),
            Self::ReplaceKey { id, key_id, secret } => formatter
                .debug_struct("ReplaceKey")
                .field("id", id)
                .field("key_id", key_id)
                .field("secret", secret)
                .finish(),
            Self::LoadBalances => formatter.write_str("LoadBalances"),
            Self::LoadUsage { generation, period } => formatter
                .debug_struct("LoadUsage")
                .field("generation", generation)
                .field("period", period)
                .finish(),
            Self::LoadUsageSettings { generation } => formatter
                .debug_struct("LoadUsageSettings")
                .field("generation", generation)
                .finish(),
            Self::LoadQuotas { token, kept, delay } => formatter
                .debug_struct("LoadQuotas")
                .field("token", token)
                .field("kept", kept)
                .field("delay", delay)
                .finish(),
            Self::RunCheckins => formatter.write_str("RunCheckins"),
            Self::LoadQuotaAlerts { generation } => formatter
                .debug_struct("LoadQuotaAlerts")
                .field("generation", generation)
                .finish(),
            Self::SetQuotaAlert { usage, balance } => formatter
                .debug_struct("SetQuotaAlert")
                .field("usage", usage)
                .field("balance", balance)
                .finish(),
            Self::SetQuotaLeft {
                revision,
                value,
                previous,
            } => formatter
                .debug_struct("SetQuotaLeft")
                .field("revision", revision)
                .field("value", value)
                .field("previous", previous)
                .finish(),
            Self::LoadRouteModels { agent, generation } => formatter
                .debug_struct("LoadRouteModels")
                .field("agent", agent)
                .field("generation", generation)
                .finish(),
            Self::LoadAgentModels {
                agent,
                generation,
                open,
            } => formatter
                .debug_struct("LoadAgentModels")
                .field("agent", agent)
                .field("generation", generation)
                .field("open", open)
                .finish(),
            Self::SetAgentModel { agent, model } => formatter
                .debug_struct("SetAgentModel")
                .field("agent", agent)
                .field("model", model)
                .finish(),
            Self::SetHiddenModels { agent, ids } => formatter
                .debug_struct("SetHiddenModels")
                .field("agent", agent)
                .field("ids", ids)
                .finish(),
            Self::SetPickedOnly { agent, only } => formatter
                .debug_struct("SetPickedOnly")
                .field("agent", agent)
                .field("only", only)
                .finish(),
            Self::PlanAgent { agent, action } => formatter
                .debug_struct("PlanAgent")
                .field("agent", agent)
                .field("action", action)
                .finish(),
            Self::ConnectAgent { agent, revision } => formatter
                .debug_struct("ConnectAgent")
                .field("agent", agent)
                .field("revision", revision)
                .finish(),
            Self::DisconnectAgent { agent, revision } => formatter
                .debug_struct("DisconnectAgent")
                .field("agent", agent)
                .field("revision", revision)
                .finish(),
            Self::LoadProfiles { token } => formatter
                .debug_struct("LoadProfiles")
                .field("token", token)
                .finish(),
            Self::SaveProfile { name } => formatter
                .debug_struct("SaveProfile")
                .field("name", name)
                .finish(),
            Self::SwitchProfile { name } => formatter
                .debug_struct("SwitchProfile")
                .field("name", name)
                .finish(),
            Self::DeleteProfile { name } => formatter
                .debug_struct("DeleteProfile")
                .field("name", name)
                .finish(),
            Self::RenameProfile { from, to } => formatter
                .debug_struct("RenameProfile")
                .field("from", from)
                .field("to", to)
                .finish(),
            Self::LoadGroups { generation } => formatter
                .debug_struct("LoadGroups")
                .field("generation", generation)
                .finish(),
            Self::LoadGroupCandidates { token } => formatter
                .debug_struct("LoadGroupCandidates")
                .field("token", token)
                .finish(),
            Self::LoadRouteView { generation } => formatter
                .debug_struct("LoadRouteView")
                .field("generation", generation)
                .finish(),
            Self::CreateGroup { request } => formatter
                .debug_struct("CreateGroup")
                .field("name", &request.name)
                .field("first_member", &request.first_member)
                .finish(),
            Self::PatchGroup { id, request } => formatter
                .debug_struct("PatchGroup")
                .field("id", id)
                .field("request", request)
                .finish(),
            Self::DeleteGroup { id, .. } => formatter
                .debug_struct("DeleteGroup")
                .field("id", id)
                .finish(),
            Self::RestoreGroup { id, .. } => formatter
                .debug_struct("RestoreGroup")
                .field("id", id)
                .finish(),
            Self::CatalogSync { force } => formatter
                .debug_struct("CatalogSync")
                .field("force", force)
                .finish(),
        }
    }
}

impl ControlIntent {
    /// How long the worker waits before treating the call as failed.
    #[must_use]
    pub fn budget(&self) -> Duration {
        match self {
            Self::CatalogSync { .. } => Duration::from_secs(45),
            Self::RunCheckins => Duration::from_secs(120),
            Self::LoadProviders { .. }
            | Self::LoadAgents { .. }
            | Self::OpenModels { .. }
            | Self::RefreshModels { .. }
            | Self::LoadRouteModels { .. }
            | Self::LoadAgentModels { .. }
            | Self::LoadGroups { .. }
            | Self::LoadGroupCandidates { .. }
            | Self::LoadRouteView { .. }
            | Self::LoadBalances
            | Self::LoadUsage { .. }
            | Self::LoadUsageSettings { .. }
            | Self::LoadQuotas { .. }
            | Self::LoadQuotaAlerts { .. }
            | Self::LoadProfiles { .. }
            | Self::LoadKeys { .. } => Duration::from_secs(15),
            _ => Duration::from_secs(30),
        }
    }

    /// Whether this call changes daemon state.
    #[must_use]
    pub fn is_mutation(&self) -> bool {
        !matches!(
            self,
            Self::LoadProviders { .. }
                | Self::LoadAgents { .. }
                | Self::OpenModels { .. }
                | Self::LoadRouteModels { .. }
                | Self::LoadAgentModels { .. }
                | Self::LoadGroups { .. }
                | Self::LoadGroupCandidates { .. }
                | Self::LoadRouteView { .. }
                | Self::LoadBalances
                | Self::LoadUsage { .. }
                | Self::LoadUsageSettings { .. }
                | Self::LoadQuotas { .. }
                | Self::LoadQuotaAlerts { .. }
                | Self::LoadProfiles { .. }
                | Self::LoadKeys { .. }
        )
    }

    /// Fills a placeholder read generation with the app's current one.
    pub fn stamp_generation(&mut self, current: u64) {
        match self {
            Self::LoadProviders { generation }
            | Self::LoadAgents { generation }
            | Self::RefreshModels { generation, .. }
            | Self::OpenModels { generation, .. }
            | Self::LoadUsage { generation, .. }
            | Self::LoadUsageSettings { generation }
            | Self::LoadQuotaAlerts { generation }
            | Self::LoadRouteModels { generation, .. }
            | Self::LoadAgentModels { generation, .. }
            | Self::LoadGroups { generation }
            | Self::LoadRouteView { generation }
                if *generation == 0 =>
            {
                *generation = current;
            }
            _ => {}
        }
    }

    /// Credential carried by this call, for redacting the error text.
    #[must_use]
    pub fn secret(&self) -> Option<&str> {
        match self {
            Self::CreateProvider { api_key, .. } => api_key.as_ref().map(Secret::expose),
            Self::ReplaceKey { secret, .. } => Some(secret.expose()),
            _ => None,
        }
    }
}

/// What a page key wants the application to do.
#[derive(Clone, Debug, Default)]
pub struct PageEffect {
    /// Control calls to run.
    pub intents: Vec<ControlIntent>,
    /// Toast text and whether it is a success.
    pub flash: Option<(String, bool)>,
    /// Status-line label while a mutation is in flight.
    pub pending: Option<String>,
    /// Show the catalog-sync status line.
    pub catalog_sync: bool,
}

impl PageEffect {
    /// No change.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// A toast and no call.
    #[must_use]
    pub fn flash(text: impl Into<String>, ok: bool) -> Self {
        Self {
            flash: Some((text.into(), ok)),
            ..Self::default()
        }
    }

    /// One mutation.
    #[must_use]
    pub fn mutation(label: impl Into<String>, intent: ControlIntent) -> Self {
        Self {
            pending: Some(label.into()),
            intents: vec![intent],
            ..Self::default()
        }
    }

    /// One read.
    #[must_use]
    pub fn read(intent: ControlIntent) -> Self {
        Self {
            intents: vec![intent],
            ..Self::default()
        }
    }
}
