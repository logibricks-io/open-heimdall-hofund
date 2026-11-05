use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::TimeZone;
use hofund_agent::Session;
use hofund_config::Config;
use hofund_control_proto::{
    AgentChangeResponse, AgentConnectRequest, AgentDisconnectRequest, AgentHiddenSetRequest,
    AgentListResponse, AgentModelRequest, AgentModelsResponse, AgentPickedOnlyRequest,
    AgentPlanRequest, AgentPlanResponse, AgentStatusResponse, AgentVisibleSetRequest,
    CatalogDisabledListResponse, CatalogModelRequest, CheckinAccountRow, CheckinListResponse,
    CheckinRunRequest, CheckinRunResponse, ControlError, ControlErrorKind, FieldIssue, GatewayKey,
    GatewayKeyAction, GatewayKeyLimit, GatewayKeyRequest, GatewayKeyResponse,
    GatewayKeyRestoreRequest, HealthResponse, ModelEntry, ModelListResponse, PicksResponse,
    PicksSetRequest, ProfileDeleteRequest, ProfileDetailResponse, ProfileListResponse,
    ProfileSaveRequest, ProfileSwitchRequest, ProfileSwitchResponse, ProviderKeyRequest,
    ProviderKeysResponse, ProviderListResponse, ProviderRenameRequest, ProviderRenameResponse,
    ProviderSpendCap, ProviderSpendCapPeriod, ProviderSpendCapRequest, ProviderSpendCapsResponse,
    ProviderSummary, ProviderTestRequest, ProviderTestResponse, ProviderTestResult,
    QuotaAlertsResponse, QuotaAlertsSetRequest, QuotaAlertsSetResponse, QuotaCreditsModesResponse,
    QuotaCreditsSetRequest, QuotaCreditsSetResponse, QuotaHistoryQuery, QuotaHistoryResponse,
    QuotaQuery, QuotaResponse, RouteListResponse, RouteModel, RouteModelListResponse, RouteQuery,
    RouteReserve, SessionRouteResponse, ShutdownRequest, ShutdownResponse, ShutdownState,
    StatusResponse, TokenRotateRequest, TokenRotateResponse, UnroutedListResponse,
    UsageBreakdownBy, UsageBreakdownGroup, UsagePeriod, UsageQuery, UsageResponse, UsageRow,
    UsageTotals, WarmupAccountRow, WarmupListResponse, WarmupRunRequest, WarmupRunResponse,
};
use hofund_control_proto::{
    ConfigApplyRequest, ConfigResponse, ConfigValidateRequest, ConfigValidationResponse,
    MemberSummary, ReloadRequest, SnapshotEvent,
};
use hofund_gateway::control::{
    ControlFailure, ControlFuture, ControlState, EventSubscription, EventsQuery,
};
use hofund_plugin_api::{CheckinOutcome, ModelSpec, WarmupOutcome};

use crate::quotas::DaemonQuotas;
use hofund_router::Cooldowns;
use hofund_secrets::{KeyLimit, KeyStore};
use hofund_usage::{Ledger, Window};
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use super::config_ops::{ConfigOps, ReloadOrigin};
use super::control_agents::AgentOps;
use super::control_profiles::ProfileOps;
use super::events::{EventBus, SnapshotSource};
use super::models::{PicksError, Timed, test_timeout};
use super::{
    DaemonError, DaemonRegistry, ErrorKind, ModelsHost, Picks, control_map, membership, plugin_for,
    protocol_of,
};

pub(crate) struct DaemonControl {
    config_path: PathBuf,
    started_at_ms: u64,
    models: Arc<ModelsHost>,
    ops: Arc<ConfigOps>,
    router: Option<hofund_router::Snapshot>,
    registry: Arc<dyn DaemonRegistry>,
    cooldowns: Arc<Cooldowns>,
    usage: Arc<Ledger>,
    keys: Arc<KeyStore>,
    agents: AgentOps,
    profiles: ProfileOps,
    shutdown: watch::Sender<bool>,
    events: Arc<EventBus>,
    in_flight: Arc<AtomicU64>,
    quotas: Option<Arc<DaemonQuotas>>,
    runtime: Option<Arc<crate::runtime::Runtime>>,
    route_trace: Option<Arc<hofund_router::RouteTraceBook>>,
}

