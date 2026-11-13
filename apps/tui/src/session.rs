//! Runs one control intent and returns a result the UI can apply.

use std::time::Duration;

use hofund_control_client::hofund_control_proto::{
    API_VERSION, AgentAction, AgentConnectRequest, AgentDisconnectRequest, AgentHiddenSetRequest,
    AgentModelRequest, AgentModelsResponse, AgentPickedOnlyRequest, AgentPlanRequest,
    AgentStatusEntry, AgentSummary, CatalogSyncRequest, CheckinRunRequest, GroupDeleteRequest,
    GroupDeleteResponse, GroupListResponse, GroupMemberCandidatesResponse, GroupRestoreRequest,
    GroupWriteResponse, ModelEntry, PicksResponse, PicksSetRequest, PresetEntry,
    ProfileRenameRequest, ProfileSummary, ProviderCreateRequest, ProviderDeleteRequest,
    ProviderKey, ProviderKeyAction, ProviderKeyRequest, ProviderPatchRequest, ProviderSummary,
    ProviderTestRequest, QuotaAlertsResponse, QuotaAlertsSetRequest, QuotaAlertsSetResponse,
    QuotaKind, QuotaQuery, QuotaResponse, RouteListResponse, RouteQuery, SessionRouteResponse,
    SettingsPatchRequest, SettingsResponse, UnroutedListResponse, UsageQuery, UsageResponse,
};
use hofund_control_client::{Client, ClientError};

use crate::intent::ControlIntent;
use crate::redact::redact;

/// Provider list plus the preset catalog and disk revision.
#[derive(Clone, Debug)]
pub struct ProviderSnapshot {
    /// Configured providers.
    pub providers: Vec<ProviderSummary>,
    /// Preset catalog. Empty when that read failed.
    pub presets: Vec<PresetEntry>,
    /// Disk revision from status, when the read succeeded.
    pub disk_revision: String,
    /// Preset-catalog failure that should be toasted without hiding providers.
    pub presets_error: Option<String>,
}

/// Agents and their connection status.
#[derive(Clone, Debug)]
pub struct AgentSnapshot {
    /// Catalog rows.
    pub agents: Vec<AgentSummary>,
    /// Status rows.
    pub status: Vec<AgentStatusEntry>,
}

/// Models and picks for one provider.
#[derive(Clone, Debug)]
pub struct ModelView {
    /// Listed models.
    pub models: Vec<ModelEntry>,
    /// Picks, including `default_exposed` when the server sends it.
    pub picks: PicksResponse,
}

/// A plan the user can confirm.
#[derive(Clone, Debug)]
pub struct PlanView {
    /// Plan revision.
    pub revision: String,
    /// Redacted change lines.
    pub lines: Vec<String>,
}

/// All reads shown by the Routing route panel.
#[derive(Clone, Debug)]
pub struct RouteSnapshot {
    /// Recent routing decisions.
    pub routes: RouteListResponse,
    /// Latest decision for the first listed session, when one exists.
    pub session: Option<SessionRouteResponse>,
    /// Requests waiting before routing.
    pub unrouted: UnroutedListResponse,
}

/// Fields a finished mutation may update.
#[derive(Clone, Debug)]
pub struct Finished {
    /// Toast.
    pub flash: (String, bool),
    /// Reload providers after this call.
    pub reload_providers: bool,
    /// Reload agents after this call.
    pub reload_agents: bool,
    /// Reload profiles after this call.
    pub reload_profiles: bool,
    /// New disk revision.
    pub disk_revision: Option<String>,
    /// Successful picks body.
    pub picks: Option<(String, PicksResponse)>,
    /// Provider whose optimistic picks edit must be undone.
    pub revert_picks: Option<String>,
    /// Agent whose optimistic hidden-model edit must be undone.
    pub revert_hidden: Option<String>,
    /// Agent whose optimistic hidden-model edit landed.
    pub commit_hidden: Option<String>,
    /// Profile list returned by rename or delete.
    pub profiles: Option<Vec<ProfileSummary>>,
}

