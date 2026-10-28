use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use hofund_agent::Session;
use hofund_config::Config;
use hofund_control_proto::{
    AgentChangeResponse, AgentConnectRequest, AgentDisconnectRequest, AgentListResponse,
    AgentPlanRequest, AgentPlanResponse, AgentStatusResponse, ControlError, ControlErrorKind,
    FieldIssue, HealthResponse, ListenSource, ModelEntry, ModelListResponse, PicksResponse,
    PicksSetRequest, ProviderListResponse, ProviderSummary, ProviderTestRequest,
    ProviderTestResponse, ProviderTestResult, RouteModel, RouteModelListResponse, ShutdownRequest,
    ShutdownResponse, ShutdownState, StatusResponse, TokenRotateRequest, TokenRotateResponse,
    UsagePeriod, UsageQuery, UsageResponse, UsageRow, UsageTotals,
};
use hofund_gateway::control::{ControlFailure, ControlFuture, ControlState};
use hofund_plugin_api::ModelSpec;
use hofund_router::Cooldowns;
use hofund_usage::{Ledger, Window};
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use super::control_agents::AgentOps;
use super::models::{PicksError, Timed, test_timeout};
use super::{
    DaemonError, DaemonRegistry, ErrorKind, ModelsHost, Picks, control_map, membership, plugin_for,
    protocol_of,
};

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
    usage: Arc<Ledger>,
    agents: AgentOps,
    shutdown: watch::Sender<bool>,
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
        shutdown: watch::Sender<bool>,
    ) -> Self {
        let registry_agents = registry.agents();
        Self {
            config_path: config_path.to_path_buf(),
            configured,
            running,
            started_at_ms,
            runtime_revision,
            models,
            registry,
            cooldowns,
            agents: AgentOps::new(
                registry_agents,
                config_path,
                format!("http://{}", config.listen),
                config
                    .routes
                    .iter()
                    .map(|route| route.model.clone())
                    .collect(),
                token_path,
            ),
            usage,
            config,
            shutdown,
        }
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
        status.shutdown = if self.draining() {
            ShutdownState::Draining
        } else {
            ShutdownState::Running
        };
        status.model_listener.configured = self.configured.to_string();
        status.model_listener.effective = self.configured.to_string();
        status.model_listener.running = Some(self.running.to_string());
        status.model_listener.effective_source = ListenSource::Config;
        status
    }

    fn draining(&self) -> bool {
        *self.shutdown.borrow()
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
            response.models = model_entries(&cached.models);
            response
        });
        Box::pin(async move { result.map_err(Into::into) })
    }

    fn refresh_models(&self, id: String) -> ControlFuture<'_, ModelListResponse> {
        Box::pin(async move {
            self.provider_exists(&id)?;
            let refreshed = self
                .models
                .refresh(self.config(), &self.config_path, &id)
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
            let probes = tokio::time::timeout(
                limit,
                self.models.probe_timed(
                    self.config(),
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
    out
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