impl DaemonControl {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        config_path: &Path,
        started_at_ms: u64,
        models: Arc<ModelsHost>,
        ops: Arc<ConfigOps>,
        registry: Arc<dyn DaemonRegistry>,
        cooldowns: Arc<Cooldowns>,
        usage: Arc<Ledger>,
        keys: Arc<KeyStore>,
        token_path: PathBuf,
        shutdown: watch::Sender<bool>,
    ) -> Self {
        let registry_agents = registry.agents();
        let routes_from = Arc::clone(&ops);
        let listener_from = Arc::clone(&ops);
        Self {
            config_path: config_path.to_path_buf(),
            started_at_ms,
            models,
            registry,
            cooldowns,
            agents: AgentOps::new(
                registry_agents,
                config_path,
                // The address served now, so a `listen` port of 0 still names
                // the port agents can reach and a relisten is followed.
                Arc::new(move || {
                    let listener = listener_from.listener();
                    format!("http://{}", listener.running.unwrap_or(listener.effective))
                }),
                Arc::new(move |agent: Option<&str>, active: Option<&str>| {
                    routes_from.agent_route_models(agent, active)
                }),
                token_path,
            ),
            profiles: ProfileOps::new(config_path, registry_agents),
            usage,
            keys,
            ops,
            router: None,
            shutdown,
            events: Arc::new(EventBus::new()),
            in_flight: Arc::new(AtomicU64::new(0)),
            quotas: None,
            runtime: None,
            route_trace: None,
        }
    }

    pub(crate) fn with_runtime(mut self, runtime: Arc<crate::runtime::Runtime>) -> Self {
        self.runtime = Some(runtime);
        self
    }

    /// Serves `/v1/hofund/route` from the shared routing trace book.
    pub(crate) fn with_route_trace(
        mut self,
        route_trace: Arc<hofund_router::RouteTraceBook>,
    ) -> Self {
        self.route_trace = Some(route_trace);
        self
    }

    /// Reads live reserve placement from the shared router snapshot.
    pub(crate) fn with_router(mut self, router: hofund_router::Snapshot) -> Self {
        self.router = Some(router);
        self
    }

    /// Serves `/v1/hofund/quotas` and its history from `quotas`.
    pub(crate) fn with_quotas(mut self, quotas: Arc<DaemonQuotas>) -> Self {
        self.quotas = Some(quotas);
        self
    }

    /// Serves `/v1/hofund/events` from `bus`.
    pub(crate) fn with_events(mut self, bus: Arc<EventBus>) -> Self {
        self.agents = self.agents.with_events(Arc::clone(&bus));
        self.events = bus;
        self
    }

    /// Shares the gateway in-flight counter with status and snapshots.
    pub(crate) fn with_in_flight(mut self, in_flight: Arc<AtomicU64>) -> Self {
        self.in_flight = in_flight;
        self
    }

    /// Builds snapshots from the same sources as [`ControlState::status`].
    fn snapshot_source(&self) -> SnapshotSource {
        let ops = Arc::clone(&self.ops);
        let cooldowns = Arc::clone(&self.cooldowns);
        let shutdown = self.shutdown.subscribe();
        let in_flight = Arc::clone(&self.in_flight);
        Arc::new(move || {
            let mut snapshot = SnapshotEvent::default();
            let live = ops.live();
            snapshot.disk_revision = ops.disk_revision();
            snapshot.runtime_revision.clone_from(&live.runtime_revision);
            snapshot.generation = live.generation;
            snapshot.pending_restart.clone_from(&live.pending_restart);
            snapshot.not_applied.clone_from(&live.not_applied);
            let rests = cooldowns.scoped_entries();
            snapshot.members = live
                .config
                .providers
                .iter()
                .map(|provider| {
                    let id = membership::member_id(&provider.id);
                    let mut member = MemberSummary::default();
                    member.ready = cooldowns.cooling(&id).is_none();
                    member.id = id.to_string();
                    member.resting_models = rests
                        .iter()
                        .filter(|(rest_id, model, _)| *rest_id == id && model.is_some())
                        .filter_map(|(_, model, _)| model.clone())
                        .collect();
                    member
                })
                .collect();
            let listener = ops.listener();
            snapshot.effective_listen = listener.effective.to_string();
            snapshot.listen_source = listener.source;
            snapshot.shutdown = if *shutdown.borrow() {
                ShutdownState::Draining
            } else {
                ShutdownState::Running
            };
            snapshot.in_flight = in_flight.load(Ordering::SeqCst);
            snapshot
        })
    }

    /// Points agent calls at `session` and publishes token rotations on
    /// `generation`. `None` keeps the process environment and a private channel.
    pub(crate) fn with_agent_hooks(
        mut self,
        session: Option<Session>,
        generation: Option<watch::Sender<u64>>,
    ) -> Self {
        self.agents = self
            .agents
            .with_session(session.clone())
            .with_token_generation(generation);
        self.profiles = self.profiles.with_session(session);
        self
    }

    /// The published configuration.
    fn config(&self) -> Arc<Config> {
        Arc::clone(&self.ops.live().config)
    }

    async fn mutate_gateway_key(
        &self,
        request: GatewayKeyRequest,
    ) -> Result<GatewayKeyResponse, ControlFailure> {
        let mut secret = None;
        let mut issued_id = None;
        match request.action {
            GatewayKeyAction::Add => {
                let added = self
                    .keys
                    .add_with_access(&request.name, request.secret.as_deref(), request.read_only)
                    .map_err(key_failure)?;
                secret = Some(added.secret);
            }
            GatewayKeyAction::Issue => {
                let seconds = request.ttl_seconds.unwrap_or(0.0);
                if !seconds.is_finite()
                    || seconds <= 0.0
                    || seconds > hofund_secrets::MAX_TTL.as_secs_f64()
                {
                    return Err(key_failure(hofund_secrets::SecretError::Key(
                        "ttl is a duration such as 30m, 1h or 24h".to_owned(),
                    )));
                }
                let issued = self
                    .keys
                    .issue(hofund_secrets::IssueRequest {
                        name: request.name,
                        ttl: std::time::Duration::from_secs_f64(seconds),
                        models: request.models,
                        accounts: request.accounts,
                        known_accounts: self.known_accounts().await?,
                        tokens: request.tokens,
                        cost: request.cost,
                        read_only: request.read_only,
                        issuer: request.id,
                    })
                    .map_err(key_failure)?;
                secret = Some(issued.key);
                issued_id = Some(issued.id);
            }
            GatewayKeyAction::Rotate => {
                secret = Some(self.keys.rotate(&request.id).map_err(key_failure)?.secret);
            }
            GatewayKeyAction::Rename => self
                .keys
                .rename(&request.id, &request.name)
                .map_err(key_failure)?,
            GatewayKeyAction::On => self
                .keys
                .set_enabled_by_name(&request.id, true)
                .map_err(key_failure)?,
            GatewayKeyAction::Off => self
                .keys
                .set_enabled_by_name(&request.id, false)
                .map_err(key_failure)?,
            GatewayKeyAction::Remove => self.keys.remove(&request.id).map_err(key_failure)?,
            GatewayKeyAction::Move => self
                .keys
                .move_key(&request.id, request.place.unwrap_or(0))
                .map_err(key_failure)?,
            GatewayKeyAction::Models => self
                .keys
                .set_models(&request.id, &request.models)
                .map_err(key_failure)?,
            GatewayKeyAction::Accounts => {
                let known = self.known_accounts().await?;
                self.keys
                    .set_accounts(&request.id, &request.accounts, &known)
                    .map_err(key_failure)?;
            }
            GatewayKeyAction::Quotas => self
                .keys
                .set_quotas(&request.id, request.quotas)
                .map_err(key_failure)?,
            GatewayKeyAction::Access => self
                .keys
                .set_read_only(&request.id, request.read_only)
                .map_err(key_failure)?,
            GatewayKeyAction::Issuer => self
                .keys
                .set_issuer(&request.id, request.issues)
                .map_err(key_failure)?,
            GatewayKeyAction::Limit => self
                .keys
                .set_limit(
                    &request.id,
                    request.limit.map(stored_limit),
                    request.index,
                    unix_now(),
                )
                .map_err(key_failure)?,
            GatewayKeyAction::ResetLimit => self
                .keys
                .reset_limits(&request.id, request.index, unix_now())
                .map_err(key_failure)?,
            GatewayKeyAction::Copy => {
                secret = Some(self.keys.copy(&request.id).map_err(key_failure)?);
            }
            _ => {
                return Err(ControlError::new(
                    ControlErrorKind::BadRequest,
                    "unknown gateway-key action",
                    "control",
                )
                .into());
            }
        }
        let mut response = key_response(self.keys.list(), secret, false)?;
        response.issued_id = issued_id;
        Ok(response)
    }

    async fn known_accounts(&self) -> Result<Vec<String>, ControlFailure> {
        let config = self.config();
        let mut known = Vec::new();
        for provider in &config.providers {
            let planned =
                super::plan_using(provider, |id| self.registry.find_preset(id), &self.registry)
                    .map_err(|error| internal(error.to_string()))?;
            let endpoint = super::endpoint_for(provider, &planned)
                .map_err(|error| internal(error.to_string()))?;
            let fingerprint = super::provider_fingerprint(provider, &endpoint)
                .map_err(|error| internal(error.to_string()))?;
            known.push(format!("{}/{fingerprint}", provider.id));
            known.push(format!(
                "{}/{}",
                provider.id,
                membership::member_id(&provider.id)
            ));
        }
        for subscription in &config.subscriptions {
            let plugin = hofund_plugin_api::PluginId::try_from(subscription.id.as_str())
                .map_err(|error| internal(error.to_string()))?;
            let host = hofund_plugin_host::runtime_host_for(
                hofund_plugin_api::PluginKind::Subscription,
                plugin,
                String::new(),
                super::beside(&self.config_path, ".hofund"),
                hofund_plugin_host::HttpLimits::new(config.connect_timeout, config.read_timeout),
            )
            .map_err(|error| internal(error.to_string()))?;
            let accounts = host
                .accounts()
                .list()
                .await
                .map_err(|error| internal(error.to_string()))?;
            for account in accounts {
                let id = format!("{}/{}", subscription.id, account.id());
                known.push(id.clone());
                known.push(format!("{id}\t{}", account.label()));
            }
        }
        Ok(known)
    }

    fn provider_exists(&self, id: &str) -> Result<(), ControlError> {
        if self
            .config()
            .providers
            .iter()
            .any(|provider| provider.id == id)
        {
            return Ok(());
        }
        Err(ControlError::new(
            ControlErrorKind::NotFound,
            format!("unknown provider `{id}`"),
            "control",
        ))
    }
}

impl ControlState for DaemonControl {
    fn health(&self) -> HealthResponse {
        let mut health = HealthResponse::default();
        health.daemon_version = env!("CARGO_PKG_VERSION").to_owned();
        health
    }

    fn status(&self) -> StatusResponse {
        let mut status = StatusResponse::default();
        status.pid = std::process::id();
        status.started_at_ms = self.started_at_ms;
        let live = self.ops.live();
        status.generation = live.generation;
        status.disk_revision = self.ops.disk_revision();
        status.runtime_revision.clone_from(&live.runtime_revision);
        status.pending_restart.clone_from(&live.pending_restart);
        status.not_applied.clone_from(&live.not_applied);
        status.in_flight = self.in_flight.load(Ordering::SeqCst);
        status.shutdown = if self.draining() {
            ShutdownState::Draining
        } else {
            ShutdownState::Running
        };
        let listener = self.ops.listener();
        status.model_listener.configured = listener.configured.to_string();
        status.model_listener.effective = listener.effective.to_string();
        status.model_listener.running = listener.running.map(|running| running.to_string());
        status.model_listener.effective_source = listener.source;
        status
    }

    fn draining(&self) -> bool {
        *self.shutdown.borrow()
    }

