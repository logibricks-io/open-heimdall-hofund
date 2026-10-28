use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use hofund_config::Config;
use hofund_control_proto::{
    AgentChangeResponse, AgentConnectRequest, AgentDisconnectRequest, AgentListResponse,
    AgentPlanRequest, AgentPlanResponse, AgentStatusResponse, ControlError, ControlErrorKind,
    HealthResponse, ListenSource, ModelEntry, ModelListResponse, PicksResponse, PicksSetRequest,
    ProviderListResponse, ProviderSummary, ProviderTestRequest, ProviderTestResponse, RouteModel,
    RouteModelListResponse, ShutdownRequest, ShutdownResponse, ShutdownState, StatusResponse,
    TokenRotateRequest, TokenRotateResponse, UsageQuery, UsageResponse,
};
use hofund_gateway::control::{ControlFuture, ControlState};
use hofund_router::Cooldowns;
use hofund_usage::Ledger;
use sha2::{Digest, Sha256};

use super::{DaemonRegistry, ModelsHost, membership, plugin_for, protocol_of};

pub(crate) struct DaemonControl {
    config_path: PathBuf,
    configured: SocketAddr,
    running: SocketAddr,
    started_at_ms: u64,
    runtime_revision: String,
    models: Arc<ModelsHost>,
    config: Arc<Config>,
    registry: Arc<dyn DaemonRegistry>,
    cooldowns: Arc<Cooldowns>,
    #[expect(dead_code, reason = "the usage route is wired in L5d")]
    usage: Arc<Ledger>,
    #[expect(dead_code, reason = "token rotation is wired in L5c")]
    token_path: PathBuf,
}

impl DaemonControl {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        config_path: &Path,
        configured: SocketAddr,
        running: SocketAddr,
        started_at_ms: u64,
        runtime_revision: String,
        models: Arc<ModelsHost>,
        config: Arc<Config>,
        registry: Arc<dyn DaemonRegistry>,
        cooldowns: Arc<Cooldowns>,
        usage: Arc<Ledger>,
        token_path: PathBuf,
    ) -> Self {
        Self {
            config_path: config_path.to_path_buf(),
            configured,
            running,
            started_at_ms,
            runtime_revision,
            models,
            config,
            registry,
            cooldowns,
            usage,
            token_path,
        }
    }

    fn config(&self) -> &Config {
        &self.config
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
        status.generation = 1;
        status.disk_revision = std::fs::read(&self.config_path)
            .map(|bytes| revision(&bytes))
            .unwrap_or_default();
        status.runtime_revision = self.runtime_revision.clone();
        status.in_flight = 0;
        status.shutdown = ShutdownState::Running;
        status.model_listener.configured = self.configured.to_string();
        status.model_listener.effective = self.configured.to_string();
        status.model_listener.running = Some(self.running.to_string());
        status.model_listener.effective_source = ListenSource::Config;
        status
    }

    fn draining(&self) -> bool {
        false
    }

    fn providers(&self) -> ControlFuture<'_, ProviderListResponse> {
        let mut response = ProviderListResponse::default();
        response.providers = self
            .config()
            .providers
            .iter()
            .map(|provider| {
                let cached = self.models.cached(&provider.id);
                let planned = self.models.planned(provider);
                let resting = self
                    .cooldowns
                    .cooling(&membership::member_id(&provider.id))
                    .is_some();
                let mut summary = ProviderSummary::default();
                summary.id = provider.id.clone();
                summary.ready = planned.is_ok() && !resting;
                summary.model_count = u64::try_from(cached.models.len()).unwrap_or(u64::MAX);
                if let Ok(planned) = planned {
                    summary.kind = plugin_for(provider, planned.wire, &self.registry);
                    summary.protocol = protocol_of(planned.wire).to_owned();
                }
                summary
            })
            .collect();
        Box::pin(async move { Ok(response) })
    }

    fn provider_models(&self, id: String) -> ControlFuture<'_, ModelListResponse> {
        let result = self.provider_exists(&id).map(|()| {
            let cached = self.models.cached(&id);
            let mut response = ModelListResponse::default();
            response.provider = id;
            response.fetched_at_ms = cached.fetched_at;
            response.delisted = cached.delisted;
            response.models = cached
                .models
                .iter()
                .map(|model| {
                    let mut entry = ModelEntry::default();
                    entry.id = model.id().to_owned();
                    entry.display_name = model.display_name().map(str::to_owned);
                    entry.context = (model.context() != 0).then(|| model.context());
                    entry.max_output = (model.max_output() != 0).then(|| model.max_output());
                    entry
                })
                .collect();
            response
        });
        Box::pin(async move { result.map_err(Into::into) })
    }

    fn refresh_models(&self, _id: String) -> ControlFuture<'_, ModelListResponse> {
        unavailable()
    }

    fn test_provider(
        &self,
        _id: String,
        _request: ProviderTestRequest,
    ) -> ControlFuture<'_, ProviderTestResponse> {
        unavailable()
    }

    fn picks(&self, _id: String) -> ControlFuture<'_, PicksResponse> {
        unavailable()
    }

    fn set_picks(
        &self,
        _id: String,
        _request: PicksSetRequest,
    ) -> ControlFuture<'_, PicksResponse> {
        unavailable()
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
                model
            })
            .collect();
        Box::pin(async move { Ok(response) })
    }

    fn agents(&self) -> ControlFuture<'_, AgentListResponse> {
        unavailable()
    }

    fn agent_status(&self) -> ControlFuture<'_, AgentStatusResponse> {
        unavailable()
    }

    fn plan_agent(
        &self,
        _id: String,
        _request: AgentPlanRequest,
    ) -> ControlFuture<'_, AgentPlanResponse> {
        unavailable()
    }

    fn connect_agent(
        &self,
        _id: String,
        _request: AgentConnectRequest,
    ) -> ControlFuture<'_, AgentChangeResponse> {
        unavailable()
    }

    fn disconnect_agent(
        &self,
        _id: String,
        _request: AgentDisconnectRequest,
    ) -> ControlFuture<'_, AgentChangeResponse> {
        unavailable()
    }

    fn usage(&self, _query: UsageQuery) -> ControlFuture<'_, UsageResponse> {
        unavailable()
    }

    fn rotate_token(&self, _request: TokenRotateRequest) -> ControlFuture<'_, TokenRotateResponse> {
        unavailable()
    }

    fn shutdown(&self, _request: ShutdownRequest) -> ControlFuture<'_, ShutdownResponse> {
        unavailable()
    }
}

fn unavailable<T>() -> ControlFuture<'static, T> {
    Box::pin(async {
        Err(ControlError::new(
            ControlErrorKind::Unavailable,
            "not wired in this build",
            "control",
        )
        .into())
    })
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