/// One finished control call.
#[derive(Clone, Debug)]
pub enum SessionOutcome {
    /// Provider list.
    Providers {
        /// Read generation.
        generation: u64,
        /// List, or a redacted error.
        result: Result<ProviderSnapshot, String>,
    },
    /// Agent list.
    Agents {
        /// Read generation.
        generation: u64,
        /// List, or a redacted error.
        result: Result<AgentSnapshot, String>,
    },
    /// Model list and picks.
    Models {
        /// Read generation.
        generation: u64,
        /// Provider id.
        id: String,
        /// Whether the picker should open.
        open: bool,
        /// Open request token. Ignored unless `open` is set.
        token: u64,
        /// Whether this call owns the pending slot.
        owns_pending: bool,
        /// Models, or a redacted error.
        result: Result<ModelView, String>,
        /// Short toast, such as a refresh count.
        flash: Option<(String, bool)>,
    },
    /// Key fingerprints.
    Keys {
        /// Provider id.
        id: String,
        /// Read token.
        token: u64,
        /// Keys, or a redacted error.
        result: Result<Vec<ProviderKey>, String>,
    },
    /// Balance lines.
    Balances(Result<Vec<String>, String>),
    /// Period usage.
    Usage {
        /// Read generation.
        generation: u64,
        /// Usage response, or a redacted error.
        result: Result<UsageResponse, String>,
    },
    /// G-5 Usage settings.
    UsageSettings {
        /// Read generation.
        generation: u64,
        /// Settings response, or a redacted error.
        result: Result<SettingsResponse, String>,
    },
    /// Quota cards.
    Quotas {
        /// Quota generation.
        token: u64,
        /// Quota response, or a redacted error.
        result: Result<QuotaResponse, String>,
    },
    /// Joined daily check-in result.
    Checkins(Result<(String, bool), String>),
    /// Quota alert thresholds.
    QuotaAlerts {
        /// Read generation.
        generation: u64,
        /// Alert response, or a redacted error.
        result: Result<QuotaAlertsResponse, String>,
    },
    /// Saved quota alert thresholds.
    QuotaAlertSaved(Result<QuotaAlertsSetResponse, String>),
    /// Saved G-5 used/left setting.
    QuotaLeftSaved {
        /// Settings response, or a redacted error.
        result: Result<SettingsResponse, String>,
        /// Value to restore on failure.
        previous: bool,
    },
    /// Profiles.
    Profiles {
        /// Read token.
        token: u64,
        /// Profiles, or a redacted error.
        result: Result<Vec<ProfileSummary>, String>,
    },
    /// Route model ids.
    RouteModels {
        /// Read generation.
        generation: u64,
        /// Agent id.
        agent: String,
        /// Ids, or a redacted error.
        result: Result<Vec<String>, String>,
    },
    /// Shown and hidden route ids.
    AgentModels {
        /// Read generation.
        generation: u64,
        /// Agent id.
        agent: String,
        /// Whether this read may open the visibility picker.
        open: bool,
        /// Shown and hidden, or a redacted error.
        result: Result<AgentModelsResponse, String>,
    },
    /// A plan ready to confirm.
    Plan {
        /// Agent id.
        agent: String,
        /// Planned action.
        action: AgentAction,
        /// Plan, or a redacted error.
        result: Result<PlanView, String>,
    },
    /// Routing group list.
    Groups {
        /// Read generation.
        generation: u64,
        /// Group list and disk revision, or a redacted error.
        result: Result<GroupListResponse, String>,
    },
    /// Member or classifier candidates.
    GroupCandidates {
        /// Picker read token.
        token: u64,
        /// Candidate lists, or a redacted error.
        result: Result<GroupMemberCandidatesResponse, String>,
    },
    /// Route panel reads.
    RouteView {
        /// Read generation.
        generation: u64,
        /// Route panel data, or a redacted error.
        result: Result<RouteSnapshot, String>,
    },
    /// A group create, patch, or restore finished.
    GroupWrite {
        /// Group id before an optional rename; empty for create.
        id: String,
        /// Success toast.
        success: String,
        /// Write response, or a redacted error.
        result: Result<GroupWriteResponse, String>,
    },
    /// A group delete finished.
    GroupDelete {
        /// Group id.
        id: String,
        /// Delete response, or a redacted error.
        result: Result<GroupDeleteResponse, String>,
    },
    /// A mutation finished.
    Finished(Finished),
    /// Catalog sync finished.
    Catalog(Result<(), String>),
}