    fn providers(&self) -> ControlFuture<'_, ProviderListResponse> {
        let mut response = ProviderListResponse::default();
        let rests = self.cooldowns.scoped_entries();
        response.providers = self
            .config()
            .providers
            .iter()
            .map(|provider| {
                let cached = self.models.cached(&provider.id);
                let planned = self.models.planned(provider);
                let id = membership::member_id(&provider.id);
                let resting = self.cooldowns.cooling(&id).is_some();
                let mut summary = ProviderSummary::default();
                summary.id = provider.id.clone();
                summary.ready = planned.is_ok() && !resting;
                summary.model_count = u64::try_from(cached.models.len()).unwrap_or(u64::MAX);
                summary.resting_models = rests
                    .iter()
                    .filter(|(rest_id, model, _)| *rest_id == id && model.is_some())
                    .filter_map(|(_, model, _)| model.clone())
                    .collect();
                if let Ok(planned) = planned {
                    summary.kind = plugin_for(provider, &planned, &self.registry);
                    summary.protocol = protocol_of(planned.wire).to_owned();
                }
                summary
            })
            .collect();
        Box::pin(async move { Ok(response) })
    }

    fn provider_keys(&self, id: String) -> ControlFuture<'_, ProviderKeysResponse> {
        let result = self
            .provider_exists(&id)
            .map_err(ControlFailure::from)
            .and_then(|()| {
                let config = self.config();
                let provider = config
                    .providers
                    .iter()
                    .find(|provider| provider.id == id)
                    .expect("provider existence checked");
                super::provider_keys::list(provider, &self.cooldowns).map_err(daemon_failure)
            });
        Box::pin(async move { result })
    }

    fn change_provider_key(
        &self,
        id: String,
        request: ProviderKeyRequest,
    ) -> ControlFuture<'_, ProviderKeysResponse> {
        Box::pin(async move {
            self.provider_exists(&id)?;
            let current = self.ops.get().await?;
            let mutation =
                super::provider_keys::mutate(&current.text, &id, &request).map_err(|message| {
                    ControlFailure::from(ControlError::new(
                        ControlErrorKind::Unprocessable,
                        message,
                        "control",
                    ))
                })?;
            if mutation.changed {
                self.ops
                    .apply_with_key_moves(&current.disk_revision, mutation.text, mutation.moves)
                    .await?;
            }
            let config = self.config();
            let provider = config
                .providers
                .iter()
                .find(|provider| provider.id == id)
                .expect("provider existence checked");
            let mut response =
                super::provider_keys::list(provider, &self.cooldowns).map_err(daemon_failure)?;
            response.added = mutation.added;
            response.already_had = mutation.already_had;
            response.removed = mutation.removed;
            response.before = mutation.before;
            response.after = mutation.after;
            Ok(response)
        })
    }

    fn provider_spend_caps(&self, id: String) -> ControlFuture<'_, ProviderSpendCapsResponse> {
        Box::pin(async move {
            self.provider_exists(&id)?;
            let runtime = self.runtime.as_ref().ok_or_else(|| {
                ControlFailure::from(ControlError::new(
                    ControlErrorKind::BadRequest,
                    "spend caps require a running daemon",
                    "control",
                ))
            })?;
            let config = self.config();
            let provider = config
                .providers
                .iter()
                .find(|provider| provider.id == id)
                .expect("provider existence checked");
            let store = runtime.spend_caps();
            Ok(render_spend_caps(id, provider, &self.usage, store.as_ref()))
        })
    }

    fn change_provider_spend_cap(
        &self,
        id: String,
        request: ProviderSpendCapRequest,
    ) -> ControlFuture<'_, ProviderSpendCapsResponse> {
        Box::pin(async move {
            self.provider_exists(&id)?;
            if self
                .config()
                .subscriptions
                .iter()
                .any(|entry| entry.id == id)
            {
                return Err(ControlFailure::from(ControlError::new(
                    ControlErrorKind::Unprocessable,
                    "subscription accounts do not use provider key spend caps",
                    "control",
                )));
            }
            let Some(runtime) = self.runtime.as_ref() else {
                return Err(ControlFailure::from(ControlError::new(
                    ControlErrorKind::BadRequest,
                    "spend caps require a running daemon",
                    "control",
                )));
            };
            let store = runtime.spend_caps();
            let config = self.config();
            let provider = config
                .providers
                .iter()
                .find(|provider| provider.id == id)
                .expect("provider existence checked");
            super::spend_caps::ensure_key_exists(provider, &request.key_id).map_err(|message| {
                ControlFailure::from(ControlError::new(
                    ControlErrorKind::Unprocessable,
                    message,
                    "control",
                ))
            })?;
            if request.cap.trim().is_empty() {
                store.clear_key(&id, &request.key_id).map_err(|message| {
                    ControlFailure::from(ControlError::new(
                        ControlErrorKind::Unprocessable,
                        message,
                        "control",
                    ))
                })?;
            } else {
                let parsed =
                    super::spend_caps::parse_spend_cap(&request.cap).map_err(|message| {
                        ControlFailure::from(ControlError::new(
                            ControlErrorKind::Unprocessable,
                            message,
                            "control",
                        ))
                    })?;
                store
                    .set_key_spend_cap(&id, &request.key_id, parsed)
                    .map_err(|message| {
                        ControlFailure::from(ControlError::new(
                            ControlErrorKind::Unprocessable,
                            message,
                            "control",
                        ))
                    })?;
            }
            runtime.republish_spend_caps(&config.providers);
            Ok(render_spend_caps(id, provider, &self.usage, store.as_ref()))
        })
    }

    fn provider_models(&self, id: String) -> ControlFuture<'_, ModelListResponse> {
        let result = self.provider_exists(&id).map(|()| {
            let cached = self.models.cached(&id);
            let mut response = ModelListResponse::default();
            response.provider = id;
            response.fetched_at_ms = cached.fetched_at;
            response.delisted = cached.delisted;
            response.models = model_entries(&cached.models);
            response
        });
        Box::pin(async move { result.map_err(Into::into) })
    }

    fn refresh_models(&self, id: String) -> ControlFuture<'_, ModelListResponse> {
        Box::pin(async move {
            self.provider_exists(&id)?;
            let config = self.config();
            let refreshed = self
                .models
                .refresh(&config, &self.config_path, &id)
                .await
                .map_err(daemon_failure)?;
            let mut response = ModelListResponse::default();
            response.provider = id;
            response.fetched_at_ms = refreshed.fetched_at;
            response.delisted = refreshed.delisted;
            response.added = refreshed.added;
            response.removed = refreshed.removed;
            response.models = model_entries(&refreshed.models);
            Ok(response)
        })
    }

    fn test_provider(
        &self,
        id: String,
        request: ProviderTestRequest,
    ) -> ControlFuture<'_, ProviderTestResponse> {
        Box::pin(async move {
            self.provider_exists(&id)?;
            let named = request.models.unwrap_or_default();
            let limit = test_timeout(!named.is_empty());
            let config = self.config();
            let probes = tokio::time::timeout(
                limit,
                self.models.probe_timed(
                    &config,
                    &self.config_path,
                    &id,
                    named.iter().map(String::as_str).collect(),
                ),
            )
            .await
            .map_err(|_| {
                ControlFailure::from(ControlError::new(
                    ControlErrorKind::Timeout,
                    format!("testing provider `{id}` timed out"),
                    "control",
                ))
            })?
            .map_err(daemon_failure)?;
            let mut response = ProviderTestResponse::default();
            response.provider = id;
            response.results = probes.into_iter().map(test_result).collect();
            Ok(response)
        })
    }

    fn picks(&self, id: String) -> ControlFuture<'_, PicksResponse> {
        let result = self.provider_exists(&id).map(|()| {
            let (picks, revision) = self.models.picks_with_revision(&id);
            picks_response(&picks, revision)
        });
        Box::pin(async move { result.map_err(Into::into) })
    }

    fn set_picks(&self, id: String, request: PicksSetRequest) -> ControlFuture<'_, PicksResponse> {
        let result = self
            .provider_exists(&id)
            .and_then(|()| request.form())
            .map_err(ControlFailure::from)
            .and_then(|form| {
                self.models
                    .edit_picks(&id, Some(&request.expected_picks_revision), form)
                    .map_err(|error| picks_failure(&id, error))
            })
            .map(|(picks, revision)| picks_response(&picks, revision));
        Box::pin(async move { result })
    }

    fn rename_provider(
        &self,
        id: String,
        request: ProviderRenameRequest,
    ) -> ControlFuture<'_, ProviderRenameResponse> {
        Box::pin(async move {
            self.provider_exists(&id)?;
            let new_id = request.new_id.trim();
            if new_id.is_empty() {
                return Err(ControlFailure::from(ControlError::new(
                    ControlErrorKind::Unprocessable,
                    "a provider's new id must not be empty",
                    "control",
                )));
            }
            super::provider_rename::rename_provider(
                &self.ops,
                &self.models,
                &self.keys,
                self.registry.agents(),
                &id,
                new_id,
            )
            .await
        })
    }

    fn route_models(&self) -> ControlFuture<'_, RouteModelListResponse> {
        let config = self.config();
        let catalog = self.ops.model_catalog();
        let router = self.router.clone();
        Box::pin(async move {
            let visible: std::collections::BTreeSet<String> =
                if let Some(catalog) = catalog.as_ref() {
                    hofund_model_catalog::listable_route_models(
                        config.routes.iter().map(|route| route.model.clone()),
                        Some(catalog.as_ref()),
                        Some(catalog.visibility()),
                    )
                    .into_iter()
                    .collect()
                } else {
                    config
                        .routes
                        .iter()
                        .map(|route| route.model.clone())
                        .collect()
                };
            let mut response = RouteModelListResponse::default();
            response.models = config
                .routes
                .iter()
                .filter(|route| visible.contains(&route.model))
                .map(|route| {
                    let mut model = RouteModel::default();
                    model.id = route.model.clone();
                    model.routing = route.routing.clone();
                    model.stays = super::route_stays(&config, route).as_str().to_owned();
                    if let Some(router) = &router {
                        model.reserves = router
                            .load()
                            .route_reserves(&route.model)
                            .into_iter()
                            .map(|(member, at_once)| {
                                let mut reserve = RouteReserve::default();
                                reserve.member = member;
                                reserve.at_once = u64::try_from(at_once).unwrap_or(u64::MAX);
                                reserve
                            })
                            .collect();
                    }
                    model
                })
                .collect();
            Ok(response)
        })
    }

    fn disable_catalog_model(
        &self,
        request: CatalogModelRequest,
    ) -> ControlFuture<'_, CatalogDisabledListResponse> {
        let ops = Arc::clone(&self.ops);
        Box::pin(async move { ops.disable_catalog_model(&request.model).await })
    }

    fn enable_catalog_model(
        &self,
        request: CatalogModelRequest,
    ) -> ControlFuture<'_, CatalogDisabledListResponse> {
        let ops = Arc::clone(&self.ops);
        Box::pin(async move { ops.enable_catalog_model(&request.model).await })
    }

    fn route_decisions(&self, query: RouteQuery) -> ControlFuture<'_, RouteListResponse> {
        let Some(book) = self.route_trace.as_ref() else {
            return Box::pin(async {
                Err(ControlError::new(
                    ControlErrorKind::Internal,
                    "routing trace is not available",
                    "control",
                )
                .into())
            });
        };
        let wait = query.wait_secs.map(std::time::Duration::from_secs);
        let book = Arc::clone(book);
        Box::pin(async move {
            Ok(if let Some(wait) = wait {
                book.list_wait(query, wait)
            } else {
                book.list(&query)
            })
        })
    }

    fn route_session(
        &self,
        session: String,
        query: RouteQuery,
    ) -> ControlFuture<'_, SessionRouteResponse> {
        let Some(book) = self.route_trace.as_ref() else {
            return Box::pin(async {
                Err(ControlError::new(
                    ControlErrorKind::Internal,
                    "routing trace is not available",
                    "control",
                )
                .into())
            });
        };
        let wait = query
            .wait_secs
            .map(|secs| std::time::Duration::from_secs(secs.min(60)));
        let after = query.after.unwrap_or(0);
        let gateway_key = query.gateway_key.clone();
        let book = Arc::clone(book);
        Box::pin(async move {
            Ok(if let Some(wait) = wait {
                book.session_wait(&session, after, wait, gateway_key.as_deref())
            } else {
                book.session_latest(&session, gateway_key.as_deref())
            })
        })
    }

    fn route_unrouted(&self) -> ControlFuture<'_, UnroutedListResponse> {
        let Some(book) = self.route_trace.as_ref() else {
            return Box::pin(async {
                Err(ControlError::new(
                    ControlErrorKind::Internal,
                    "routing trace is not available",
                    "control",
                )
                .into())
            });
        };
        let book = Arc::clone(book);
        Box::pin(async move { Ok(book.unrouted()) })
    }

    fn agents(&self) -> ControlFuture<'_, AgentListResponse> {
        Box::pin(async move { self.agents.list().await.map_err(Into::into) })
    }

    fn profiles(&self) -> ControlFuture<'_, ProfileListResponse> {
        Box::pin(async move { self.profiles.list().await.map_err(Into::into) })
    }

    fn profile_detail(&self, name: String) -> ControlFuture<'_, ProfileDetailResponse> {
        Box::pin(async move { self.profiles.detail(name).await.map_err(Into::into) })
    }

    fn save_profile(
        &self,
        request: ProfileSaveRequest,
    ) -> ControlFuture<'_, ProfileDetailResponse> {
        let name = request.name.trim().to_owned();
        Box::pin(async move {
            self.profiles
                .save(request)
                .await
                .map_err(ControlFailure::from)?;
            self.profiles.detail(name).await.map_err(Into::into)
        })
    }

    fn switch_profile(
        &self,
        request: ProfileSwitchRequest,
    ) -> ControlFuture<'_, ProfileSwitchResponse> {
        Box::pin(async move { self.profiles.switch(request).await.map_err(Into::into) })
    }

    fn delete_profile(
        &self,
        request: ProfileDeleteRequest,
    ) -> ControlFuture<'_, ProfileListResponse> {
        Box::pin(async move {
            self.profiles
                .delete(request)
                .await
                .map_err(ControlFailure::from)?;
            self.profiles.list().await.map_err(Into::into)
        })
    }

    fn agent_status(&self) -> ControlFuture<'_, AgentStatusResponse> {
        Box::pin(async move { self.agents.status().await.map_err(Into::into) })
    }

    fn plan_agent(
        &self,
        id: String,
        request: AgentPlanRequest,
    ) -> ControlFuture<'_, AgentPlanResponse> {
        Box::pin(async move { self.agents.plan(id, request).await.map_err(Into::into) })
    }

    fn connect_agent(
        &self,
        id: String,
        request: AgentConnectRequest,
    ) -> ControlFuture<'_, AgentChangeResponse> {
        Box::pin(async move { self.agents.connect(id, request).await.map_err(Into::into) })
    }

    fn disconnect_agent(
        &self,
        id: String,
        request: AgentDisconnectRequest,
    ) -> ControlFuture<'_, AgentChangeResponse> {
        Box::pin(async move {
            self.agents
                .disconnect(id, request)
                .await
                .map_err(Into::into)
        })
    }

    fn set_agent_model(
        &self,
        id: String,
        request: AgentModelRequest,
    ) -> ControlFuture<'_, AgentChangeResponse> {
        Box::pin(async move { self.agents.set_model(id, request).await.map_err(Into::into) })
    }

    fn agent_models(&self, id: String) -> ControlFuture<'_, AgentModelsResponse> {
        Box::pin(async move {
            let active = self.agents.configured_model_id(&id).await;
            let (shown, hidden) = self.ops.agent_models(Some(&id), active.as_deref());
            let mut response = AgentModelsResponse::default();
            response.agent = id;
            response.shown = shown;
            response.hidden = hidden;
            Ok(response)
        })
    }

    fn set_agent_visible(
        &self,
        id: String,
        request: AgentVisibleSetRequest,
    ) -> ControlFuture<'_, ConfigResponse> {
        Box::pin(async move { self.ops.set_agent_visible(&id, request.names).await })
    }

    fn set_agent_hidden_models(
        &self,
        id: String,
        request: AgentHiddenSetRequest,
    ) -> ControlFuture<'_, ConfigResponse> {
        Box::pin(async move { self.ops.set_agent_hidden_models(&id, request.ids).await })
    }

    fn set_agent_picked_only(
        &self,
        id: String,
        request: AgentPickedOnlyRequest,
    ) -> ControlFuture<'_, ConfigResponse> {
        Box::pin(async move { self.ops.set_agent_picked_only(&id, request.only).await })
    }

    fn quotas(&self, query: QuotaQuery) -> ControlFuture<'_, QuotaResponse> {
        Box::pin(async move {
            let quotas = self.quotas.as_ref().ok_or_else(quotas_unavailable)?;
            Ok::<_, ControlFailure>(quotas.response(&query).await)
        })
    }

    fn quota_history(&self, query: QuotaHistoryQuery) -> ControlFuture<'_, QuotaHistoryResponse> {
        Box::pin(async move {
            let quotas = self.quotas.as_ref().ok_or_else(quotas_unavailable)?;
            Ok::<_, ControlFailure>(quotas.history_response(&query))
        })
    }

    fn quota_credits_modes(&self) -> ControlFuture<'_, QuotaCreditsModesResponse> {
        let runtime = self.runtime.clone();
        Box::pin(async move {
            let Some(runtime) = runtime else {
                return Err(internal("quota credits modes are not available".to_owned()).into());
            };
            Ok(crate::credits_modes::list(runtime.credits_modes()))
        })
    }

    fn set_quota_credits(
        &self,
        request: QuotaCreditsSetRequest,
    ) -> ControlFuture<'_, QuotaCreditsSetResponse> {
        let runtime = self.runtime.clone();
        Box::pin(async move {
            let Some(runtime) = runtime else {
                return Err(internal("quota credits settings are not available".to_owned()).into());
            };
            crate::credits_modes::set(&runtime, runtime.credits_modes(), request).await
        })
    }

    fn quota_alerts(&self) -> ControlFuture<'_, QuotaAlertsResponse> {
        let runtime = self.runtime.clone();
        Box::pin(async move {
            let Some(runtime) = runtime else {
                return Err(internal("quota alerts are not available".to_owned()).into());
            };
            Ok(crate::quota_alerts::list(runtime.quota_alerts()))
        })
    }

    fn set_quota_alerts(
        &self,
        request: QuotaAlertsSetRequest,
    ) -> ControlFuture<'_, QuotaAlertsSetResponse> {
        let runtime = self.runtime.clone();
        Box::pin(async move {
            let Some(runtime) = runtime else {
                return Err(internal("quota alert settings are not available".to_owned()).into());
            };
            crate::quota_alerts::set(runtime.quota_alerts(), runtime.alert_wake(), request)
        })
    }

    fn warmups(&self) -> ControlFuture<'_, WarmupListResponse> {
        let runtime = self.runtime.clone();
        let registry = Arc::clone(&self.registry);
        Box::pin(async move {
            let Some(runtime) = runtime else {
                return Err(internal("warm-up status is not available".to_owned()).into());
            };
            let rows = crate::warmup::list_statuses(&runtime, registry.as_ref(), runtime.warmup())
                .into_iter()
                .map(warmup_row)
                .collect();
            let mut response = WarmupListResponse::default();
            response.accounts = rows;
            Ok(response)
        })
    }

    fn run_warmup(&self, request: WarmupRunRequest) -> ControlFuture<'_, WarmupRunResponse> {
        let runtime = self.runtime.clone();
        let registry = Arc::clone(&self.registry);
        Box::pin(async move {
            let Some(runtime) = runtime else {
                return Err(internal("warm-up is not available".to_owned()).into());
            };
            let record = crate::warmup::run_now(
                &runtime,
                runtime.warmup(),
                registry.as_ref(),
                &request.subscription,
                &request.account,
            )
            .await
            .map_err(|message| {
                ControlFailure::from(ControlError::new(
                    ControlErrorKind::Unprocessable,
                    message,
                    "warmup",
                ))
            })?;
            Ok(warmup_run_response(record))
        })
    }

    fn checkins(&self) -> ControlFuture<'_, CheckinListResponse> {
        let runtime = self.runtime.clone();
        let registry = Arc::clone(&self.registry);
        Box::pin(async move {
            let Some(runtime) = runtime else {
                return Err(internal("check-in status is not available".to_owned()).into());
            };
            let rows =
                crate::checkin::list_statuses(&runtime, registry.as_ref(), runtime.checkin())
                    .into_iter()
                    .map(checkin_row)
                    .collect();
            let mut response = CheckinListResponse::default();
            response.accounts = rows;
            Ok(response)
        })
    }

    fn run_checkin(&self, request: CheckinRunRequest) -> ControlFuture<'_, CheckinRunResponse> {
        let runtime = self.runtime.clone();
        let registry = Arc::clone(&self.registry);
        Box::pin(async move {
            let Some(runtime) = runtime else {
                return Err(internal("check-in is not available".to_owned()).into());
            };
            let record = crate::checkin::run_now(
                &runtime,
                runtime.checkin(),
                registry.as_ref(),
                &request.subscription,
                &request.account,
            )
            .await
            .map_err(|message| {
                ControlFailure::from(ControlError::new(
                    ControlErrorKind::Unprocessable,
                    message,
                    "checkin",
                ))
            })?;
            Ok(checkin_run_response(record))
        })
    }

    fn usage(&self, query: UsageQuery) -> ControlFuture<'_, UsageResponse> {
        let ledger = Arc::clone(&self.usage);
        let config = self.ops.live().config.clone();
        Box::pin(async move {
            let (since, period, window_label) = resolve_usage_query(&query)?;
            let model_owned = query.model.clone();
            let account_owned = query.account.clone();
            let row_limit = if query.unlimited {
                usize::MAX
            } else {
                USAGE_ROW_LIMIT
            };
            if let Some(by) = query.by {
                let dimension = breakdown_dimension(by)?;
                let ledger_for_groups = Arc::clone(&ledger);
                let since_for_groups = since;
                let model_filter = model_owned.clone();
                let account_filter = account_owned.clone();
                let groups = tokio::task::spawn_blocking(move || {
                    ledger_for_groups.breakdown(
                        since_for_groups,
                        dimension,
                        model_filter.as_deref(),
                        account_filter.as_deref(),
                    )
                })
                .await
                .map_err(|error| internal(format!("usage breakdown task failed: {error}")))?
                .map_err(|error| internal(format!("usage breakdown failed: {error}")))?;
                let ledger_for_summary = Arc::clone(&ledger);
                let model_filter = model_owned.clone();
                let account_filter = account_owned.clone();
                let summary = tokio::task::spawn_blocking(move || {
                    ledger_for_summary.summary_filtered(
                        since,
                        model_filter.as_deref(),
                        account_filter.as_deref(),
                        row_limit,
                    )
                })
                .await
                .map_err(|error| internal(format!("usage query task failed: {error}")))?
                .map_err(|error| internal(format!("usage query failed: {error}")))?;
                let ledger_for_speed = Arc::clone(&ledger);
                let model_filter = model_owned.clone();
                let account_filter = account_owned.clone();
                let all_rows = tokio::task::spawn_blocking(move || {
                    ledger_for_speed
                        .summary_filtered(
                            since,
                            model_filter.as_deref(),
                            account_filter.as_deref(),
                            usize::MAX,
                        )
                        .map(|(rows, _)| rows)
                })
                .await
                .map_err(|error| internal(format!("usage query task failed: {error}")))?
                .map_err(|error| internal(format!("usage query failed: {error}")))?;
                return Ok(usage_breakdown_response(
                    period,
                    by,
                    groups,
                    summary,
                    &all_rows,
                    &config,
                    window_label,
                ));
            }
            let ledger_for_summary = Arc::clone(&ledger);
            let model_filter = model_owned.clone();
            let account_filter = account_owned.clone();
            let summary = tokio::task::spawn_blocking(move || {
                ledger_for_summary.summary_filtered(
                    since,
                    model_filter.as_deref(),
                    account_filter.as_deref(),
                    row_limit,
                )
            })
            .await
            .map_err(|error| internal(format!("usage query task failed: {error}")))?
            .map_err(|error| internal(format!("usage query failed: {error}")))?;
            let ledger_for_speed = Arc::clone(&ledger);
            let model_filter = model_owned;
            let account_filter = account_owned;
            let all_rows = tokio::task::spawn_blocking(move || {
                ledger_for_speed
                    .summary_filtered(
                        since,
                        model_filter.as_deref(),
                        account_filter.as_deref(),
                        usize::MAX,
                    )
                    .map(|(rows, _)| rows)
            })
            .await
            .map_err(|error| internal(format!("usage query task failed: {error}")))?
            .map_err(|error| internal(format!("usage query failed: {error}")))?;
            Ok::<_, ControlFailure>(usage_response(
                period,
                summary,
                &all_rows,
                &config,
                window_label,
            ))
        })
    }

    fn gateway_keys(&self) -> ControlFuture<'_, GatewayKeyResponse> {
        Box::pin(async move { key_response(self.keys.list(), None, false) })
    }

    fn change_gateway_key(
        &self,
        request: GatewayKeyRequest,
    ) -> ControlFuture<'_, GatewayKeyResponse> {
        Box::pin(self.mutate_gateway_key(request))
    }

    fn export_gateway_keys(&self) -> ControlFuture<'_, GatewayKeyResponse> {
        Box::pin(async move { key_response(self.keys.export(), None, true) })
    }

    fn restore_gateway_keys(
        &self,
        request: GatewayKeyRestoreRequest,
    ) -> ControlFuture<'_, GatewayKeyResponse> {
        Box::pin(async move {
            let keys = request.keys.into_iter().map(stored_key).collect::<Vec<_>>();
            self.keys.restore(&keys).map_err(key_failure)?;
            key_response(self.keys.list(), None, false)
        })
    }

    fn rotate_token(&self, _request: TokenRotateRequest) -> ControlFuture<'_, TokenRotateResponse> {
        Box::pin(async move { self.agents.rotate_token().await.map_err(Into::into) })
    }

    fn shutdown(&self, _request: ShutdownRequest) -> ControlFuture<'_, ShutdownResponse> {
        // `send_replace` stores the flag even when the runner has not
        // subscribed, and a repeat request leaves it set.
        self.shutdown.send_replace(true);
        let mut response = ShutdownResponse::default();
        response.draining = true;
        Box::pin(async move { Ok(response) })
    }

    fn config(&self) -> ControlFuture<'_, ConfigResponse> {
        Box::pin(self.ops.get())
    }

    fn validate_config(
        &self,
        request: ConfigValidateRequest,
    ) -> ControlFuture<'_, ConfigValidationResponse> {
        Box::pin(async move { Ok(self.ops.validate(&request.text).await) })
    }

    fn apply_config(&self, request: ConfigApplyRequest) -> ControlFuture<'_, ConfigResponse> {
        Box::pin(async move {
            self.ops
                .apply(&request.expected_revision, request.text)
                .await
        })
    }

    fn reload_config(&self, request: ReloadRequest) -> ControlFuture<'_, ConfigResponse> {
        Box::pin(async move {
            self.ops
                .reload(Some(&request.expected_revision), ReloadOrigin::Explicit)
                .await
        })
    }

    fn events(&self, query: EventsQuery) -> Result<EventSubscription, ControlFailure> {
        Ok(self.events.subscribe(&query, self.snapshot_source()))
    }
}

