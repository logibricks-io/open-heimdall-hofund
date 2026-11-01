use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use hofund_agent::Session;
use hofund_config::Config;
use hofund_control_proto::{
    AgentChangeResponse, AgentConnectRequest, AgentDisconnectRequest, AgentListResponse,
    AgentModelRequest, AgentPlanRequest, AgentPlanResponse, AgentStatusResponse, ControlError,
    ControlErrorKind, FieldIssue, GatewayKey, GatewayKeyAction, GatewayKeyLimit, GatewayKeyRequest,
    GatewayKeyResponse, GatewayKeyRestoreRequest, HealthResponse, ModelEntry, ModelListResponse,
    PicksResponse, PicksSetRequest, ProviderKeyRequest, ProviderKeysResponse, ProviderListResponse,
    ProviderSummary, ProviderTestRequest, ProviderTestResponse, ProviderTestResult,
    QuotaHistoryQuery, QuotaHistoryResponse, QuotaQuery, QuotaResponse, RouteModel,
    RouteModelListResponse, RouteReserve, ShutdownRequest, ShutdownResponse, ShutdownState,
    StatusResponse, TokenRotateRequest, TokenRotateResponse, UsagePeriod, UsageQuery,
    UsageResponse, UsageRow, UsageTotals,
};
use hofund_control_proto::{
    ConfigApplyRequest, ConfigResponse, ConfigValidateRequest, ConfigValidationResponse,
    MemberSummary, ReloadRequest, SnapshotEvent,
};
use hofund_gateway::control::{
    ControlFailure, ControlFuture, ControlState, EventSubscription, EventsQuery,
};
use hofund_plugin_api::ModelSpec;

use crate::quotas::DaemonQuotas;
use hofund_router::Cooldowns;
use hofund_secrets::{KeyLimit, KeyStore};
use hofund_usage::{Ledger, Window};
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use super::config_ops::{ConfigOps, ReloadOrigin};
use super::control_agents::AgentOps;
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
    shutdown: watch::Sender<bool>,
    events: Arc<EventBus>,
    in_flight: Arc<AtomicU64>,
    quotas: Option<Arc<DaemonQuotas>>,
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
                Arc::new(move || {
                    routes_from
                        .live()
                        .config
                        .routes
                        .iter()
                        .map(|route| route.model.clone())
                        .collect()
                }),
                token_path,
            ),
            usage,
            keys,
            ops,
            router: None,
            shutdown,
            events: Arc::new(EventBus::new()),
            in_flight: Arc::new(AtomicU64::new(0)),
            quotas: None,
        }
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
            .with_session(session)
            .with_token_generation(generation);
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
        match request.action {
            GatewayKeyAction::Add => {
                let added = self
                    .keys
                    .add_with_access(&request.name, request.secret.as_deref(), request.read_only)
                    .map_err(key_failure)?;
                secret = Some(added.secret);
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
                .set_enabled(&request.id, true)
                .map_err(key_failure)?,
            GatewayKeyAction::Off => self
                .keys
                .set_enabled(&request.id, false)
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
        key_response(self.keys.list(), secret, false)
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

    fn route_models(&self) -> ControlFuture<'_, RouteModelListResponse> {
        let mut response = RouteModelListResponse::default();
        response.models = self
            .config()
            .routes
            .iter()
            .map(|route| {
                let mut model = RouteModel::default();
                model.id = route.model.clone();
                model.routing = route.routing.clone();
                model.stays = super::route_stays(&self.config(), route)
                    .as_str()
                    .to_owned();
                if let Some(router) = &self.router {
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
        Box::pin(async move { Ok(response) })
    }

    fn agents(&self) -> ControlFuture<'_, AgentListResponse> {
        Box::pin(async move { self.agents.list().await.map_err(Into::into) })
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

    fn usage(&self, query: UsageQuery) -> ControlFuture<'_, UsageResponse> {
        let ledger = Arc::clone(&self.usage);
        Box::pin(async move {
            let window = window_of(query.period)?;
            let summary = tokio::task::spawn_blocking(move || {
                ledger.summary(window.since_now(), USAGE_ROW_LIMIT)
            })
            .await
            .map_err(|error| internal(format!("usage query task failed: {error}")))?
            .map_err(|error| internal(format!("usage query failed: {error}")))?;
            Ok::<_, ControlFailure>(usage_response(query.period, summary))
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

fn usage_response(
    period: UsagePeriod,
    (rows, totals): (Vec<hofund_usage::Row>, hofund_usage::Totals),
) -> UsageResponse {
    let mut response = UsageResponse::default();
    response.period = period;
    response.rows_truncated = totals.requests > u64::try_from(rows.len()).unwrap_or(u64::MAX);
    response.rows = rows.into_iter().map(usage_row).collect();
    let mut summary = UsageTotals::default();
    summary.requests = totals.requests;
    summary.input_tokens = totals.input_tokens;
    summary.output_tokens = totals.output_tokens;
    response.totals = summary;
    response
}

fn usage_row(row: hofund_usage::Row) -> UsageRow {
    let mut out = UsageRow::default();
    out.created_at_ms = u64::try_from(row.created_unix)
        .unwrap_or(0)
        .saturating_mul(1000);
    out.model = row.model;
    out.provider = row.upstream;
    out.input_tokens = row
        .input_tokens
        .map(|tokens| u64::try_from(tokens).unwrap_or(0));
    out.output_tokens = row
        .output_tokens
        .map(|tokens| u64::try_from(tokens).unwrap_or(0));
    out.duration_ms = u64::try_from(row.latency_ms).unwrap_or(0);
    out.status = row.status;
    out.caller_key_id = row.caller_key_id;
    out.caller_key_name = row.caller_key_name;
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