impl ControlIntent {
    /// Outcome used when the call exceeds its budget.
    #[must_use]
    pub fn timed_out(&self) -> SessionOutcome {
        let message = "timed out".to_owned();
        match self {
            Self::LoadProviders { generation } => SessionOutcome::Providers {
                generation: *generation,
                result: Err(message),
            },
            Self::LoadAgents { generation } => SessionOutcome::Agents {
                generation: *generation,
                result: Err(message),
            },
            Self::OpenModels {
                id,
                generation,
                token,
            } => SessionOutcome::Models {
                generation: *generation,
                id: id.clone(),
                open: false,
                token: *token,
                owns_pending: false,
                result: Err(message),
                flash: Some(("timed out".to_owned(), false)),
            },
            Self::RefreshModels { id, generation } => SessionOutcome::Models {
                generation: *generation,
                id: id.clone(),
                open: false,
                token: 0,
                owns_pending: true,
                result: Err(message),
                flash: Some(("timed out".to_owned(), false)),
            },
            Self::LoadKeys { id, token } => SessionOutcome::Keys {
                id: id.clone(),
                token: *token,
                result: Err(message),
            },
            Self::LoadBalances => SessionOutcome::Balances(Err(message)),
            Self::LoadUsage { generation, .. } => SessionOutcome::Usage {
                generation: *generation,
                result: Err(message),
            },
            Self::LoadUsageSettings { generation } => SessionOutcome::UsageSettings {
                generation: *generation,
                result: Err(message),
            },
            Self::LoadQuotas { token, .. } => SessionOutcome::Quotas {
                token: *token,
                result: Err(message),
            },
            Self::RunCheckins => SessionOutcome::Checkins(Err(message)),
            Self::LoadQuotaAlerts { generation } => SessionOutcome::QuotaAlerts {
                generation: *generation,
                result: Err(message),
            },
            Self::SetQuotaAlert { .. } => SessionOutcome::QuotaAlertSaved(Err(message)),
            Self::SetQuotaLeft { previous, .. } => SessionOutcome::QuotaLeftSaved {
                result: Err(message),
                previous: *previous,
            },
            Self::LoadProfiles { token } => SessionOutcome::Profiles {
                token: *token,
                result: Err(message),
            },
            Self::LoadRouteModels { agent, generation } => SessionOutcome::RouteModels {
                generation: *generation,
                agent: agent.clone(),
                result: Err(message),
            },
            Self::LoadAgentModels {
                agent,
                generation,
                open,
            } => SessionOutcome::AgentModels {
                generation: *generation,
                agent: agent.clone(),
                open: *open,
                result: Err(message),
            },
            Self::PlanAgent { agent, action } => SessionOutcome::Plan {
                agent: agent.clone(),
                action: *action,
                result: Err(message),
            },
            Self::LoadGroups { generation } => SessionOutcome::Groups {
                generation: *generation,
                result: Err(message),
            },
            Self::LoadGroupCandidates { token } => SessionOutcome::GroupCandidates {
                token: *token,
                result: Err(message),
            },
            Self::LoadRouteView { generation } => SessionOutcome::RouteView {
                generation: *generation,
                result: Err(message),
            },
            Self::CreateGroup { .. } | Self::PatchGroup { .. } | Self::RestoreGroup { .. } => {
                SessionOutcome::GroupWrite {
                    id: String::new(),
                    success: String::new(),
                    result: Err(message),
                }
            }
            Self::DeleteGroup { id, .. } => SessionOutcome::GroupDelete {
                id: id.clone(),
                result: Err(message),
            },
            Self::CatalogSync { .. } => SessionOutcome::Catalog(Err(message)),
            Self::ReplacePicks { id, .. } | Self::ResetPicks { id, .. } => {
                SessionOutcome::Finished(Finished {
                    flash: ("timed out".to_owned(), false),
                    reload_providers: false,
                    reload_agents: false,
                    reload_profiles: false,
                    disk_revision: None,
                    picks: None,
                    revert_picks: Some(id.clone()),
                    revert_hidden: None,
                    commit_hidden: None,
                    profiles: None,
                })
            }
            Self::SetHiddenModels { agent, .. } => hidden_failure(agent.clone(), message),
            _ => SessionOutcome::Finished(Finished {
                flash: ("timed out".to_owned(), false),
                reload_providers: false,
                reload_agents: false,
                reload_profiles: false,
                disk_revision: None,
                picks: None,
                revert_picks: None,
                revert_hidden: None,
                commit_hidden: None,
                profiles: None,
            }),
        }
    }
}