fn key_response(
    result: Result<Vec<hofund_secrets::Key>, hofund_secrets::SecretError>,
    secret: Option<String>,
    backup: bool,
) -> Result<GatewayKeyResponse, ControlFailure> {
    let keys = result.map_err(key_failure)?;
    let mut response = GatewayKeyResponse::default();
    response.keys = keys.into_iter().map(|key| wire_key(key, backup)).collect();
    response.secret = secret;
    Ok(response)
}

fn wire_key(key: hofund_secrets::Key, backup: bool) -> GatewayKey {
    let mut out = GatewayKey::default();
    out.id = key.id;
    out.name = key.name;
    out.off = key.off;
    out.is_default = key.is_default;
    out.masked = key.masked;
    out.secret = if backup { key.secret } else { String::new() };
    out.models = key.models;
    out.accounts = key.accounts;
    out.quotas = key.quotas;
    out.read_only = key.read_only;
    out.expires = key.expires;
    out.issuer = key.issuer;
    out.issues = key.issues;
    out.limits = key.limits.into_iter().map(wire_limit).collect();
    out.removed = backup && key.removed;
    out
}

fn wire_limit(limit: KeyLimit) -> GatewayKeyLimit {
    let mut out = GatewayKeyLimit::default();
    out.period = limit.period;
    out.days = limit.days;
    out.hours = limit.hours;
    out.tokens = limit.tokens;
    out.cost = limit.cost;
    out.cache_reads = limit.cache_reads;
    out.since = limit.since;
    out
}

fn stored_key(key: GatewayKey) -> hofund_secrets::Key {
    hofund_secrets::Key {
        id: key.id,
        name: key.name,
        off: key.off,
        is_default: key.is_default,
        secret: key.secret,
        masked: String::new(),
        models: key.models,
        accounts: key.accounts,
        quotas: key.quotas,
        read_only: key.read_only,
        expires: key.expires,
        issuer: key.issuer,
        issues: key.issues,
        limits: key.limits.into_iter().map(stored_limit).collect(),
        removed: key.removed,
    }
}

fn stored_limit(limit: GatewayKeyLimit) -> KeyLimit {
    KeyLimit {
        period: limit.period,
        days: limit.days,
        hours: limit.hours,
        tokens: limit.tokens,
        cost: limit.cost,
        cache_reads: limit.cache_reads,
        since: limit.since,
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
        })
}

fn key_failure(error: hofund_secrets::SecretError) -> ControlFailure {
    let kind = match error {
        hofund_secrets::SecretError::Key(_) => ControlErrorKind::Unprocessable,
        _ => ControlErrorKind::Internal,
    };
    ControlError::new(kind, error.to_string(), "gateway_keys").into()
}

fn model_entries(models: &[ModelSpec]) -> Vec<ModelEntry> {
    models
        .iter()
        .map(|model| {
            let mut entry = ModelEntry::default();
            entry.id = model.id().to_owned();
            entry.display_name = model.display_name().map(str::to_owned);
            entry.context = (model.context() != 0).then(|| model.context());
            entry.max_output = (model.max_output() != 0).then(|| model.max_output());
            entry
        })
        .collect()
}