/// Runs `intent` on `client`. The caller applies the timeout budget.
pub async fn execute(client: &Client, intent: ControlIntent) -> SessionOutcome {
    let secret = intent.secret().map(str::to_owned);
    match intent {
        ControlIntent::LoadProviders { generation } => SessionOutcome::Providers {
            generation,
            result: load_providers(client).await,
        },
        ControlIntent::LoadAgents { generation } => SessionOutcome::Agents {
            generation,
            result: load_agents(client).await,
        },
        ControlIntent::RefreshModels { id, generation } => {
            refresh_models(client, id, generation).await
        }
        ControlIntent::TestProvider { id } => test_provider(client, id).await,
        ControlIntent::OpenModels {
            id,
            generation,
            token,
        } => open_models(client, id, generation, token).await,
        ControlIntent::ReplacePicks { id, revision, ids } => {
            let request = PicksSetRequest::replace(revision, ids);
            write_picks(client, id, request).await
        }
        ControlIntent::ResetPicks { id, revision } => {
            let request = PicksSetRequest::edit(revision, vec!["-".to_owned()]);
            write_picks(client, id, request).await
        }
        ControlIntent::CreateProvider {
            preset,
            revision,
            name,
            base_url,
            api_key,
            options,
        } => {
            let mut request = ProviderCreateRequest::new(revision, preset);
            request.name = name;
            request.base_url = base_url;
            request.api_key = api_key.as_ref().map(|value| value.expose().to_owned());
            request.options = options;
            create_provider(client, request, secret.as_deref()).await
        }
        ControlIntent::PatchProvider {
            id,
            revision,
            enabled,
            listed,
            family,
            base_url,
        } => {
            let mut request = ProviderPatchRequest::default();
            request.version = API_VERSION;
            request.expected_revision = revision;
            request.enabled = enabled;
            request.family = family.map(Some);
            request.listed = listed;
            request.base_url = base_url;
            patch_provider(client, &id, request).await
        }
        ControlIntent::DeleteProvider { id, revision } => {
            let request = ProviderDeleteRequest::new(revision);
            delete_provider(client, &id, request).await
        }
        ControlIntent::LoadKeys { id, token } => match client.provider_keys(&id).await {
            Ok(response) => SessionOutcome::Keys {
                id,
                token,
                result: Ok(response.keys),
            },
            Err(error) => SessionOutcome::Keys {
                id,
                token,
                result: Err(explain(error, None)),
            },
        },
        ControlIntent::ReplaceKey {
            id,
            key_id,
            secret: value,
        } => {
            let mut request = ProviderKeyRequest::new(ProviderKeyAction::Replace).for_id(key_id);
            request.keys = vec![value.expose().to_owned()];
            match client.change_provider_key(&id, request).await {
                Ok(_) => finished(format!("replaced key on {id}"), true, true, false, None),
                Err(error) => {
                    finished(explain(error, secret.as_deref()), false, false, false, None)
                }
            }
        }
        ControlIntent::LoadBalances => SessionOutcome::Balances(load_balances(client).await),
        ControlIntent::LoadUsage { generation, period } => SessionOutcome::Usage {
            generation,
            result: client
                .usage(UsageQuery::with_breakdown(
                    period,
                    hofund_control_client::hofund_control_proto::UsageBreakdownBy::Agent,
                ))
                .await
                .map_err(|error| explain(error, None)),
        },
        ControlIntent::LoadUsageSettings { generation } => SessionOutcome::UsageSettings {
            generation,
            result: client
                .settings()
                .await
                .map_err(|error| explain(error, None)),
        },
        ControlIntent::LoadQuotas { token, kept, delay } => {
            if delay {
                tokio::time::sleep(Duration::from_millis(1500)).await;
            }
            let query = if kept {
                QuotaQuery::kept()
            } else {
                QuotaQuery::fresh()
            };
            SessionOutcome::Quotas {
                token,
                result: client
                    .quotas(query)
                    .await
                    .map_err(|error| explain(error, None)),
            }
        }
        ControlIntent::RunCheckins => SessionOutcome::Checkins(run_checkins(client).await),
        ControlIntent::LoadQuotaAlerts { generation } => SessionOutcome::QuotaAlerts {
            generation,
            result: client
                .quota_alerts()
                .await
                .map_err(|error| explain(error, None)),
        },
        ControlIntent::SetQuotaAlert { usage, balance } => {
            let mut request = QuotaAlertsSetRequest::new();
            request.usage_alert = usage;
            request.balance_alert = balance;
            SessionOutcome::QuotaAlertSaved(
                client
                    .set_quota_alerts(request)
                    .await
                    .map_err(|error| explain(error, None)),
            )
        }
        ControlIntent::SetQuotaLeft {
            revision,
            value,
            previous,
        } => {
            let mut request = SettingsPatchRequest::new(revision);
            request.quota_left = Some(value);
            SessionOutcome::QuotaLeftSaved {
                result: client
                    .patch_settings(&request)
                    .await
                    .map_err(|error| explain(error, None)),
                previous,
            }
        }
        ControlIntent::LoadRouteModels { agent, generation } => match client.route_models().await {
            Ok(response) => SessionOutcome::RouteModels {
                generation,
                agent,
                result: Ok(response.models.into_iter().map(|model| model.id).collect()),
            },
            Err(error) => SessionOutcome::RouteModels {
                generation,
                agent,
                result: Err(explain(error, None)),
            },
        },
        ControlIntent::LoadAgentModels {
            agent,
            generation,
            open,
        } => match client.agent_models(&agent).await {
            Ok(response) => SessionOutcome::AgentModels {
                generation,
                agent,
                open,
                result: Ok(response),
            },
            Err(error) => SessionOutcome::AgentModels {
                generation,
                agent,
                open,
                result: Err(explain(error, None)),
            },
        },
        ControlIntent::SetAgentModel { agent, model } => {
            match client
                .set_agent_model(&agent, AgentModelRequest::new(model))
                .await
            {
                Ok(_) => finished(format!("saved model for {agent}"), true, false, true, None),
                Err(error) => finished(explain(error, None), false, false, false, None),
            }
        }
        ControlIntent::SetHiddenModels { agent, ids } => {
            match client
                .set_agent_hidden_models(&agent, AgentHiddenSetRequest::new(ids))
                .await
            {
                Ok(_) => hidden_saved(agent),
                Err(error) => hidden_failure(agent, explain(error, None)),
            }
        }
        ControlIntent::SetPickedOnly { agent, only } => {
            match client
                .set_agent_picked_only(&agent, AgentPickedOnlyRequest::new(only))
                .await
            {
                Ok(_) => finished(
                    format!("saved visibility for {agent}"),
                    true,
                    false,
                    true,
                    None,
                ),
                Err(error) => finished(explain(error, None), false, false, false, None),
            }
        }
        ControlIntent::PlanAgent { agent, action } => match client
            .plan_agent(&agent, AgentPlanRequest::new(action, false))
            .await
        {
            Ok(response) => SessionOutcome::Plan {
                agent,
                action,
                result: Ok(PlanView {
                    revision: response.plan_revision,
                    lines: response
                        .changes
                        .into_iter()
                        .map(|change| change.path)
                        .collect(),
                }),
            },
            Err(error) => SessionOutcome::Plan {
                agent,
                action,
                result: Err(explain(error, None)),
            },
        },
        ControlIntent::ConnectAgent { agent, revision } => match client
            .connect_agent(&agent, AgentConnectRequest::new(revision))
            .await
        {
            Ok(_) => finished(format!("connected {agent}"), true, false, true, None),
            Err(error) => finished(explain(error, None), false, false, false, None),
        },
        ControlIntent::DisconnectAgent { agent, revision } => match client
            .disconnect_agent(&agent, AgentDisconnectRequest::new(false, revision))
            .await
        {
            Ok(_) => finished(format!("disconnected {agent}"), true, false, true, None),
            Err(error) => finished(explain(error, None), false, false, false, None),
        },
        ControlIntent::LoadProfiles { token } => match client.profiles().await {
            Ok(response) => SessionOutcome::Profiles {
                token,
                result: Ok(response.profiles),
            },
            Err(error) => SessionOutcome::Profiles {
                token,
                result: Err(explain(error, None)),
            },
        },
        ControlIntent::SaveProfile { name } => match client.save_profile(&name).await {
            Ok(_) => finished_reload(format!("saved profile {name}"), false, false, true, None),
            Err(error) => finished(explain(error, None), false, false, false, None),
        },
        ControlIntent::SwitchProfile { name } => match client.switch_profile(&name).await {
            Ok(_) => finished_reload(format!("switched to {name}"), false, true, true, None),
            Err(error) => finished(explain(error, None), false, false, false, None),
        },
        ControlIntent::DeleteProfile { name } => match client.delete_profile(&name).await {
            Ok(response) => finished_profiles(format!("deleted profile {name}"), response.profiles),
            Err(error) => finished(explain(error, None), false, false, false, None),
        },
        ControlIntent::RenameProfile { from, to } => {
            let request = ProfileRenameRequest::new(from, to);
            match client.rename_profile(&request).await {
                Ok(response) => finished_profiles("renamed profile".to_owned(), response.profiles),
                Err(error) => finished(explain(error, None), false, false, false, None),
            }
        }
        ControlIntent::LoadGroups { generation } => SessionOutcome::Groups {
            generation,
            result: client.groups().await.map_err(|error| explain(error, None)),
        },
        ControlIntent::LoadGroupCandidates { token } => SessionOutcome::GroupCandidates {
            token,
            result: client
                .group_member_candidates()
                .await
                .map_err(|error| explain(error, None)),
        },
        ControlIntent::LoadRouteView { generation } => SessionOutcome::RouteView {
            generation,
            result: load_route_view(client).await,
        },
        ControlIntent::CreateGroup { request } => {
            let success = format!("created group {}", request.name);
            let result = client
                .create_group(&request)
                .await
                .map_err(|error| explain(error, None));
            SessionOutcome::GroupWrite {
                id: String::new(),
                success,
                result,
            }
        }
        ControlIntent::PatchGroup { id, request } => {
            let result = client
                .patch_group(&id, &request)
                .await
                .map_err(|error| explain(error, None));
            SessionOutcome::GroupWrite {
                success: format!("saved {id}"),
                id,
                result,
            }
        }
        ControlIntent::RestoreGroup { id, revision } => {
            let request = GroupRestoreRequest::new(revision);
            let result = client
                .restore_group(&id, &request)
                .await
                .map_err(|error| explain(error, None));
            SessionOutcome::GroupWrite {
                success: format!("restored {id}"),
                id,
                result,
            }
        }
        ControlIntent::DeleteGroup { id, revision } => {
            let request = GroupDeleteRequest::new(revision);
            let result = client
                .delete_group(&id, &request)
                .await
                .map_err(|error| explain(error, None));
            SessionOutcome::GroupDelete { id, result }
        }
        ControlIntent::CatalogSync { force } => {
            match client.sync_catalog(&CatalogSyncRequest::new(force)).await {
                Ok(response) if response.ok => SessionOutcome::Catalog(Ok(())),
                Ok(response) => SessionOutcome::Catalog(Err(redact(
                    response.error.as_deref().unwrap_or("catalog sync failed"),
                    None,
                ))),
                Err(error) => SessionOutcome::Catalog(Err(explain(error, None))),
            }
        }
    }
}

/// Runs `intent` and converts a timeout into a failed outcome.
pub async fn run_intent(client: &Client, intent: ControlIntent) -> SessionOutcome {
    let budget = intent.budget();
    let timed_out = intent.timed_out();
    match tokio::time::timeout(budget, execute(client, intent)).await {
        Ok(outcome) => outcome,
        Err(_) => timed_out,
    }
}

async fn load_providers(client: &Client) -> Result<ProviderSnapshot, String> {
    let providers = client
        .providers()
        .await
        .map_err(|error| explain(error, None))?;
    let (presets, presets_error) = match client.presets().await {
        Ok(response) => (response.presets, None),
        Err(error) => (Vec::new(), Some(explain(error, None))),
    };
    let disk_revision = match client.status().await {
        Ok(status) => status.disk_revision,
        Err(_) => String::new(),
    };
    Ok(ProviderSnapshot {
        providers: providers.providers,
        presets,
        disk_revision,
        presets_error,
    })
}

async fn load_agents(client: &Client) -> Result<AgentSnapshot, String> {
    let agents = client
        .agents()
        .await
        .map_err(|error| explain(error, None))?;
    let status = client
        .agent_status()
        .await
        .map_err(|error| explain(error, None))?;
    Ok(AgentSnapshot {
        agents: agents.agents,
        status: status.agents,
    })
}