fn picks_response(picks: &Picks, revision: String) -> PicksResponse {
    let mut response = PicksResponse::default();
    response.picks = picks.picks().to_vec();
    response.picked_from = picks.picked_from().to_vec();
    response.delisted = picks.delisted().to_vec();
    response.picks_revision = revision;
    response
}

fn picks_failure(id: &str, error: PicksError) -> ControlFailure {
    let error = match error {
        PicksError::Stale => ControlError::new(
            ControlErrorKind::Conflict,
            format!("the picks of provider `{id}` changed since they were read"),
            "control",
        )
        .with_details(FieldIssue::new(
            "expected_picks_revision",
            "stale",
            "read the picks again and retry",
        )),
        PicksError::Invalid(issue) => ControlError::new(
            ControlErrorKind::Unprocessable,
            issue.message.clone(),
            "control",
        )
        .with_details(issue),
        PicksError::Save(error) => ControlError::new(
            ControlErrorKind::Internal,
            format!("the picks of provider `{id}` were not saved: {error}"),
            "control",
        ),
    };
    error.into()
}

/// Maps a daemon failure to its control error, keeping an upstream status
/// so a rate limit answers 429.
fn daemon_failure(error: DaemonError) -> ControlFailure {
    let message = error.to_string();
    let retry_after = error.retry_after();
    let (kind, status) = match error.kind() {
        ErrorKind::NotFound => (ControlErrorKind::NotFound, None),
        ErrorKind::Selection => (ControlErrorKind::Unprocessable, None),
        ErrorKind::Timeout => (ControlErrorKind::Timeout, None),
        ErrorKind::Internal => (ControlErrorKind::Internal, None),
        ErrorKind::Upstream { kind, status } => {
            let wire = control_map::provider_error(kind, status, retry_after);
            (wire.kind, Some(wire.http_status))
        }
    };
    let mut control = ControlError::new(kind, message, "control");
    control.retry_after_ms = retry_after.map(control_map::round_up_ms);
    ControlFailure {
        error: control,
        status,
    }
}