async fn load_route_view(client: &Client) -> Result<RouteSnapshot, String> {
    let routes = client
        .route_decisions(RouteQuery::default())
        .await
        .map_err(|error| explain(error, None))?;
    let unrouted = client
        .route_unrouted()
        .await
        .map_err(|error| explain(error, None))?;
    let session = match routes
        .decisions
        .iter()
        .find(|decision| !decision.session.is_empty())
    {
        Some(decision) => Some(
            client
                .route_session(&decision.session, RouteQuery::default())
                .await
                .map_err(|error| explain(error, None))?,
        ),
        None => None,
    };
    Ok(RouteSnapshot {
        routes,
        session,
        unrouted,
    })
}

async fn open_models(client: &Client, id: String, generation: u64, token: u64) -> SessionOutcome {
    match read_models(client, &id, false).await {
        Ok(view) => SessionOutcome::Models {
            generation,
            id,
            open: true,
            token,
            owns_pending: false,
            result: Ok(view),
            flash: None,
        },
        Err(error) => SessionOutcome::Models {
            generation,
            id,
            open: true,
            token,
            owns_pending: false,
            result: Err(error),
            flash: None,
        },
    }
}

async fn refresh_models(client: &Client, id: String, generation: u64) -> SessionOutcome {
    match read_models(client, &id, true).await {
        Ok(view) => {
            let count = view.models.len();
            SessionOutcome::Models {
                generation,
                id,
                open: false,
                token: 0,
                owns_pending: true,
                result: Ok(view),
                flash: Some((format!("refreshed {count} models"), true)),
            }
        }
        Err(error) => SessionOutcome::Models {
            generation,
            id,
            open: false,
            token: 0,
            owns_pending: true,
            result: Err(error.clone()),
            flash: Some((error, false)),
        },
    }
}

async fn read_models(client: &Client, id: &str, refresh: bool) -> Result<ModelView, String> {
    let models = if refresh {
        client.refresh_models(id).await
    } else {
        client.provider_models(id).await
    }
    .map_err(|error| explain(error, None))?;
    let picks = client
        .picks(id)
        .await
        .map_err(|error| explain(error, None))?;
    Ok(ModelView {
        models: models.models,
        picks,
    })
}

async fn test_provider(client: &Client, id: String) -> SessionOutcome {
    match client
        .test_provider(&id, ProviderTestRequest::new(None))
        .await
    {
        Ok(response) => {
            let failed = response.results.iter().find(|result| !result.ok);
            let (text, ok) = if let Some(result) = failed {
                let detail = result
                    .error
                    .as_ref()
                    .map(|error| error.message.clone())
                    .unwrap_or_else(|| "failed".to_owned());
                (format!("failed {id}: {}", redact(&detail, None)), false)
            } else if response.results.is_empty() {
                (format!("tested {id}"), true)
            } else {
                (format!("ok {id}"), true)
            };
            finished(text, ok, false, false, None)
        }
        Err(error) => finished(explain(error, None), false, false, false, None),
    }
}

async fn write_picks(client: &Client, id: String, request: PicksSetRequest) -> SessionOutcome {
    match client.set_picks(&id, request).await {
        Ok(picks) => SessionOutcome::Finished(Finished {
            flash: (format!("saved picks for {id}"), true),
            reload_providers: true,
            reload_agents: false,
            reload_profiles: false,
            disk_revision: None,
            picks: Some((id, picks)),
            revert_picks: None,
            revert_hidden: None,
            commit_hidden: None,
            profiles: None,
        }),
        Err(error) => SessionOutcome::Finished(Finished {
            flash: (explain(error, None), false),
            reload_providers: false,
            reload_agents: false,
            reload_profiles: false,
            disk_revision: None,
            picks: None,
            revert_picks: Some(id),
            revert_hidden: None,
            commit_hidden: None,
            profiles: None,
        }),
    }
}

async fn create_provider(
    client: &Client,
    request: ProviderCreateRequest,
    secret: Option<&str>,
) -> SessionOutcome {
    match client.create_provider(&request).await {
        Ok(response) => finished(
            format!("added {}", response.provider.id),
            true,
            true,
            false,
            Some(response.disk_revision),
        ),
        Err(error) => finished(explain(error, secret), false, false, false, None),
    }
}

async fn patch_provider(
    client: &Client,
    id: &str,
    request: ProviderPatchRequest,
) -> SessionOutcome {
    match client.patch_provider(id, &request).await {
        Ok(response) => finished(
            format!("saved {id}"),
            true,
            true,
            false,
            Some(response.disk_revision),
        ),
        Err(error) => finished(explain(error, None), false, false, false, None),
    }
}