fn test_result(timed: Timed) -> ProviderTestResult {
    let mut result = ProviderTestResult::default();
    result.model = timed.model;
    result.latency_ms = Some(u64::try_from(timed.latency.as_millis()).unwrap_or(u64::MAX));
    match timed.result {
        Ok(outcome) => {
            result.model = outcome.model;
            result.ok = true;
        }
        Err(error) => {
            if let ErrorKind::Upstream { status, .. } = error.kind() {
                result.status = status;
            }
            result.error = Some(daemon_failure(error).error);
        }
    }
    result
}

trait WithDetails {
    fn with_details(self, issue: FieldIssue) -> Self;
}

impl WithDetails for ControlError {
    fn with_details(mut self, issue: FieldIssue) -> Self {
        self.details.push(issue);
        self
    }
}

/// The most rows one usage response carries. Totals still cover the whole
/// period; `rows_truncated` reports that older rows were left out.
pub(crate) const USAGE_ROW_LIMIT: usize = 10_000;

fn window_of(period: UsagePeriod) -> Result<Window, ControlError> {
    match period {
        UsagePeriod::Day => Ok(Window::Today),
        UsagePeriod::Week => Ok(Window::Week),
        UsagePeriod::Month => Ok(Window::Month),
        UsagePeriod::All => Ok(Window::All),
        _ => Err(ControlError::new(
            ControlErrorKind::BadRequest,
            "unknown usage period; expected day, week, month, or all",
            "validate",
        )),
    }
}

fn resolve_usage_query(
    query: &UsageQuery,
) -> Result<(Option<i64>, UsagePeriod, String), ControlFailure> {
    use chrono::Local;
    use hofund_usage::{ReportWindow, parse_days, parse_last, parse_since};
    let now = Local::now();
    if let Some(value) = query.days.as_deref().filter(|value| !value.is_empty()) {
        let duration = parse_days(value).map_err(usage_bad_request)?;
        return Ok((
            ReportWindow::Rolling(duration).since_unix(now),
            UsagePeriod::All,
            format!("last {value} days"),
        ));
    }
    if let Some(value) = query.last.as_deref().filter(|value| !value.is_empty()) {
        let duration = parse_last(value).map_err(usage_bad_request)?;
        return Ok((
            ReportWindow::Rolling(duration).since_unix(now),
            UsagePeriod::All,
            format!("last {value}"),
        ));
    }
    if let Some(value) = query.since.as_deref().filter(|value| !value.is_empty()) {
        let since = parse_since(value).map_err(usage_bad_request)?;
        return Ok((Some(since), UsagePeriod::All, format!("since {value}")));
    }
    let period = query.period;
    let window = window_of(period)?;
    Ok((window.since_now(), period, String::new()))
}

fn usage_bad_request(message: String) -> ControlFailure {
    ControlError::new(ControlErrorKind::BadRequest, message, "validate").into()
}

fn breakdown_dimension(
    by: UsageBreakdownBy,
) -> Result<hofund_usage::BreakdownDimension, ControlFailure> {
    Ok(match by {
        UsageBreakdownBy::Agent => hofund_usage::BreakdownDimension::Agent,
        UsageBreakdownBy::ProviderKey => hofund_usage::BreakdownDimension::ProviderKey,
        UsageBreakdownBy::GatewayKey => hofund_usage::BreakdownDimension::GatewayKey,
        UsageBreakdownBy::Model => hofund_usage::BreakdownDimension::Model,
        UsageBreakdownBy::Provider => hofund_usage::BreakdownDimension::Provider,
        UsageBreakdownBy::Account => hofund_usage::BreakdownDimension::Account,
        UsageBreakdownBy::Session => hofund_usage::BreakdownDimension::Session,
        UsageBreakdownBy::Day => hofund_usage::BreakdownDimension::Day,
        UsageBreakdownBy::Unknown => {
            return Err(ControlError::new(
                ControlErrorKind::BadRequest,
                "unknown usage breakdown; expected agent, provider-key, gateway-key, model, provider, account, session, or day",
                "validate",
            )
            .into());
        }
        _ => {
            return Err(ControlError::new(
                ControlErrorKind::BadRequest,
                "unknown usage breakdown; expected agent, provider-key, gateway-key, model, provider, account, session, or day",
                "validate",
            )
            .into());
        }
    })
}

fn usage_breakdown_response(
    period: UsagePeriod,
    by: UsageBreakdownBy,
    groups: Vec<hofund_usage::BreakdownGroup>,
    (rows, totals): (Vec<hofund_usage::Row>, hofund_usage::Totals),
    all_rows: &[hofund_usage::Row],
    config: &hofund_config::Config,
    window_label: String,
) -> UsageResponse {
    let mut response = usage_response(period, (rows, totals), all_rows, config, window_label);
    response.breakdown_by = Some(by);
    let convert = |usd: f64| match config.usage.currency {
        hofund_config::UsageCurrency::Usd => usd,
        hofund_config::UsageCurrency::Cny => {
            usd * config
                .usage
                .cny_per_usd
                .filter(|rate| rate.is_finite() && *rate > 0.0)
                .unwrap_or(hofund_usage::CnyRate::FALLBACK)
        }
    };
    response.breakdown = groups
        .into_iter()
        .map(|group| {
            let group_rows: Vec<_> = all_rows
                .iter()
                .filter(|row| breakdown_group_id(row, by) == group.id)
                .cloned()
                .collect();
            let speed = hofund_usage::speed_totals(&group_rows);
            let mut out = UsageBreakdownGroup::default();
            out.id = group.id;
            out.label = group.label;
            out.requests = group.totals.requests;
            out.input_tokens = group.totals.input_tokens;
            out.output_tokens = group.totals.output_tokens;
            out.cost = convert(group.totals.cost_usd);
            out.unpriced = group.totals.unpriced;
            out.mean_ttft_ms = speed.mean_ttft_ms;
            out.mean_tokens_per_second = speed.mean_tokens_per_second;
            out
        })
        .collect();
    response
}

fn usage_response(
    period: UsagePeriod,
    (rows, totals): (Vec<hofund_usage::Row>, hofund_usage::Totals),
    all_rows: &[hofund_usage::Row],
    config: &hofund_config::Config,
    window_label: String,
) -> UsageResponse {
    let mut response = UsageResponse::default();
    response.period = period;
    response.window_label = window_label;
    response.rows_truncated = totals.requests > u64::try_from(rows.len()).unwrap_or(u64::MAX);
    let cny_per_usd = config
        .usage
        .cny_per_usd
        .filter(|rate| rate.is_finite() && *rate > 0.0)
        .unwrap_or(hofund_usage::CnyRate::FALLBACK);
    let convert = |usd: f64| match config.usage.currency {
        hofund_config::UsageCurrency::Usd => usd,
        hofund_config::UsageCurrency::Cny => usd * cny_per_usd,
    };
    response.currency = match config.usage.currency {
        hofund_config::UsageCurrency::Usd => "USD".to_owned(),
        hofund_config::UsageCurrency::Cny => "CNY".to_owned(),
    };
    if config.usage.currency == hofund_config::UsageCurrency::Cny {
        response.cny_per_usd = Some(cny_per_usd);
    }
    response.rows = rows
        .into_iter()
        .map(|row| usage_row(row, convert))
        .collect();
    let mut summary = UsageTotals::default();
    summary.requests = totals.requests;
    summary.input_tokens = totals.input_tokens;
    summary.output_tokens = totals.output_tokens;
    summary.cache_read_tokens = totals.cache_read_tokens;
    summary.cache_write_5m_tokens = totals.cache_write_5m_tokens;
    summary.cache_write_1h_tokens = totals.cache_write_1h_tokens;
    summary.reasoning_tokens = totals.reasoning_tokens;
    summary.cost = convert(totals.cost_usd);
    summary.unpriced = totals.unpriced;
    let speed = hofund_usage::speed_totals(all_rows);
    summary.mean_ttft_ms = speed.mean_ttft_ms;
    summary.mean_tokens_per_second = speed.mean_tokens_per_second;
    response.totals = summary;
    response
}

fn breakdown_group_id(row: &hofund_usage::Row, by: UsageBreakdownBy) -> String {
    match by {
        UsageBreakdownBy::Agent => hofund_usage::normalize_client_agent(&row.client_agent),
        UsageBreakdownBy::ProviderKey => {
            if row.provider_key_id.is_empty() {
                hofund_usage::NO_PROVIDER_KEY.to_owned()
            } else {
                row.provider_key_id.clone()
            }
        }
        UsageBreakdownBy::GatewayKey => {
            if row.caller_key_id.is_empty() {
                hofund_usage::NO_GATEWAY_KEY.to_owned()
            } else {
                row.caller_key_id.clone()
            }
        }
        UsageBreakdownBy::Model => row.model.clone(),
        UsageBreakdownBy::Provider => row.upstream.clone(),
        UsageBreakdownBy::Account => {
            if row.upstream.starts_with("account:") {
                row.upstream.clone()
            } else {
                "none".to_owned()
            }
        }
        UsageBreakdownBy::Session => {
            if row.session_id.is_empty() {
                "none".to_owned()
            } else {
                row.session_id.clone()
            }
        }
        UsageBreakdownBy::Day => chrono::Local
            .timestamp_opt(row.created_unix, 0)
            .single()
            .map(|time| time.date_naive().to_string())
            .unwrap_or_else(|| "unknown".to_owned()),
        UsageBreakdownBy::Unknown => String::new(),
        _ => String::new(),
    }
}

fn usage_row(row: hofund_usage::Row, convert: impl Fn(f64) -> f64 + Copy) -> UsageRow {
    let tokens_per_second = hofund_usage::row_tokens_per_second(&row);
    let mut out = UsageRow::default();
    out.created_at_ms = u64::try_from(row.created_unix)
        .unwrap_or(0)
        .saturating_mul(1000);
    out.model = row.model.clone();
    out.provider = row.upstream.clone();
    out.input_tokens = row
        .input_tokens
        .map(|tokens| u64::try_from(tokens).unwrap_or(0));
    out.output_tokens = row
        .output_tokens
        .map(|tokens| u64::try_from(tokens).unwrap_or(0));
    out.duration_ms = u64::try_from(row.latency_ms).unwrap_or(0);
    out.status = row.status.clone();
    out.caller_key_id = row.caller_key_id;
    out.caller_key_name = row.caller_key_name;
    out.caller_issuer_id = row.caller_issuer_id;
    out.cache_read_tokens = row
        .cache_read_tokens
        .map(|tokens| u64::try_from(tokens).unwrap_or(0));
    out.cache_write_5m_tokens = row
        .cache_write_5m_tokens
        .map(|tokens| u64::try_from(tokens).unwrap_or(0));
    out.cache_write_1h_tokens = row
        .cache_write_1h_tokens
        .map(|tokens| u64::try_from(tokens).unwrap_or(0));
    out.reasoning_tokens = row
        .reasoning_tokens
        .map(|tokens| u64::try_from(tokens).unwrap_or(0));
    out.reasoning_hidden = row.reasoning_hidden;
    out.priced = row.priced;
    out.cost_usd = row.cost_usd.filter(|_| row.priced);
    out.cost = out.cost_usd.map(convert);
    out.outcome = row.outcome.clone();
    out.provider_key_id = row.provider_key_id.clone();
    out.served_model = row.served_model.clone();
    out.client_agent = row.client_agent;
    out.ttft_ms = row.ttft_ms.and_then(|ms| u64::try_from(ms).ok());
    out.tokens_per_second = tokens_per_second;
    out.account = hofund_usage::account_of_upstream(&row.upstream)
        .unwrap_or_default()
        .to_owned();
    out.session_id = row.session_id;
    out
}

fn quotas_unavailable() -> ControlFailure {
    ControlError::new(
        ControlErrorKind::Unavailable,
        "quota reports are not served by this daemon",
        "quotas",
    )
    .into()
}

fn internal(message: String) -> ControlError {
    ControlError::new(ControlErrorKind::Internal, message, "usage")
}

fn warmup_row(status: hofund_plugin_api::WarmupAccountStatus) -> WarmupAccountRow {
    let mut row = WarmupAccountRow::default();
    row.subscription = status.subscription().to_owned();
    row.account = status.account().to_string();
    if let Some(last) = status.last() {
        row.outcome = outcome_name(last.outcome()).to_owned();
        row.detail = last.detail().to_owned();
        row.at_ms = last
            .at()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(0));
    }
    row
}

fn warmup_run_response(record: hofund_plugin_api::WarmupRecord) -> WarmupRunResponse {
    let mut response = WarmupRunResponse::default();
    response.outcome = outcome_name(record.outcome()).to_owned();
    response.detail = record.detail().to_owned();
    response.at_ms = record
        .at()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .unwrap_or(0);
    response
}

const fn outcome_name(outcome: WarmupOutcome) -> &'static str {
    match outcome {
        WarmupOutcome::Ok => "ok",
        WarmupOutcome::Skipped => "skipped",
        WarmupOutcome::Failed => "failed",
        _ => "failed",
    }
}

fn checkin_row(status: hofund_plugin_api::CheckinAccountStatus) -> CheckinAccountRow {
    let mut row = CheckinAccountRow::default();
    row.subscription = status.subscription().to_owned();
    row.account = status.account().to_string();
    if let Some(last) = status.last() {
        row.outcome = checkin_outcome_name(last.outcome()).to_owned();
        row.detail = last.detail().to_owned();
        row.at_ms = last
            .at()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(0));
    }
    row
}

fn checkin_run_response(record: hofund_plugin_api::CheckinRecord) -> CheckinRunResponse {
    let mut response = CheckinRunResponse::default();
    response.outcome = checkin_outcome_name(record.outcome()).to_owned();
    response.detail = record.detail().to_owned();
    response.at_ms = record
        .at()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .unwrap_or(0);
    response
}

fn checkin_outcome_name(outcome: CheckinOutcome) -> &'static str {
    outcome.as_str()
}

fn render_spend_caps(
    provider_id: String,
    _provider: &hofund_config::Provider,
    usage: &Ledger,
    store: &crate::spend_caps::SpendCapStore,
) -> ProviderSpendCapsResponse {
    let caps = store
        .list()
        .into_iter()
        .filter(|cap| cap.provider == provider_id)
        .map(|cap| {
            let period = match cap.period {
                crate::spend_caps::StoredPeriod::Day => hofund_usage::SpendCapPeriod::Day,
                crate::spend_caps::StoredPeriod::Month => hofund_usage::SpendCapPeriod::Month,
                crate::spend_caps::StoredPeriod::Total => hofund_usage::SpendCapPeriod::Total,
            };
            let since = hofund_usage::spend_cap_since(period, cap.set_at);
            let spent_usd = usage.provider_key_cost(&cap.key_id, since).unwrap_or(0.0);
            ProviderSpendCap {
                provider: cap.provider,
                key_id: cap.key_id,
                usd: cap.usd,
                period: match cap.period {
                    crate::spend_caps::StoredPeriod::Day => ProviderSpendCapPeriod::Day,
                    crate::spend_caps::StoredPeriod::Month => ProviderSpendCapPeriod::Month,
                    crate::spend_caps::StoredPeriod::Total => ProviderSpendCapPeriod::Total,
                },
                set_at: cap.set_at,
                spent_usd,
            }
        })
        .collect();
    ProviderSpendCapsResponse {
        version: hofund_control_proto::API_VERSION,
        caps,
    }
}

pub(crate) fn unix_milliseconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

pub(crate) fn revision(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