async fn delete_provider(
    client: &Client,
    id: &str,
    request: ProviderDeleteRequest,
) -> SessionOutcome {
    match client.delete_provider(id, &request).await {
        Ok(response) => finished(
            format!("deleted {}", response.removed),
            true,
            true,
            false,
            Some(response.disk_revision),
        ),
        Err(error) => finished(explain(error, None), false, false, false, None),
    }
}

async fn load_balances(client: &Client) -> Result<Vec<String>, String> {
    let response = client
        .quotas(QuotaQuery::fresh())
        .await
        .map_err(|error| explain(error, None))?;
    Ok(response
        .data
        .into_iter()
        .filter(|quota| quota.kind == QuotaKind::Balance)
        .map(|quota| {
            let amount = if quota.balance.is_empty() {
                quota.error
            } else {
                quota.balance
            };
            format!("{} {amount}", quota.name)
        })
        .collect())
}

async fn run_checkins(client: &Client) -> Result<(String, bool), String> {
    let list = client
        .checkins()
        .await
        .map_err(|error| explain(error, None))?;
    if list.accounts.is_empty() {
        return Ok(("no accounts have a daily check-in".to_owned(), true));
    }
    let mut parts = Vec::new();
    let mut ok = true;
    for row in list.accounts {
        let who = format!("{}/{}", row.subscription, row.account);
        match client
            .run_checkin(CheckinRunRequest::new(row.subscription, row.account))
            .await
        {
            Ok(response) => {
                let outcome = if response.outcome.is_empty() {
                    "unknown".to_owned()
                } else {
                    response.outcome
                };
                let mut part = format!("check-in {who}: {outcome}");
                if !response.detail.is_empty() {
                    part.push_str(&format!(" · {}", response.detail));
                }
                if outcome.eq_ignore_ascii_case("failed")
                    || outcome.eq_ignore_ascii_case("failure")
                    || outcome.eq_ignore_ascii_case("error")
                {
                    ok = false;
                }
                parts.push(part);
            }
            Err(error) => {
                ok = false;
                parts.push(format!("check-in {who}: failed · {}", explain(error, None)));
            }
        }
    }
    Ok((parts.join("; "), ok))
}

fn finished(
    text: impl Into<String>,
    ok: bool,
    reload_providers: bool,
    reload_agents: bool,
    disk_revision: Option<String>,
) -> SessionOutcome {
    SessionOutcome::Finished(Finished {
        flash: (text.into(), ok),
        reload_providers,
        reload_agents,
        reload_profiles: false,
        disk_revision,
        picks: None,
        revert_picks: None,
        revert_hidden: None,
        commit_hidden: None,
        profiles: None,
    })
}

fn hidden_saved(agent: String) -> SessionOutcome {
    SessionOutcome::Finished(Finished {
        flash: (format!("saved visibility for {agent}"), true),
        reload_providers: false,
        reload_agents: true,
        reload_profiles: false,
        disk_revision: None,
        picks: None,
        revert_picks: None,
        revert_hidden: None,
        commit_hidden: Some(agent),
        profiles: None,
    })
}

fn hidden_failure(agent: String, message: String) -> SessionOutcome {
    SessionOutcome::Finished(Finished {
        flash: (message, false),
        reload_providers: false,
        reload_agents: false,
        reload_profiles: false,
        disk_revision: None,
        picks: None,
        revert_picks: None,
        revert_hidden: Some(agent),
        commit_hidden: None,
        profiles: None,
    })
}

fn finished_reload(
    text: impl Into<String>,
    reload_providers: bool,
    reload_agents: bool,
    reload_profiles: bool,
    disk_revision: Option<String>,
) -> SessionOutcome {
    SessionOutcome::Finished(Finished {
        flash: (text.into(), true),
        reload_providers,
        reload_agents,
        reload_profiles,
        disk_revision,
        picks: None,
        revert_picks: None,
        revert_hidden: None,
        commit_hidden: None,
        profiles: None,
    })
}

fn finished_profiles(text: impl Into<String>, profiles: Vec<ProfileSummary>) -> SessionOutcome {
    SessionOutcome::Finished(Finished {
        flash: (text.into(), true),
        reload_providers: false,
        reload_agents: false,
        reload_profiles: false,
        disk_revision: None,
        picks: None,
        revert_picks: None,
        revert_hidden: None,
        commit_hidden: None,
        profiles: Some(profiles),
    })
}

fn explain(error: ClientError, secret: Option<&str>) -> String {
    redact(&error.to_string(), secret)
}
