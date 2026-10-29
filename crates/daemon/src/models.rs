//! Per-provider model aggregation and connectivity probes.
//!
//! The optional models.dev limits lookup is deliberately small. The kernel
//! cache that implements it arrives with feature-parity row 2.8.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use futures_util::StreamExt;
use hofund_config::Config;
use hofund_control_proto::{EventPayload, FieldIssue, ModelsRefreshedEvent, PickEdit, PicksForm};
use hofund_plugin_api::{
    AuthChoice, CredentialStyle, DynUpstreamFactory, HostContext, ListKind, ListQuery, ModelSpec,
    PluginError, ProbeQuery, ProbeReport, ProtocolId, RawTable, RawValue, SecretString,
    UpstreamProvider, redact_token, safe_url,
};
use hofund_plugin_host::HttpLimits;
use hofund_plugin_sdk::{Call, PageError, PageErrorKind, after_id};
use hofund_provider::{Credential, Endpoint, ModelPlan, Vertex, Wire};
use tracing::Instrument;

use super::events::EventBus;
use super::{
    DaemonError, DaemonRegistry, ErrorKind, Planned, RegistryAdapter, beside, build_raw_plugin,
    credential_name, endpoint_for, implementation_id, insert_options, option_present, plan_using,
    plugin_for, protocol_of, resolve, show,
};

const NO_MODEL: &str = "no model to try: expose one, or refresh the model list";
const ALL_FAILED: &str = "type its model ids in by hand, or give the URL its list is at";
const PUBLIC_DRAWERS: &str = "https://aihubmix.com/api/v1/models";
const START_DELAY: std::time::Duration = std::time::Duration::from_secs(30);
const TICK: std::time::Duration = std::time::Duration::from_secs(60);
const FETCH_CAP: std::time::Duration = std::time::Duration::from_secs(60);
const RETRY_CAP: std::time::Duration = std::time::Duration::from_secs(15 * 60);
const MIN_RETRY: std::time::Duration = std::time::Duration::from_secs(10 * 60);
/// Limits an upstream-directed retry delay to one day, including after restart.
const MAX_RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
const IMAGE_APIS: &[&str] = &[
    "gpt-image",
    "chatgpt-image",
    "dall-e",
    "imagen",
    "imagine",
    "qwen-image",
    "wanx",
    "wan2",
    "seedream",
    "cogview",
    "flux",
    "stable-diffusion",
    "sdxl",
    "kolors",
    "hidream",
];

/// Optional context and output limits from a kernel model catalog.
pub trait ModelLimits: Send + Sync {
    /// Returns `(context, max_output)`, with zero for an unknown value.
    fn limits(&self, model: &str) -> (u64, u64);
}

/// One configured provider's aggregated list.
///
/// ```compile_fail
/// use hofund_daemon::ProviderModels;
/// use hofund_plugin_api::ListKind;
///
/// let _ = ProviderModels {
///     models: Vec::new(),
///     kind: ListKind::Live,
///     corrected_base: None,
///     base_model_fallback: false,
///     side_errors: Vec::new(),
/// };
/// ```
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ProviderModels {
    /// Merged and planned models.
    pub models: Vec<ModelSpec>,
    /// Resolved list capability after applying the factory bound.
    pub kind: ListKind,
    /// A `/v1` base learned from a successful fallback, when one was found.
    pub corrected_base: Option<String>,
    /// Whether Azure returned its explicitly labelled base-model fallback.
    pub base_model_fallback: bool,
    /// Side failures hidden by a successful response from another side.
    pub side_errors: Vec<String>,
}

/// Refresh-loop timings, with a test constructor for short local runs.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct RefreshTiming {
    start_delay: std::time::Duration,
    tick: std::time::Duration,
    fetch_cap: std::time::Duration,
}

impl RefreshTiming {
    /// Constructs timing for a local lifecycle test.
    #[doc(hidden)]
    #[must_use]
    pub const fn new(
        start_delay: std::time::Duration,
        tick: std::time::Duration,
        fetch_cap: std::time::Duration,
    ) -> Self {
        Self {
            start_delay,
            tick,
            fetch_cap,
        }
    }
}

impl Default for RefreshTiming {
    fn default() -> Self {
        Self {
            start_delay: START_DELAY,
            tick: TICK,
            fetch_cap: FETCH_CAP,
        }
    }
}

/// The host-owned model picks for one configured provider.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Picks {
    picks: Vec<String>,
    picked_from: Vec<String>,
    delisted: Vec<String>,
}

impl Picks {
    /// Model ids picked for exposure, in their saved order.
    #[must_use]
    pub fn picks(&self) -> &[String] {
        &self.picks
    }

    /// Ids in the saved list when the picks were last written.
    #[must_use]
    pub fn picked_from(&self) -> &[String] {
        &self.picked_from
    }

    /// Former picks absent from the latest non-empty list.
    #[must_use]
    pub fn delisted(&self) -> &[String] {
        &self.delisted
    }
}

/// One selected model and its probe result.
#[derive(Clone, Debug)]
pub struct ProbeOutcome {
    /// Aggregated or exposed model id selected by the host.
    pub model: String,
    /// Result returned by the configured plugin instance.
    pub report: ProbeReport,
}

struct RegisteredInstance {
    upstream: Arc<dyn UpstreamProvider>,
    built_base: String,
}

struct Fetched {
    models: ProviderModels,
    sides: std::collections::BTreeMap<String, Vec<ModelSpec>>,
}

struct Applied {
    models: ProviderModels,
    dropped: Vec<String>,
    taken: Vec<String>,
    saved: Result<(), super::models_state::StateError>,
}

/// What a manual refresh fetched and changed.
pub(crate) struct Refreshed {
    pub(crate) models: Vec<ModelSpec>,
    pub(crate) added: Vec<String>,
    pub(crate) removed: Vec<String>,
    pub(crate) delisted: Vec<String>,
    pub(crate) fetched_at: Option<u64>,
}

/// One probe's result and how long it took.
pub(crate) struct Timed {
    pub(crate) model: String,
    pub(crate) latency: std::time::Duration,
    pub(crate) result: Result<ProbeOutcome, DaemonError>,
}

/// Why a picks edit was refused or not saved.
#[derive(Debug)]
pub(crate) enum PicksError {
    /// The expected revision no longer matches the saved state.
    Stale,
    /// The edit is not valid against the current picks.
    Invalid(FieldIssue),
    /// The state file could not be written.
    Save(super::models_state::StateError),
}

/// How long a manual refresh may take.
pub(crate) const REFRESH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// How long a provider test may take: host-side selection gets 30 s and
/// named models 90 s, as the reference CLI allows.
pub(crate) const fn test_timeout(named: bool) -> std::time::Duration {
    if named {
        std::time::Duration::from_secs(90)
    } else {
        std::time::Duration::from_secs(30)
    }
}

fn picks_of(state: &super::models_state::ProviderState) -> Picks {
    Picks {
        picks: state.picks.clone(),
        picked_from: state.picked_from.clone(),
        delisted: state.delisted.clone(),
    }
}

/// Listed ids served when a provider has no picks: every model that is not
/// an image, video, or router model.
fn default_exposed(models: &[ModelSpec]) -> Vec<String> {
    models
        .iter()
        .filter(|model| !model.draws() && !model.films() && !model.decides())
        .map(|model| model.id().to_owned())
        .collect()
}

/// The picks a form leaves, following the reference `editPicks`.
fn next_picks(
    provider_id: &str,
    current: &[String],
    listed: &[ModelSpec],
    form: PicksForm,
) -> Result<Vec<String>, PicksError> {
    match form {
        PicksForm::Replace(ids) => {
            let mut picks = Vec::with_capacity(ids.len());
            for id in ids {
                if !picks.contains(&id) {
                    picks.push(id);
                }
            }
            Ok(picks)
        }
        PicksForm::Edit(edits) => {
            let mut picks = if current.is_empty() {
                default_exposed(listed)
            } else {
                current.to_vec()
            };
            let prefix = format!("{provider_id}/");
            for edit in edits {
                match edit {
                    PickEdit::Add(id) => {
                        let id = id.strip_prefix(&prefix).unwrap_or(&id).to_owned();
                        if !picks.contains(&id) {
                            picks.push(id);
                        }
                    }
                    PickEdit::Remove(id) => {
                        let id = id.strip_prefix(&prefix).unwrap_or(&id);
                        let Some(index) = picks.iter().position(|pick| pick == id) else {
                            return Err(PicksError::Invalid(FieldIssue::new(
                                "edit",
                                "not_exposed",
                                format!(
                                    "{provider_id}/{id} isn't exposed, so there is nothing to take out"
                                ),
                            )));
                        };
                        picks.remove(index);
                    }
                    _ => {
                        return Err(PicksError::Invalid(FieldIssue::new(
                            "edit",
                            "unsupported",
                            "this edit is not supported",
                        )));
                    }
                }
            }
            if picks.is_empty() {
                return Err(PicksError::Invalid(FieldIssue::new(
                    "edit",
                    "empty_result",
                    format!("that leaves no model of {provider_id} exposed"),
                )));
            }
            Ok(picks)
        }
        PicksForm::Reset => Ok(Vec::new()),
        _ => Err(PicksError::Invalid(FieldIssue::new(
            "ids",
            "unsupported",
            "this picks form is not supported",
        ))),
    }
}

/// A provider's cached model state, read without contacting an upstream.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Cached {
    /// Cached merged models.
    pub models: Vec<ModelSpec>,
    /// Unix milliseconds when the list was fetched, when known.
    pub fetched_at: Option<u64>,
    /// Former picks absent from the latest nonempty list.
    pub delisted: Vec<String>,
}

/// Host-side model list and probe composition, with optional persistent state.
pub struct ModelsHost {
    limits: HttpLimits,
    catalog_limits: Option<Arc<dyn ModelLimits>>,
    public_drawers_url: String,
    public_drawers_override: bool,
    plan_overrides: HashMap<String, ModelPlan>,
    host_overrides: HashMap<String, String>,
    instances: RwLock<HashMap<String, RegisteredInstance>>,
    registry: Arc<dyn DaemonRegistry>,
    state: Mutex<super::models_state::StateStore>,
    events: OnceLock<Arc<EventBus>>,
}

fn list_kind_label(kind: ListKind) -> &'static str {
    match kind {
        ListKind::Live => "live",
        ListKind::Catalog => "catalog",
        ListKind::Deployments => "deployments",
        ListKind::Unsupported => "unsupported",
        _ => "unknown",
    }
}

impl ModelsHost {
    /// Creates a host with no models.dev limits lookup and in-memory learned state.
    #[must_use]
    pub fn new(connect_timeout: std::time::Duration, read_timeout: std::time::Duration) -> Self {
        Self {
            limits: HttpLimits::new(connect_timeout, read_timeout),
            catalog_limits: None,
            public_drawers_url: PUBLIC_DRAWERS.to_owned(),
            public_drawers_override: false,
            plan_overrides: HashMap::new(),
            host_overrides: HashMap::new(),
            instances: RwLock::new(HashMap::new()),
            registry: Arc::new(RegistryAdapter),
            state: Mutex::new(super::models_state::StateStore::new(None)),
            events: OnceLock::new(),
        }
    }

    /// Emits `models_refreshed` on the shared control bus when set.
    pub(crate) fn attach_events(&self, bus: Arc<EventBus>) {
        let _ = self.events.set(bus);
    }

    fn emit_refreshed(
        &self,
        provider: &str,
        list_kind: ListKind,
        model_count: u64,
        ok: bool,
        error: Option<String>,
    ) {
        let Some(bus) = self.events.get() else {
            return;
        };
        let mut event = ModelsRefreshedEvent::default();
        event.provider = provider.to_owned();
        event.list_kind = list_kind_label(list_kind).to_owned();
        event.model_count = model_count;
        event.ok = ok;
        event.error = error;
        bus.emit(EventPayload::ModelsRefreshed(event));
    }

    pub(super) fn with_registry(mut self, registry: Arc<dyn DaemonRegistry>) -> Self {
        self.registry = registry;
        self
    }

    /// Stores learned state under `root/host/models` and reloads it lazily by provider id.
    ///
    /// Without this builder, state lasts only for this host instance. The daemon
    /// passes `.hofund` beside the active configuration file.
    #[must_use]
    pub fn with_state_root(mut self, root: impl Into<std::path::PathBuf>) -> Self {
        self.state = Mutex::new(super::models_state::StateStore::new(Some(root.into())));
        self
    }

    /// Reuses the configured daemon instance when its exact built base still matches.
    ///
    /// A missing entry or a different current base falls back to a fresh list/probe build.
    pub fn register_instance(
        &mut self,
        id: impl Into<String>,
        upstream: Arc<dyn UpstreamProvider>,
        built_base: impl Into<String>,
    ) {
        self.instances
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .insert(
                id.into(),
                RegisteredInstance {
                    upstream,
                    built_base: built_base.into(),
                },
            );
    }

    /// Replaces every registered instance with the ones a reload published.
    ///
    /// Each entry is `(provider id, upstream, built base)`. List and probe
    /// calls that start afterwards use these instances; calls already running
    /// keep the instance they cloned.
    pub(crate) fn replace_instances(
        &self,
        instances: impl IntoIterator<Item = (String, Arc<dyn UpstreamProvider>, String)>,
    ) {
        let next = instances
            .into_iter()
            .map(|(id, upstream, built_base)| {
                (
                    id,
                    RegisteredInstance {
                        upstream,
                        built_base,
                    },
                )
            })
            .collect();
        *self
            .instances
            .write()
            .unwrap_or_else(|error| error.into_inner()) = next;
    }

    #[cfg(test)]
    pub(crate) fn instance(&self, id: &str) -> Option<Arc<dyn UpstreamProvider>> {
        self.instances
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .get(id)
            .map(|instance| Arc::clone(&instance.upstream))
    }

    /// Reads a provider's picks, source-list ids, and delisted ids.
    #[must_use]
    pub fn picks(&self, provider_id: &str) -> Picks {
        self.picks_with_revision(provider_id).0
    }

    /// Reads a provider's picks and the revision of its saved state under
    /// one lock.
    pub(crate) fn picks_with_revision(&self, provider_id: &str) -> (Picks, String) {
        let mut store = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let state = store.get(provider_id);
        (picks_of(state), super::models_state::revision(state))
    }

    /// Changes a provider's picks and saves them.
    ///
    /// The lock is held from reading the revision through the save, so a
    /// concurrent edit cannot slip between the check and the write. A failed
    /// save leaves memory as it was.
    pub(crate) fn edit_picks(
        &self,
        provider_id: &str,
        expected: Option<&str>,
        form: PicksForm,
    ) -> Result<(Picks, String), PicksError> {
        let mut store = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let state = store.get(provider_id);
        if let Some(expected) = expected
            && super::models_state::revision(state) != expected
        {
            return Err(PicksError::Stale);
        }
        let listed = super::models_state::models(&state.list);
        let next = next_picks(provider_id, &state.picks, &listed, form)?;
        let picked_from = listed
            .iter()
            .map(|model| model.id().to_owned())
            .collect::<Vec<_>>();
        store
            .try_update(provider_id, |state| {
                state.delisted.retain(|id| !next.contains(id));
                state.picks = next;
                state.picked_from = picked_from;
            })
            .map_err(PicksError::Save)?;
        let state = store.get(provider_id);
        Ok((picks_of(state), super::models_state::revision(state)))
    }

    /// Reads cached models without building a provider or contacting an upstream.
    #[must_use]
    pub fn cached(&self, provider_id: &str) -> Cached {
        let mut store = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let state = store.get(provider_id);
        Cached {
            models: super::models_state::models(&state.list),
            fetched_at: state.fetched_at,
            delisted: state.delisted.clone(),
        }
    }

    /// Saves picked ids in the supplied order and restores them from delisted.
    ///
    /// Duplicates keep their first place. An empty slice restores the
    /// default list. A failed save is logged and memory is left unchanged.
    pub fn set_picks(&self, provider_id: &str, ids: &[String]) {
        let form = if ids.is_empty() {
            PicksForm::Reset
        } else {
            PicksForm::Replace(ids.to_vec())
        };
        if let Err(error) = self.edit_picks(provider_id, None, form) {
            tracing::warn!(
                provider.id = provider_id,
                ?error,
                "could not set model picks"
            );
        }
    }

    /// Installs the limits seam used only by the static-fallback planning path.
    #[must_use]
    pub fn with_catalog_limits(mut self, limits: Arc<dyn ModelLimits>) -> Self {
        self.catalog_limits = Some(limits);
        self
    }

    /// Replaces the public drawer feed for a local mock.
    ///
    /// An explicit override also enables the drawer rule for a mock base host.
    #[doc(hidden)]
    #[must_use]
    pub fn with_public_drawers_url(mut self, url: impl Into<String>) -> Self {
        self.public_drawers_url = url.into();
        self.public_drawers_override = true;
        self
    }

    /// Overrides one configured provider's region-resolved plan for local tests.
    #[doc(hidden)]
    #[must_use]
    pub fn with_model_plan(mut self, provider: impl Into<String>, plan: ModelPlan) -> Self {
        self.plan_overrides.insert(provider.into(), plan);
        self
    }

    /// Overrides the hostname used by host-side policy for one exact base URL.
    #[doc(hidden)]
    #[must_use]
    pub fn with_base_host(mut self, base: impl Into<String>, host: impl Into<String>) -> Self {
        self.host_overrides.insert(base.into(), host.into());
        self
    }

    /// Fetches each due provider in configuration order and returns the ids asked.
    #[doc(hidden)]
    pub async fn refresh_due(
        &self,
        config: &Config,
        config_path: &Path,
        now: std::time::SystemTime,
    ) -> Vec<String> {
        self.refresh_due_with_timing(config, config_path, now, RefreshTiming::default())
            .await
    }

    /// Refreshes due model lists on every tick, reading the published
    /// configuration from `config` each time so a reload takes effect on the
    /// next tick.
    pub(super) fn spawn_refresh(
        self: &Arc<Self>,
        config: tokio::sync::watch::Receiver<Arc<Config>>,
        config_path: std::path::PathBuf,
        timing: RefreshTiming,
    ) -> tokio::task::JoinHandle<()> {
        let host = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(timing.start_delay).await;
            loop {
                let now = std::time::SystemTime::now();
                let current = Arc::clone(&config.borrow());
                host.refresh_due_with_timing(&current, &config_path, now, timing)
                    .await;
                tokio::time::sleep(timing.tick).await;
            }
        })
    }

    async fn refresh_due_with_timing(
        &self,
        config: &Config,
        config_path: &Path,
        now: std::time::SystemTime,
        timing: RefreshTiming,
    ) -> Vec<String> {
        let now = system_ms(now);
        let mut asked = Vec::new();
        for provider in &config.providers {
            let Some(interval) = provider.model_list.refresh_interval() else {
                continue;
            };
            let refreshable = (|| {
                let planned = self.planned(provider)?;
                let model_plan = self.model_plan(provider)?;
                let configured_plugin = plugin_for(provider, planned.wire, &self.registry);
                self.kind(provider, &planned, model_plan.as_ref(), &configured_plugin)
            })();
            let Ok(kind) = refreshable else {
                tracing::warn!(
                    provider.id = provider.id,
                    "scheduled model list fetch could not resolve the provider"
                );
                continue;
            };
            // Disabled and decide-only providers do not exist in hofund yet.
            if matches!(kind, ListKind::Catalog | ListKind::Unsupported) {
                continue;
            }
            let (fetched_at, retry_at) = {
                let mut store = self.state.lock().unwrap_or_else(|error| error.into_inner());
                let state = store.get(&provider.id);
                (state.fetched_at, state.retry_at)
            };
            if retry_at.is_some_and(|retry| retry > now)
                || fetched_at.is_some_and(|fetched| {
                    u128::from(now.saturating_sub(fetched)) < interval.as_millis()
                })
            {
                continue;
            }
            let retry_at = now.saturating_add(duration_ms(Self::retry_wait(interval, None)));
            self.state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .update(&provider.id, |state| {
                    state.tried_at = Some(now);
                    state.retry_at = Some(retry_at);
                });
            asked.push(provider.id.clone());
            let fetched = tokio::time::timeout(
                timing.fetch_cap,
                self.fetch_applied(
                    config_path,
                    provider,
                    now,
                    Some(&provider.model_list.auto_include),
                ),
            )
            .await;
            match fetched {
                Ok(Ok(applied)) => {
                    tracing::info!(
                        kind = "model_refresh",
                        provider.id = provider.id,
                        model_count = applied.models.models.len(),
                        taken = ?applied.taken,
                        dropped = ?applied.dropped,
                        outcome = "success"
                    );
                    self.emit_refreshed(
                        &provider.id,
                        applied.models.kind,
                        applied.models.models.len() as u64,
                        true,
                        None,
                    );
                }
                Ok(Err(error)) => {
                    let message = error.to_string();
                    self.record_refresh_retry(&provider.id, now, interval, error.retry_after());
                    self.emit_refreshed(&provider.id, kind, 0, false, Some(message));
                }
                Err(_) => {
                    self.record_refresh_retry(&provider.id, now, interval, None);
                    self.emit_refreshed(
                        &provider.id,
                        kind,
                        0,
                        false,
                        Some("model refresh timed out".to_owned()),
                    );
                }
            }
        }
        asked
    }

    fn retry_wait(
        interval: std::time::Duration,
        retry_after: Option<std::time::Duration>,
    ) -> std::time::Duration {
        interval
            .min(RETRY_CAP)
            .max(retry_after.unwrap_or_default().min(MAX_RETRY_AFTER))
            .max(MIN_RETRY)
    }

    fn record_refresh_retry(
        &self,
        provider: &str,
        now: u64,
        interval: std::time::Duration,
        retry_after: Option<std::time::Duration>,
    ) {
        let wait = Self::retry_wait(interval, retry_after);
        let retry_at = now.saturating_add(duration_ms(wait));
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .update(provider, |state| {
                state.retry_at = Some(retry_at);
            });
        let empty: &[String] = &[];
        tracing::info!(
            kind = "model_refresh",
            provider.id = provider,
            model_count = 0,
            taken = ?empty,
            dropped = ?empty,
            retry_in_ms = duration_ms(wait),
            outcome = "error"
        );
    }

    /// Lists one configured provider, never merging it with another provider.
    ///
    /// # Errors
    ///
    /// Returns [`DaemonError`] when configuration, construction, or every
    /// required list source fails. Secret values and URL queries are redacted.
    pub async fn list_models(
        &self,
        config: &Config,
        config_path: &Path,
        provider_id: &str,
    ) -> Result<ProviderModels, DaemonError> {
        let provider = provider(config, provider_id)?;
        let planned = self.planned(provider)?;
        let endpoint = endpoint_for(provider, &planned)?;
        let redactor = Redactor::new(provider, &endpoint, None);
        let span = tracing::info_span!(
            "hofund_model_list",
            provider.id = provider.id,
            wire = protocol_of(planned.wire),
            kind = tracing::field::Empty,
            outcome = tracing::field::Empty
        );
        async {
            let result = self
                .fetch_applied(
                    config_path,
                    provider,
                    unix_ms(),
                    Some(&provider.model_list.auto_include),
                )
                .await
                .map(|applied| applied.models);
            match &result {
                Ok(models) => {
                    tracing::Span::current().record("kind", format_args!("{:?}", models.kind));
                    tracing::Span::current().record("outcome", "success");
                }
                Err(_) => {
                    tracing::Span::current().record("outcome", "error");
                }
            }
            let outcome = match &result {
                Ok(_) => "success",
                Err(_) => "error",
            };
            tracing::info!(
                kind = "model_list",
                provider.id = provider.id,
                wire = protocol_of(planned.wire),
                outcome
            );
            result.map_err(|error| redactor.clean_error(error))
        }
        .instrument(span)
        .await
    }

    /// Fetches a provider's list now, applies it, and reports what changed.
    ///
    /// # Errors
    ///
    /// Returns [`DaemonError`] when the fetch fails or runs past
    /// [`REFRESH_TIMEOUT`], or when the result cannot be saved.
    pub(crate) async fn refresh(
        &self,
        config: &Config,
        config_path: &Path,
        provider_id: &str,
    ) -> Result<Refreshed, DaemonError> {
        let provider = provider(config, provider_id)?;
        let planned = self.planned(provider)?;
        let endpoint = endpoint_for(provider, &planned)?;
        let redactor = Redactor::new(provider, &endpoint, None);
        let applied = tokio::time::timeout(
            REFRESH_TIMEOUT,
            self.fetch_applied(
                config_path,
                provider,
                unix_ms(),
                Some(&provider.model_list.auto_include),
            ),
        )
        .await
        .map_err(|_| {
            DaemonError::new(format!(
                "listing models for provider `{provider_id}` timed out"
            ))
            .with_kind(ErrorKind::Timeout)
        })?
        .map_err(|error| redactor.clean_error(error))?;
        if let Err(error) = applied.saved {
            return Err(DaemonError::new(error.to_string()));
        }
        let mut store = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let state = store.get(provider_id);
        let kind = applied.models.kind;
        let count = applied.models.models.len() as u64;
        let refreshed = Refreshed {
            models: applied.models.models,
            added: applied.taken,
            removed: applied.dropped,
            delisted: state.delisted.clone(),
            fetched_at: state.fetched_at,
        };
        drop(store);
        self.emit_refreshed(provider_id, kind, count, true, None);
        Ok(refreshed)
    }

    /// Probes models and times each probe, keeping input order.
    ///
    /// # Errors
    ///
    /// Returns [`DaemonError`] for provider construction or selection
    /// failures; no probe is sent then.
    pub(crate) async fn probe_timed(
        &self,
        config: &Config,
        config_path: &Path,
        provider_id: &str,
        named: Vec<&str>,
    ) -> Result<Vec<Timed>, DaemonError> {
        self.probe_inner(config, config_path, provider_id, named)
            .await
    }

    /// Refreshes the list, selects one model, and probes the configured wire.
    ///
    /// `named` may name an exposed route id or a listed upstream id. Absent
    /// selection tries exposed models and then listed models in two passes.
    ///
    /// # Errors
    ///
    /// Returns [`DaemonError`] when no model is eligible, a named model is not
    /// served on this protocol, construction fails, or the upstream rejects the
    /// probe. No request is sent for selection errors.
    pub async fn probe(
        &self,
        config: &Config,
        config_path: &Path,
        provider_id: &str,
        named: Option<&str>,
    ) -> Result<ProbeOutcome, DaemonError> {
        let mut results = self
            .probe_many(
                config,
                config_path,
                provider_id,
                named.into_iter().collect(),
            )
            .await?;
        results.remove(0)
    }

    /// Probes several named models with at most four calls in flight.
    ///
    /// An empty `named` slice performs ordinary two-pass selection. Results
    /// retain input order and each upstream failure is returned in its own slot.
    ///
    /// # Errors
    ///
    /// Returns [`DaemonError`] for provider construction or selection failures.
    pub async fn probe_many(
        &self,
        config: &Config,
        config_path: &Path,
        provider_id: &str,
        named: Vec<&str>,
    ) -> Result<Vec<Result<ProbeOutcome, DaemonError>>, DaemonError> {
        Ok(self
            .probe_inner(config, config_path, provider_id, named)
            .await?
            .into_iter()
            .map(|timed| timed.result)
            .collect())
    }

    async fn probe_inner(
        &self,
        config: &Config,
        config_path: &Path,
        provider_id: &str,
        named: Vec<&str>,
    ) -> Result<Vec<Timed>, DaemonError> {
        let provider = provider(config, provider_id)?;
        let planned = self.planned(provider)?;
        let endpoint = endpoint_for(provider, &planned)?;
        let redactor = Redactor::new(provider, &endpoint, None);
        let listed = self.list_models(config, config_path, provider_id).await;
        let models = listed
            .as_ref()
            .map_or(&[][..], |list| list.models.as_slice());
        let picks = self.picks(provider_id);
        let selected = if named.is_empty() {
            vec![select_model(
                config,
                provider,
                &planned,
                models,
                picks.picks(),
                None,
            )?]
        } else {
            named
                .into_iter()
                .map(|name| {
                    select_model(
                        config,
                        provider,
                        &planned,
                        models,
                        picks.picks(),
                        Some(name),
                    )
                })
                .collect::<Result<Vec<_>, _>>()?
        };
        let upstream = self.configured(provider, &planned, &endpoint, config_path)?;
        let protocol = ProtocolId::try_from(protocol_of(planned.wire)).map_err(show)?;
        let learned = self.learned_auth(provider, planned.wire);
        let extra = self.extra_body(provider)?;
        let results = futures_util::stream::iter(selected.into_iter().map(|selected| {
            let upstream = Arc::clone(&upstream);
            let extra = extra.clone();
            let planned = planned.clone();
            let redactor = redactor.clone();
            async move {
                let model = selected.exposed.clone();
                let started = std::time::Instant::now();
                let result = self
                    .probe_one(
                        provider, &planned, upstream, protocol, selected, learned, extra,
                    )
                    .await
                    .map_err(|error| redactor.clean_error(error));
                Timed {
                    model,
                    latency: started.elapsed(),
                    result,
                }
            }
        }))
        .buffered(4)
        .collect::<Vec<_>>()
        .await;
        Ok(results)
    }

    async fn fetch_applied(
        &self,
        path: &Path,
        provider: &hofund_config::Provider,
        now: u64,
        auto_include: Option<&[String]>,
    ) -> Result<Applied, DaemonError> {
        let planned = self.planned(provider)?;
        let endpoint = endpoint_for(provider, &planned)?;
        let rules = auto_include
            .unwrap_or(&[])
            .iter()
            .map(|rule| hofund_config::compile_auto_include(rule).map_err(show))
            .collect::<Result<Vec<_>, _>>()?;
        let fetched = self.list_inner(path, provider, &planned, &endpoint).await;
        match fetched {
            Ok(fetched) => Ok(self.apply_success(provider, fetched, now, rules)),
            Err(error) => {
                self.state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .update(&provider.id, |state| state.tried_at = Some(now));
                Err(error)
            }
        }
    }

    fn apply_success(
        &self,
        provider: &hofund_config::Provider,
        fetched: Fetched,
        now: u64,
        auto_include: Vec<regex::Regex>,
    ) -> Applied {
        let listed = super::models_state::rows(&fetched.models.models);
        let now_ids = listed
            .iter()
            .map(|row| row.id.clone())
            .collect::<HashSet<_>>();
        let chat_ids = default_exposed(&fetched.models.models);
        let mut dropped = Vec::new();
        let mut taken = Vec::new();
        // Memory keeps the new list even when the save fails, so a list
        // request and the scheduled refresh still serve what was fetched;
        // the refresh route reports the failed save.
        let saved = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .update_saved(&provider.id, |state| {
                state.tried_at = Some(now);
                if listed.is_empty() {
                    return;
                }
                state.retry_at = None;
                let before = state
                    .list
                    .iter()
                    .map(|row| row.id.clone())
                    .collect::<HashSet<_>>();
                let mut kept = Vec::new();
                for id in &state.picks {
                    if before.contains(id) && !now_ids.contains(id) {
                        dropped.push(id.clone());
                    } else {
                        kept.push(id.clone());
                    }
                }
                for id in &dropped {
                    if !state.delisted.contains(id) {
                        state.delisted.push(id.clone());
                    }
                }
                state.picks = kept;
                // Restore after dropping, unlike the reference gateway,
                // internal/provider/models.go:317-335, which can lose a
                // restored pick when one fetch also drops another.
                let restored = state
                    .delisted
                    .iter()
                    .filter(|id| now_ids.contains(*id))
                    .cloned()
                    .collect::<Vec<_>>();
                state.delisted.retain(|id| !now_ids.contains(id));
                for id in restored {
                    if !state.picks.contains(&id) {
                        state.picks.push(id);
                    }
                }
                if !auto_include.is_empty() {
                    let served = if !state.picks.is_empty() {
                        state.picks.clone()
                    } else if state.delisted.is_empty() {
                        chat_ids.clone()
                    } else {
                        Vec::new()
                    };
                    for id in &chat_ids {
                        if before.contains(id)
                            || served.contains(id)
                            || taken.contains(id)
                            || !auto_include.iter().any(|rule| rule.is_match(id))
                        {
                            continue;
                        }
                        taken.push(id.clone());
                    }
                    if !taken.is_empty() {
                        let mut picks = taken.clone();
                        picks.extend(served);
                        state.picks = picks;
                        state.picked_from = chat_ids.clone();
                    }
                }
                state.fetched_at = Some(now);
                state.list = listed;
                state.sides = fetched
                    .sides
                    .iter()
                    .map(|(base, models)| (base.clone(), super::models_state::rows(models)))
                    .collect();
            });
        Applied {
            models: fetched.models,
            dropped,
            taken,
            saved,
        }
    }

    async fn list_inner(
        &self,
        path: &Path,
        provider: &hofund_config::Provider,
        planned: &Planned,
        endpoint: &Endpoint,
    ) -> Result<Fetched, DaemonError> {
        let model_plan = self.model_plan(provider)?;
        let configured_plugin = plugin_for(provider, planned.wire, &self.registry);
        let kind = self.kind(provider, planned, model_plan.as_ref(), &configured_plugin)?;
        tracing::Span::current().record("kind", format_args!("{kind:?}"));
        let configured = self.configured(provider, planned, endpoint, path)?;
        let host = self.runtime_context(provider, endpoint, path, &configured_plugin)?;
        let custom_url = if provider.model_list.url.trim().is_empty() {
            None
        } else {
            Some(hofund_secrets::resolve(provider.model_list.url.trim()).map_err(show)?)
        }
        .filter(|_| kind != ListKind::Catalog);
        let previous = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&provider.id)
            .clone();
        let redactor = Redactor::new(provider, endpoint, custom_url.as_deref());
        let result = if let Some(url) = custom_url.as_deref() {
            let messages_only = planned.wire == Wire::Messages
                && model_plan.as_ref().is_none_or(|plan| {
                    plan.chat.trim().is_empty() && plan.responses.trim().is_empty()
                });
            self.fetch_exact(provider, planned, endpoint, &host, url, messages_only)
                .await
                .map(|list| (list, url.to_owned(), std::collections::BTreeMap::new()))
        } else {
            self.fetch_bases(
                path,
                provider,
                planned,
                endpoint,
                model_plan.as_ref(),
                kind,
                configured,
                &configured_plugin,
                &previous,
            )
            .await
        };
        let (mut merged, winning_base, sides) =
            result.map_err(|error| redactor.clean_error(error))?;
        let models = self.plan_models(
            provider,
            model_plan.as_ref(),
            std::mem::take(&mut merged.models),
        )?;
        merged.models = models;
        self.append_public_drawers(&host, &winning_base, &mut merged.models)
            .await;
        merged.kind = kind;
        Ok(Fetched {
            models: merged,
            sides,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn fetch_bases(
        &self,
        path: &Path,
        provider: &hofund_config::Provider,
        planned: &Planned,
        endpoint: &Endpoint,
        model_plan: Option<&ModelPlan>,
        kind: ListKind,
        configured: Arc<dyn UpstreamProvider>,
        configured_plugin: &str,
        previous: &super::models_state::ProviderState,
    ) -> Result<
        (
            ProviderModels,
            String,
            std::collections::BTreeMap<String, Vec<ModelSpec>>,
        ),
        DaemonError,
    > {
        if kind == ListKind::Catalog {
            let models = if matches!(planned.wire, Wire::Vertex | Wire::Converse) {
                configured
                    .list_models(&ListQuery::new())
                    .await
                    .map_err(show)?
                    .models()
                    .to_vec()
            } else {
                static_models(model_plan.map_or(&[][..], |plan| plan.models.as_slice()))?
            };
            return Ok((
                ProviderModels {
                    models,
                    kind,
                    corrected_base: None,
                    base_model_fallback: false,
                    side_errors: Vec::new(),
                },
                planned.base_url.clone(),
                std::collections::BTreeMap::new(),
            ));
        }

        let mut sides = Vec::new();
        if provider.base_url.trim().is_empty()
            && let Some(model_plan) = model_plan
        {
            for (wire, base) in [
                (Wire::Chat, &model_plan.chat),
                (Wire::Responses, &model_plan.responses),
                (Wire::Messages, &model_plan.anthropic),
                (Wire::Generative, &model_plan.generative),
            ] {
                let base = if wire == planned.wire {
                    &planned.base_url
                } else {
                    base
                };
                if !base.trim().is_empty() {
                    sides.push((wire, base.clone(), wire == planned.wire));
                }
            }
        }
        if sides.is_empty() {
            sides.push((planned.wire, planned.base_url.clone(), true));
        }

        let mut asked = 0;
        let mut dead = Vec::new();
        let mut side_lists = std::collections::BTreeMap::new();
        let has_openai = sides
            .iter()
            .any(|(wire, _, _)| matches!(wire, Wire::Chat | Wire::Responses));
        let mut seen_bases = HashSet::new();
        let mut merged_lists = HashSet::new();
        let mut merged = Merged::default();
        let mut errors = Vec::new();
        let mut retry_after: Option<std::time::Duration> = None;
        let mut failure: Option<ErrorKind> = None;
        let mut winning_base = None;
        let mut corrected_base = None;
        let mut fallback = false;
        let mut succeeded = 0;
        for (wire, base, is_configured) in sides {
            if wire == Wire::Messages
                && has_openai
                && self.host(&base).as_deref() == Some("openrouter.ai")
            {
                continue;
            }
            let auth_key = format!(
                "{}\0{:?}\0{:?}",
                endpoint.api_key,
                self.learned_auth(provider, wire),
                endpoint.headers
            );
            if !seen_bases.insert((base.trim_end_matches('/').to_owned(), auth_key)) {
                continue;
            }
            asked += 1;
            let upstream = if is_configured {
                Arc::clone(&configured)
            } else {
                match self.list_only(
                    path,
                    provider,
                    planned,
                    endpoint,
                    model_plan,
                    configured_plugin,
                    wire,
                    &base,
                ) {
                    Ok(upstream) => upstream,
                    Err(error) => {
                        errors.push(error.to_string());
                        dead.push(base.clone());
                        continue;
                    }
                }
            };
            let query = match self.learned_auth(provider, wire) {
                Some(choice) if wire == Wire::Messages => ListQuery::new().with_auth_choice(choice),
                _ => ListQuery::new(),
            };
            match upstream.list_models(&query).await {
                Ok(list) => {
                    if wire == Wire::Messages
                        && self.host(&base).as_deref() == Some("openrouter.ai")
                        && list
                            .models()
                            .iter()
                            .any(|model| model.id().starts_with("anthropic/"))
                    {
                        errors.push(format!(
                            "{}: with the Anthropic version header this base answers with OpenRouter's own catalog namespaced under `anthropic/`, which is not a list of models to serve; give this provider OpenRouter's Chat base (https://openrouter.ai/api/v1), which lists those models under the ids OpenRouter serves",
                            safe_url(&base)
                        ));
                        dead.push(base.clone());
                        continue;
                    }
                    let auth_key = format!(
                        "{}\0{:?}\0{:?}",
                        endpoint.api_key,
                        list.auth_choice()
                            .or_else(|| self.learned_auth(provider, wire)),
                        endpoint.headers
                    );
                    let answered = list
                        .source_url()
                        .unwrap_or_else(|| base.trim_end_matches('/'))
                        .to_owned();
                    // reference: internal/provider/models.go:687-696 and 777-781
                    if !merged_lists.insert((answered, auth_key)) {
                        continue;
                    }
                    succeeded += 1;
                    if let Some(choice) = list.auth_choice()
                        && wire == Wire::Messages
                    {
                        self.set_auth(&provider.id, choice);
                    }
                    fallback |= list.base_model_fallback();
                    let mut side_base = base.clone();
                    if list.v1_fallback() && matches!(wire, Wire::Chat | Wire::Responses) {
                        let fixed = format!("{}/v1", base.trim_end_matches('/'));
                        self.state
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .update(&provider.id, |state| {
                                state
                                    .bases
                                    .insert(wire_name(wire).to_owned(), fixed.clone());
                            });
                        if wire == planned.wire {
                            corrected_base = Some(fixed.clone());
                            side_base = fixed;
                        }
                    }
                    if winning_base.is_none() {
                        winning_base = Some(side_base.clone());
                    }
                    side_lists.insert(side_base, list.models().to_vec());
                    merged.add(list.models(), list.dropped());
                }
                Err(error) => {
                    // reference: internal/provider/models.go:774-785
                    tracing::warn!(
                        provider.id = provider.id,
                        wire = wire_name(wire),
                        base = %safe_url(&base),
                        error.kind = ?error.kind(),
                        error.message = error.message(),
                        "model list side failed"
                    );
                    retry_after = match (retry_after, error.retry_after()) {
                        (Some(left), Some(right)) => Some(left.max(right)),
                        (None, right) => right,
                        (left, None) => left,
                    };
                    let message = error.to_string();
                    let converted = crate::adapt::provider_error(error);
                    if failure.is_none() || converted.status == Some(429) {
                        failure = Some(ErrorKind::Upstream {
                            kind: converted.kind,
                            status: converted.status,
                        });
                    }
                    if !errors.contains(&message) {
                        errors.push(message);
                    }
                    dead.push(base.clone());
                }
            }
        }
        if succeeded == 0 {
            if errors.is_empty() {
                return Err(DaemonError::new(format!(
                    "provider `{}` has no endpoint to ask",
                    provider.id
                )));
            }
            let message = if kind == ListKind::Deployments {
                errors.join("; ")
            } else {
                format!("{} — {ALL_FAILED}", errors.join("; "))
            };
            return Err(DaemonError::new(message)
                .with_retry_after(retry_after)
                .with_kind(failure.unwrap_or_default()));
        }
        // reference: internal/provider/models.go:809-822 and 927-968
        if !dead.is_empty() {
            let answering = side_lists
                .values()
                .flatten()
                .map(|model| model.id().to_owned())
                .collect::<HashSet<_>>();
            let previous_list = super::models_state::models(&previous.list);
            for base in dead {
                if let Some(saved) = previous.sides.get(&base) {
                    let part = super::models_state::models(saved);
                    side_lists.insert(base, part.clone());
                    merged.add(&part, &[]);
                } else if previous.sides.is_empty() {
                    let part = previous_list
                        .iter()
                        .filter(|model| !answering.contains(model.id()))
                        .cloned()
                        .collect::<Vec<_>>();
                    merged.add(&part, &[]);
                }
            }
        }
        if asked <= 1 {
            side_lists.clear();
        }
        Ok((
            ProviderModels {
                models: merged.finish(),
                kind,
                corrected_base,
                base_model_fallback: fallback,
                side_errors: errors,
            },
            winning_base.unwrap_or_else(|| planned.base_url.clone()),
            side_lists,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn list_only(
        &self,
        path: &Path,
        provider: &hofund_config::Provider,
        planned: &Planned,
        endpoint: &Endpoint,
        model_plan: Option<&ModelPlan>,
        configured_plugin: &str,
        wire: Wire,
        base: &str,
    ) -> Result<Arc<dyn UpstreamProvider>, DaemonError> {
        let credential = self.credential(provider, wire)?;
        let (plugin_id, raw, own) = list_only_raw(
            provider,
            planned,
            endpoint,
            model_plan,
            configured_plugin,
            wire,
            base,
            credential,
        )?;
        build_raw_plugin(
            provider,
            plugin_id,
            raw,
            super::registry::protocol_for(wire),
            &endpoint.api_key,
            path,
            self.limits,
            own,
            false,
            None,
            &self.registry,
        )
    }

    fn configured(
        &self,
        provider: &hofund_config::Provider,
        planned: &Planned,
        endpoint: &Endpoint,
        path: &Path,
    ) -> Result<Arc<dyn UpstreamProvider>, DaemonError> {
        if let Some(upstream) = self
            .instances
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .get(&provider.id)
            .filter(|instance| instance.built_base == planned.base_url)
            .map(|instance| Arc::clone(&instance.upstream))
        {
            return Ok(upstream);
        }
        let plugin_id = plugin_for(provider, planned.wire, &self.registry);
        let mut raw = super::raw_for(provider, planned, endpoint, &self.registry)?;
        raw.insert("base_url", RawValue::String(planned.base_url.clone()));
        build_raw_plugin(
            provider,
            &plugin_id,
            raw,
            planned.protocol,
            &endpoint.api_key,
            path,
            self.limits,
            true,
            false,
            None,
            &self.registry,
        )
    }

    fn runtime_context(
        &self,
        provider: &hofund_config::Provider,
        endpoint: &Endpoint,
        path: &Path,
        plugin_id: &str,
    ) -> Result<HostContext, DaemonError> {
        let plugin = hofund_plugin_api::PluginId::try_from(plugin_id).map_err(show)?;
        hofund_plugin_host::runtime_host(
            plugin,
            endpoint.api_key.clone(),
            beside(path, ".hofund"),
            self.limits,
        )
        .map_err(show)
        .map_err(|error| DaemonError::new(format!("provider `{}`: {error}", provider.id)))
    }

    fn host(&self, base: &str) -> Option<String> {
        self.host_overrides
            .get(base)
            .cloned()
            .or_else(|| url_host(base))
    }

    pub(super) fn planned(
        &self,
        provider: &hofund_config::Provider,
    ) -> Result<Planned, DaemonError> {
        self.planned_with(provider, |id| self.registry.find_preset(id))
    }

    fn planned_with(
        &self,
        provider: &hofund_config::Provider,
        lookup: impl Fn(
            &str,
        ) -> Option<(
            &'static super::registry::CompiledPreset,
            Option<&'static str>,
        )> + Copy,
    ) -> Result<Planned, DaemonError> {
        let mut planned = plan_using(provider, lookup, &self.registry)?;
        if provider.base_url.trim().is_empty()
            && let Some(model_plan) = self.model_plan_with(provider, lookup)?
        {
            let base = match planned.wire {
                Wire::Chat => model_plan.chat,
                Wire::Responses => model_plan.responses,
                Wire::Messages => model_plan.anthropic,
                Wire::Generative | Wire::Vertex | Wire::Converse => String::new(),
            };
            if !base.is_empty() {
                planned.base_url = base;
            }
        }
        let wire = wire_name(planned.wire);
        if let Some(base) = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&provider.id)
            .bases
            .get(wire)
            .cloned()
            .filter(|base| *base == format!("{}/v1", planned.base_url.trim_end_matches('/')))
        {
            planned.base_url = base;
        }
        Ok(planned)
    }

    fn model_plan(
        &self,
        provider: &hofund_config::Provider,
    ) -> Result<Option<ModelPlan>, DaemonError> {
        self.model_plan_with(provider, |id| self.registry.find_preset(id))
    }

    fn model_plan_with(
        &self,
        provider: &hofund_config::Provider,
        lookup: impl Fn(
            &str,
        ) -> Option<(
            &'static super::registry::CompiledPreset,
            Option<&'static str>,
        )>,
    ) -> Result<Option<ModelPlan>, DaemonError> {
        if let Some(plan) = self.plan_overrides.get(&provider.id) {
            return Ok(Some(plan.clone()));
        }
        let Some(preset) = provider.preset.as_deref() else {
            return Ok(None);
        };
        if let Some((compiled, alias_region)) = lookup(preset) {
            let region = super::registry::resolve_region(
                compiled,
                option_present(provider, "region")?.or(alias_region),
            )
            .map_err(show)?;
            return Ok(Some(super::registry::model_plan_of(compiled, region)));
        }
        hofund_provider::model_plan(preset, option_present(provider, "region")?).map_err(show)
    }

    fn kind(
        &self,
        provider: &hofund_config::Provider,
        planned: &Planned,
        model_plan: Option<&ModelPlan>,
        plugin_id: &str,
    ) -> Result<ListKind, DaemonError> {
        let azure = matches!(planned.wire, Wire::Chat | Wire::Responses)
            && (provider.preset.as_deref() == Some("azure")
                || hofund_provider::azure_host(&self.host(&planned.base_url).unwrap_or_default()));
        let catalog = model_plan.is_some_and(|plan| {
            plan.no_list && !plan.lists && provider.model_list.url.trim().is_empty()
        });
        let wanted = if azure {
            ListKind::Deployments
        } else if catalog {
            ListKind::Catalog
        } else {
            ListKind::Live
        };
        let factory = self
            .registry
            .factory(plugin_id)
            .ok_or_else(|| DaemonError::new(format!("no factory for plugin `{plugin_id}`")))?;
        Ok(bounded_kind(
            wanted,
            DynUpstreamFactory::spec(factory.as_ref()).list_kind(),
        ))
    }

    fn credential(
        &self,
        provider: &hofund_config::Provider,
        wire: Wire,
    ) -> Result<Credential, DaemonError> {
        self.credential_with(provider, wire, |id| self.registry.find_preset(id))
    }

    fn credential_with(
        &self,
        provider: &hofund_config::Provider,
        wire: Wire,
        lookup: impl Fn(
            &str,
        ) -> Option<(
            &'static super::registry::CompiledPreset,
            Option<&'static str>,
        )>,
    ) -> Result<Credential, DaemonError> {
        let Some(preset) = provider.preset.as_deref() else {
            return Ok(Credential::Default);
        };
        if let Some((compiled, alias_region)) = lookup(preset) {
            let region = option_present(provider, "region")?.or(alias_region);
            return super::compiled_credential(compiled, region, wire, &self.registry);
        }
        resolve(&hofund_provider::Resolve {
            id: preset,
            region: option_present(provider, "region")?,
            wire: Some(wire),
            base_url: "",
            project: "",
            location: "",
        })
        .map(|resolved| resolved.credential)
        .map_err(show)
    }

    async fn fetch_exact(
        &self,
        provider: &hofund_config::Provider,
        planned: &Planned,
        endpoint: &Endpoint,
        host: &HostContext,
        url: &str,
        messages_only: bool,
    ) -> Result<ProviderModels, DaemonError> {
        let redactor = Redactor::new(provider, endpoint, Some(url));
        let learning = messages_only && self.exact_learning(provider, planned, endpoint);
        let learned = Arc::new(Mutex::new(self.learned_auth(provider, planned.wire)));
        let learned_for_page = Arc::clone(&learned);
        let rows = after_id(url, |page| {
            let learned = Arc::clone(&learned_for_page);
            async move {
                let supplied = *learned.lock().unwrap_or_else(|error| error.into_inner());
                let choice = supplied.unwrap_or(AuthChoice::Both);
                let call = exact_call(provider, planned, endpoint, &page, supplied, learning);
                let mut result = call.map_err(ListFailure)?.send(host.http()).await;
                // reference: internal/provider/models.go:651-654 and internal/catalog/live.go:530-541
                if learning
                    && choice == AuthChoice::Both
                    && result.as_ref().is_err_and(|error| {
                        error.status() == Some(401)
                            && hofund_provider::both_auth_refused(401, error.message().as_bytes())
                    })
                {
                    for single in [AuthChoice::XApiKey, AuthChoice::Bearer] {
                        result =
                            exact_call(provider, planned, endpoint, &page, Some(single), learning)
                                .map_err(ListFailure)?
                                .send(host.http())
                                .await;
                        let refused = result
                            .as_ref()
                            .is_err_and(|error| matches!(error.status(), Some(401 | 403)));
                        if !refused {
                            *learned.lock().unwrap_or_else(|error| error.into_inner()) =
                                Some(single);
                            break;
                        }
                    }
                }
                let reply = result.map_err(ListFailure)?;
                if reply.status() != 200 {
                    return Err(ListFailure(PluginError::status_error(
                        reply.status(),
                        format!("{}: HTTP {}", safe_url(&page), reply.status()),
                    )));
                }
                Ok(reply.into_body())
            }
        })
        .await;
        let rows = match rows {
            Ok(rows) => rows,
            Err(error) => return Err(redactor.clean(error.0)),
        };
        if let Some(choice) = *learned.lock().unwrap_or_else(|error| error.into_inner()) {
            self.set_auth(&provider.id, choice);
        }
        let (models, dropped) = hofund_provider::parse_model_rows(&rows).map_err(show)?;
        let mut merged = Merged::default();
        merged.add(&models, &dropped);
        Ok(ProviderModels {
            models: merged.finish(),
            kind: ListKind::Live,
            corrected_base: None,
            base_model_fallback: false,
            side_errors: Vec::new(),
        })
    }

    async fn append_public_drawers(
        &self,
        host: &HostContext,
        base: &str,
        models: &mut Vec<ModelSpec>,
    ) {
        let matching = self.public_drawers_override
            || self
                .host(base)
                .is_some_and(|host| host.trim_start_matches("www.") == "aihubmix.com");
        if !matching {
            return;
        }
        let Ok(reply) = Call::get(&self.public_drawers_url)
            .header("accept", "application/json")
            .send(host.http())
            .await
        else {
            return;
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(reply.body()) else {
            return;
        };
        let Some(rows) = value.get("data").and_then(serde_json::Value::as_array) else {
            return;
        };
        for row in rows {
            let Some(id) = row.get("model_id").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let types = row
                .get("types")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let output = row
                .get("output_modalities")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if (!types.contains("image_generation") && !output.contains("image"))
                || !draws_id(id)
                || models.iter().any(|model| model.id() == id)
            {
                continue;
            }
            let name = row
                .get("model_name")
                .and_then(serde_json::Value::as_str)
                .filter(|name| !name.is_empty())
                .unwrap_or(id)
                .to_owned();
            let input = row
                .get("input_modalities")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let image_input = input.split(',').any(|item| item == "image");
            if let Ok(model) = ModelSpec::new(id, Some(name)) {
                models.push(
                    model
                        .with_draws(true)
                        .with_image_input(image_input.then_some(true)),
                );
            }
        }
    }

    fn plan_models(
        &self,
        provider: &hofund_config::Provider,
        model_plan: Option<&ModelPlan>,
        live: Vec<ModelSpec>,
    ) -> Result<Vec<ModelSpec>, DaemonError> {
        let Some(model_plan) = model_plan else {
            return Ok(live);
        };
        let mut static_ids = model_plan.models.clone();
        if provider.preset.as_deref() == Some("google-vertex") {
            static_ids = Vertex::models_for(super::option(provider, "location")?);
        }
        if model_plan.only.is_empty() && (static_ids.is_empty() || !live.is_empty()) {
            return Ok(live);
        }
        let mut planned = live
            .into_iter()
            .filter(|model| model.id().starts_with(&model_plan.only))
            .collect::<Vec<_>>();
        if planned.is_empty() {
            planned = static_models(&static_ids)?;
        }
        if let Some(limits) = &self.catalog_limits {
            planned = planned
                .into_iter()
                .map(|model| {
                    let id = model
                        .id()
                        .strip_prefix(&model_plan.only)
                        .unwrap_or(model.id());
                    let (context, output) = limits.limits(id);
                    let context = if model.context() == 0 {
                        context
                    } else {
                        model.context()
                    };
                    let output = if model.max_output() == 0 {
                        output
                    } else {
                        model.max_output()
                    };
                    model.with_context(context).with_max_output(output)
                })
                .collect();
        }
        Ok(planned)
    }

    #[allow(clippy::too_many_arguments)]
    async fn probe_one(
        &self,
        provider: &hofund_config::Provider,
        planned: &Planned,
        upstream: Arc<dyn UpstreamProvider>,
        protocol: ProtocolId,
        selected: Selected,
        learned: Option<AuthChoice>,
        extra_body: Option<bytes::Bytes>,
    ) -> Result<ProbeOutcome, DaemonError> {
        let span = tracing::info_span!(
            "hofund_provider_test",
            provider.id = provider.id,
            wire = protocol_of(planned.wire),
            kind = "provider_test",
            outcome = tracing::field::Empty
        );
        async {
            let mut query = ProbeQuery::new(protocol, &selected.upstream).map_err(show)?;
            if planned.wire == Wire::Messages
                && !pinned_auth(provider)
                && let Some(choice) = learned
            {
                query = query.with_auth_choice(choice);
            }
            if let Some(extra) = extra_body {
                query = query.with_extra_body(extra);
            }
            let result = upstream.probe(&query).await;
            match result {
                Ok(report) => {
                    if planned.wire == Wire::Messages
                        && let Some(choice) = report.auth_choice()
                    {
                        self.set_auth(&provider.id, choice);
                    }
                    tracing::Span::current().record("outcome", "success");
                    tracing::info!(
                        kind = "provider_test",
                        provider.id = provider.id,
                        wire = protocol_of(planned.wire),
                        outcome = "success"
                    );
                    Ok(ProbeOutcome {
                        model: selected.exposed,
                        report,
                    })
                }
                Err(error) => {
                    tracing::Span::current().record("outcome", "error");
                    tracing::info!(
                        kind = "provider_test",
                        provider.id = provider.id,
                        wire = protocol_of(planned.wire),
                        outcome = "error"
                    );
                    Err(upstream_failure(error))
                }
            }
        }
        .instrument(span)
        .await
    }

    fn exact_learning(
        &self,
        provider: &hofund_config::Provider,
        planned: &Planned,
        endpoint: &Endpoint,
    ) -> bool {
        let host = self.host(&planned.base_url).unwrap_or_default();
        hofund_provider::both_auth_applies(
            &host,
            !endpoint.api_key.is_empty(),
            !planned.base_url.trim().is_empty(),
            endpoint.credential,
            pinned_auth(provider),
        )
    }

    fn learned_auth(&self, provider: &hofund_config::Provider, wire: Wire) -> Option<AuthChoice> {
        if wire != Wire::Messages || pinned_auth(provider) {
            return None;
        }
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&provider.id)
            .auth
            .map(Into::into)
    }

    fn set_auth(&self, provider: &str, choice: AuthChoice) {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .update(provider, |state| state.auth = Some(choice.into()));
    }

    fn extra_body(
        &self,
        provider: &hofund_config::Provider,
    ) -> Result<Option<bytes::Bytes>, DaemonError> {
        let Some(plan) = self.model_plan(provider)? else {
            return Ok(None);
        };
        plan.extra_body
            .map(|body| {
                serde_json::from_str::<serde_json::Value>(&body)
                    .map_err(|_| DaemonError::new("preset probe extra body is not JSON"))
                    .and_then(|value| {
                        serde_json::to_vec(&value)
                            .map(bytes::Bytes::from)
                            .map_err(|_| {
                                DaemonError::new("preset probe extra body cannot be encoded")
                            })
                    })
            })
            .transpose()
    }
}

#[allow(clippy::too_many_arguments)]
fn list_only_raw(
    provider: &hofund_config::Provider,
    planned: &Planned,
    endpoint: &Endpoint,
    model_plan: Option<&ModelPlan>,
    configured_plugin: &str,
    wire: Wire,
    base: &str,
    credential: Credential,
) -> Result<(&'static str, RawTable, bool), DaemonError> {
    let plugin_id = implementation_id(wire);
    let mut raw = RawTable::new();
    raw.insert(
        "id",
        RawValue::String(format!("{}#list:{}", provider.id, wire_name(wire))),
    );
    raw.insert("base_url", RawValue::String(base.to_owned()));
    if let Some(preset) = &provider.preset {
        raw.insert("preset", RawValue::String(preset.clone()));
    }
    match wire {
        Wire::Chat => raw.insert("protocol", RawValue::String("chat".to_owned())),
        Wire::Responses => raw.insert("protocol", RawValue::String("responses".to_owned())),
        _ => {}
    }
    raw.insert(
        "credential",
        RawValue::String(credential_name(credential).to_owned()),
    );
    raw.insert("api_key", RawValue::String(endpoint.api_key.clone()));
    let mut headers = RawTable::new();
    for (name, value) in &endpoint.headers {
        headers.insert(name.clone(), RawValue::String(value.clone()));
    }
    raw.insert("headers", RawValue::Table(headers));
    if let Some(quirks) = model_plan.and_then(|plan| plan.quirks.as_ref()) {
        raw.insert("quirks", RawValue::String(quirks.clone()));
    }
    let own = plugin_id == configured_plugin && wire != planned.wire;
    if own {
        insert_options(&mut raw, provider)?;
    }
    Ok((plugin_id, raw, own))
}

/// A provider's failure, keeping its kind and status for the control plane.
fn upstream_failure(error: PluginError) -> DaemonError {
    let message = error.to_string();
    let converted = crate::adapt::provider_error(error);
    DaemonError::new(message)
        .with_retry_after(converted.retry_after)
        .with_kind(ErrorKind::Upstream {
            kind: converted.kind,
            status: converted.status,
        })
}

fn provider<'a>(config: &'a Config, id: &str) -> Result<&'a hofund_config::Provider, DaemonError> {
    config
        .providers
        .iter()
        .find(|provider| provider.id == id)
        .ok_or_else(|| {
            DaemonError::new(format!("unknown provider `{id}`")).with_kind(ErrorKind::NotFound)
        })
}

fn bounded_kind(wanted: ListKind, bound: ListKind) -> ListKind {
    if wanted.within(bound) {
        return wanted;
    }
    match bound.rank() {
        3 => ListKind::Live,
        2 => ListKind::Deployments,
        1 => ListKind::Catalog,
        _ => ListKind::Unsupported,
    }
}

#[derive(Default)]
struct Merged {
    models: Vec<ModelSpec>,
    dropped: HashSet<String>,
}

impl Merged {
    fn add(&mut self, models: &[ModelSpec], dropped: &[String]) {
        self.dropped.extend(dropped.iter().cloned());
        for incoming in models {
            if let Some(index) = self
                .models
                .iter()
                .position(|model| model.id() == incoming.id())
            {
                self.models[index] = merge_model(&self.models[index], incoming);
            } else {
                self.models.push(incoming.clone());
            }
        }
    }

    fn finish(self) -> Vec<ModelSpec> {
        self.models
            .into_iter()
            .filter(|model| !self.dropped.contains(model.id()))
            .collect()
    }
}

fn merge_model(left: &ModelSpec, right: &ModelSpec) -> ModelSpec {
    let image_input = match (left.image_input(), right.image_input()) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None,
    };
    let model = ModelSpec::new(
        left.id(),
        first(left.display_name(), right.display_name()).map(str::to_owned),
    )
    .unwrap_or_else(|_| unreachable!("an existing model id is non-empty"));
    let model = model
        .with_context(if left.context() == 0 {
            right.context()
        } else {
            left.context()
        })
        .with_max_output(if left.max_output() == 0 {
            right.max_output()
        } else {
            left.max_output()
        })
        .with_input_modalities(first_slice(
            left.input_modalities(),
            right.input_modalities(),
        ))
        .with_output_modalities(first_slice(
            left.output_modalities(),
            right.output_modalities(),
        ))
        .with_image_input(image_input)
        .with_endpoints(first_slice(left.endpoints(), right.endpoints()))
        .with_draws(left.draws() || right.draws())
        .with_films(left.films() || right.films())
        .with_decides(left.decides() || right.decides())
        .with_efforts(first_slice(left.efforts(), right.efforts()))
        .with_service_tiers(first_slice(left.service_tiers(), right.service_tiers()));
    match left.web_search().or_else(|| right.web_search()) {
        Some(search) => model.with_web_search(search),
        None => model,
    }
}

fn first<'a>(left: Option<&'a str>, right: Option<&'a str>) -> Option<&'a str> {
    left.filter(|value| !value.is_empty()).or(right)
}

fn first_slice(left: &[String], right: &[String]) -> Vec<String> {
    if left.is_empty() {
        right.to_vec()
    } else {
        left.to_vec()
    }
}

fn static_models(ids: &[String]) -> Result<Vec<ModelSpec>, DaemonError> {
    ids.iter()
        .map(|id| ModelSpec::new(id, Some(id.clone())).map_err(show))
        .collect()
}

struct Selected {
    exposed: String,
    upstream: String,
}

fn select_model(
    config: &Config,
    provider: &hofund_config::Provider,
    planned: &Planned,
    listed: &[ModelSpec],
    picks: &[String],
    named: Option<&str>,
) -> Result<Selected, DaemonError> {
    if let Some(named) = named {
        let upstream = exposed(config, &provider.id)
            .into_iter()
            .find(|(route, _)| route == named)
            .map_or_else(|| named.to_owned(), |(_, upstream)| upstream);
        if let Some(model) = listed
            .iter()
            .find(|model| model.id() == named || model.id() == upstream)
            && names_protocols(model.endpoints())
            && !serves(planned.wire, model.endpoints())
        {
            return Err(DaemonError::new(format!(
                "model `{named}` is not served on this protocol ({})",
                protocol_of(planned.wire)
            ))
            .with_kind(ErrorKind::Selection));
        }
        if listed
            .iter()
            .any(|model| model.id() == named && model.decides())
        {
            return Err(DaemonError::new(NO_MODEL).with_kind(ErrorKind::Selection));
        }
        return Ok(Selected {
            exposed: named.to_owned(),
            upstream: upstream.to_owned(),
        });
    }

    let mut pools = Vec::new();
    let mut exposed = exposed(config, &provider.id);
    for id in picks {
        if !exposed
            .iter()
            .any(|(route, upstream)| route == id || upstream == id)
        {
            exposed.push((id.clone(), id.clone()));
        }
    }
    if !exposed.is_empty() {
        pools.push(exposed);
    }
    pools.push(
        listed
            .iter()
            .map(|model| (model.id().to_owned(), model.id().to_owned()))
            .collect(),
    );
    // reference: internal/provider/test.go:405-442
    for first_pass in [true, false] {
        for pool in &pools {
            for (route, upstream) in pool {
                let model = listed
                    .iter()
                    .find(|model| model.id() == *route || model.id() == *upstream);
                if model.is_some_and(|model| model.decides()) {
                    continue;
                }
                if first_pass {
                    if image_api(route) || image_api(upstream) {
                        continue;
                    }
                    let open_code = provider
                        .preset
                        .as_deref()
                        .is_some_and(|preset| preset.starts_with("opencode"));
                    let named_only = open_code && planned.wire == Wire::Generative;
                    if let Some(model) = model {
                        if names_protocols(model.endpoints()) {
                            if !serves(planned.wire, model.endpoints()) {
                                continue;
                            }
                        } else if open_code {
                            if named_only {
                                continue;
                            }
                        } else if (planned.wire == Wire::Messages
                            && !claude(model.id())
                            && !claude(upstream))
                            || named_only
                        {
                            continue;
                        }
                    } else if (planned.wire == Wire::Messages && !claude(upstream)) || named_only {
                        continue;
                    }
                }
                return Ok(Selected {
                    exposed: route.clone(),
                    upstream: upstream.clone(),
                });
            }
        }
    }
    Err(DaemonError::new(NO_MODEL).with_kind(ErrorKind::Selection))
}

fn exposed(config: &Config, provider: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for route in &config.routes {
        for upstream in &route.upstreams {
            if upstream.provider() == Some(provider) {
                out.push((
                    route.model.clone(),
                    upstream
                        .model
                        .clone()
                        .unwrap_or_else(|| route.model.clone()),
                ));
            }
        }
    }
    out
}

fn image_api(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    IMAGE_APIS.iter().any(|needle| id.contains(needle))
}

fn claude(id: &str) -> bool {
    id.rsplit('/')
        .next()
        .unwrap_or(id)
        .to_ascii_lowercase()
        .starts_with("claude")
}

fn draws_id(id: &str) -> bool {
    const NEEDLES: &[&str] = &[
        "image",
        "imagen",
        "imagine",
        "dall-e",
        "flux",
        "seedream",
        "cogview",
        "stable-diffusion",
        "sdxl",
        "wanx",
        "kolors",
        "hidream",
        "banana",
    ];
    let id = id.to_ascii_lowercase();
    !id.contains("deep-research") && NEEDLES.iter().any(|needle| id.contains(needle))
}

fn names_protocols(endpoints: &[String]) -> bool {
    endpoints.iter().any(|endpoint| {
        [
            Wire::Chat,
            Wire::Responses,
            Wire::Messages,
            Wire::Generative,
            Wire::Vertex,
            Wire::Converse,
        ]
        .into_iter()
        .any(|wire| endpoint_matches(wire, endpoint))
    })
}

fn serves(wire: Wire, endpoints: &[String]) -> bool {
    endpoints
        .iter()
        .any(|endpoint| endpoint_matches(wire, endpoint))
}

fn endpoint_matches(wire: Wire, endpoint: &str) -> bool {
    let endpoint = endpoint.to_ascii_lowercase();
    match wire {
        Wire::Chat => {
            endpoint == "chat"
                || endpoint.contains("openai-chat")
                || endpoint.contains("chat/completions")
        }
        Wire::Responses => {
            endpoint == "responses"
                || endpoint.contains("openai-responses")
                || endpoint.ends_with("/responses")
        }
        Wire::Messages => {
            endpoint == "anthropic"
                || endpoint == "messages"
                || endpoint.contains("anthropic-messages")
                || endpoint.ends_with("/messages")
        }
        Wire::Generative | Wire::Vertex => {
            endpoint == "gemini"
                || endpoint == "generative"
                || endpoint.contains("generatecontent")
                || endpoint.contains("generative-language")
        }
        Wire::Converse => endpoint == "converse" || endpoint.contains("bedrock-converse"),
    }
}

fn pinned_auth(provider: &hofund_config::Provider) -> bool {
    provider.headers.keys().any(|name| {
        name.eq_ignore_ascii_case("authorization") || name.eq_ignore_ascii_case("x-api-key")
    })
}

fn exact_call(
    provider: &hofund_config::Provider,
    planned: &Planned,
    endpoint: &Endpoint,
    url: &str,
    learned: Option<AuthChoice>,
    learning: bool,
) -> Result<Call, PluginError> {
    let mut call = Call::get(url).header("accept", "application/json");
    if planned.wire == Wire::Messages {
        call = call.header("anthropic-version", "2023-06-01");
    }
    if !endpoint.api_key.is_empty() {
        let secret = SecretString::from(endpoint.api_key.clone());
        let mut style = match endpoint.credential {
            Credential::Default => match planned.wire {
                Wire::Messages => CredentialStyle::XApiKey,
                Wire::Generative => CredentialStyle::NamedHeader(
                    hofund_plugin_api::HeaderName::new("x-goog-api-key"),
                ),
                _ => CredentialStyle::Bearer,
            },
            Credential::Bearer => CredentialStyle::Bearer,
            Credential::ApiKey => CredentialStyle::ApiKey,
            Credential::XApiKey => CredentialStyle::XApiKey,
            Credential::MessagesBoth => CredentialStyle::MessagesBoth,
            Credential::GoogApiKey => {
                CredentialStyle::NamedHeader(hofund_plugin_api::HeaderName::new("x-goog-api-key"))
            }
        };
        if learning && learned.is_none() {
            style = CredentialStyle::MessagesBoth;
        }
        let style = match learned {
            Some(AuthChoice::XApiKey) => CredentialStyle::XApiKey,
            Some(AuthChoice::Bearer) => CredentialStyle::Bearer,
            Some(AuthChoice::Both) => CredentialStyle::MessagesBoth,
            _ => style,
        };
        // A user-pinned auth header wins over the credential header it would
        // duplicate; other credential headers still apply.
        let pinned = |name: &str| {
            provider
                .headers
                .keys()
                .any(|header| header.eq_ignore_ascii_case(name))
        };
        let collides = match &style {
            CredentialStyle::Bearer => pinned("authorization"),
            CredentialStyle::XApiKey => pinned("x-api-key"),
            CredentialStyle::MessagesBoth => pinned_auth(provider),
            _ => false,
        };
        if !collides {
            call = call.credential(style, &secret)?;
        }
    }
    for (name, value) in &provider.headers {
        call = call.sensitive_header(name, value);
    }
    Ok(call)
}

struct ListFailure(PluginError);

impl From<PageError> for ListFailure {
    fn from(error: PageError) -> Self {
        let message = error.to_string();
        if error.kind() == PageErrorKind::Budget {
            Self(PluginError::timeout(message))
        } else {
            Self(PluginError::decode(message))
        }
    }
}

#[derive(Clone)]
struct Redactor {
    secrets: Vec<String>,
}

impl Redactor {
    fn new(
        provider: &hofund_config::Provider,
        endpoint: &Endpoint,
        models_url: Option<&str>,
    ) -> Self {
        let mut secrets: Vec<String> = std::iter::once(endpoint.api_key.clone())
            .chain(provider.headers.values().cloned())
            .filter(|secret| !secret.is_empty())
            .collect();
        if let Some(url) = models_url.and_then(|url| url::Url::parse(url).ok()) {
            secrets.extend(
                url.query_pairs()
                    .map(|(_, value)| value.into_owned())
                    .filter(|value| !value.is_empty()),
            );
        }
        Self { secrets }
    }

    fn clean(&self, error: PluginError) -> DaemonError {
        let message = self.text(error.message());
        let mut cleaned = upstream_failure(error);
        cleaned.message = message;
        cleaned
    }

    fn clean_error(&self, error: DaemonError) -> DaemonError {
        DaemonError::new(self.text(&error.to_string()))
            .with_retry_after(error.retry_after())
            .with_kind(error.kind())
    }

    fn text(&self, text: &str) -> String {
        let mut clean = safe_urls_in(text);
        for secret in &self.secrets {
            clean = redact_token(&clean, secret);
        }
        clean
    }
}

/// Rewrites every `http://` or `https://` URL embedded in `text` with
/// [`safe_url`], so userinfo and query values of any length (including
/// pagination URLs that differ from the configured models URL, and URLs quoted
/// inside transport errors) are never printed verbatim.
fn safe_urls_in(text: &str) -> String {
    let mut clean = String::with_capacity(text.len());
    let mut remaining = text;
    loop {
        let start = match (remaining.find("http://"), remaining.find("https://")) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) | (None, Some(a)) => a,
            (None, None) => break,
        };
        clean.push_str(&remaining[..start]);
        let rest = &remaining[start..];
        let end = rest
            .find(|character: char| {
                character.is_whitespace() || matches!(character, ')' | '"' | '\'' | '<' | '>' | '`')
            })
            .unwrap_or(rest.len());
        clean.push_str(&safe_url(&rest[..end]));
        remaining = &rest[end..];
    }
    clean.push_str(remaining);
    clean
}

fn url_host(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
}

fn unix_ms() -> u64 {
    system_ms(std::time::SystemTime::now())
}

fn system_ms(now: std::time::SystemTime) -> u64 {
    let millis = now
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    u64::try_from(millis).unwrap_or(u64::MAX)
}

fn duration_ms(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn wire_name(wire: Wire) -> &'static str {
    match wire {
        Wire::Chat => "chat",
        Wire::Responses => "responses",
        Wire::Messages => "anthropic",
        Wire::Generative => "generative",
        Wire::Vertex => "vertex",
        Wire::Converse => "converse",
    }
}

#[cfg(test)]
mod tests {
    use hofund_plugin_api::{ListKind, ModelSpec, RawValue};
    use hofund_provider::{Credential, ModelPlan, Wire};

    use super::{Merged, bounded_kind, list_only_raw};

    fn redaction_fixture(models_url: Option<&str>) -> super::Redactor {
        let config = hofund_config::load(
            "[[providers]]\nid = \"fixture\"\nprotocol = \"openai-chat\"\nbase_url = \"https://example.test/v1\"\nheaders = { x-short = \"tiny\" }\n",
        )
        .expect("config");
        let mut endpoint = hofund_provider::Endpoint::new("fixture", "https://example.test/v1");
        endpoint.api_key = "secret-token".to_owned();
        super::Redactor::new(&config.providers[0], &endpoint, models_url)
    }

    #[test]
    fn redactor_masks_only_whole_tokens_of_at_least_eight_characters() {
        let redactor = redaction_fixture(None);
        assert_eq!(
            redactor.text("secret-token prefixsecret-token secret-tokensuffix"),
            "[redacted] prefixsecret-token secret-tokensuffix"
        );
        assert_eq!(redactor.text("tiny tinyish"), "tiny tinyish");
    }

    #[test]
    fn redactor_strips_userinfo_and_short_query_values() {
        let url = "https://user:pass@example.test/models?key=tiny#fragment";
        let redactor = redaction_fixture(Some(url));
        assert_eq!(redactor.text(url), "https://example.test/models?<redacted>");
    }

    #[test]
    fn redactor_strips_short_query_values_from_any_url_in_the_text() {
        let redactor = redaction_fixture(Some("https://example.test/models?key=tiny"));
        let message = "error sending request for url (https://example.test/models?key=tiny&after_id=x&limit=1000): operation timed out";
        let clean = redactor.text(message);
        assert!(!clean.contains("tiny"), "{clean}");
        assert_eq!(
            clean,
            "error sending request for url (https://example.test/models?<redacted>): operation timed out"
        );
        let other = redactor.text("see 'http://u:p@other.test/x?a=1' and \"https://a.test/b\"");
        assert_eq!(
            other,
            "see 'http://other.test/x?<redacted>' and \"https://a.test/b\""
        );
    }

    #[test]
    fn compiled_models_credential_uses_the_region_resolved_host() {
        let host = super::ModelsHost::new(
            std::time::Duration::from_secs(1),
            std::time::Duration::from_secs(1),
        );
        let fixture = hofund_config::load(
            "[[providers]]\nid = \"fixture\"\npreset = \"fixture-anthropic-host\"\nprotocol = \"anthropic-messages\"\n",
        )
        .expect("fixture config");
        let actual = host
            .credential_with(
                &fixture.providers[0],
                Wire::Messages,
                hofund_plugin_registry::find_fixture_preset,
            )
            .expect("fixture credential");
        assert_eq!(actual, Credential::XApiKey);

        let catalog = hofund_config::load(
            "[[providers]]\nid = \"catalog\"\npreset = \"anthropic\"\nprotocol = \"anthropic-messages\"\n",
        )
        .expect("catalog config");
        let expected = host
            .credential(&catalog.providers[0], Wire::Messages)
            .expect("catalog credential");
        assert_eq!(actual, expected);
    }

    #[test]
    fn list_only_raw_has_exact_keys_and_options_never_cross_plugins() {
        let config = hofund_config::load(
            "[[providers]]\nid = \"configured\"\npreset = \"google\"\nprotocol = \"openai-chat\"\napi_key = \"test-key\"\nheaders = { x-test = \"header-value\" }\n[providers.options]\nlocation = \"env:LOCATION\"\n[providers.models]\nurl = \"env:MODELS_URL\"\n",
        )
        .expect("config");
        let provider = &config.providers[0];
        let planned = super::Planned {
            base_url: "https://chat.example/v1".to_owned(),
            wire: Wire::Chat,
            protocol: Some(hofund_plugin_api::ProtocolId::OPENAI_CHAT),
            credential: Credential::Bearer,
        };
        let endpoint = super::endpoint_for(provider, &planned).expect("endpoint");
        let model_plan = ModelPlan {
            quirks: Some("profile".to_owned()),
            ..ModelPlan::default()
        };
        let (_, own, resolve_options) = list_only_raw(
            provider,
            &planned,
            &endpoint,
            Some(&model_plan),
            "openai-compat",
            Wire::Responses,
            "https://responses.example/v1",
            Credential::Bearer,
        )
        .expect("own raw table");
        assert!(resolve_options);
        assert_eq!(
            own.keys().collect::<Vec<_>>(),
            [
                "api_key",
                "base_url",
                "credential",
                "headers",
                "id",
                "location",
                "preset",
                "protocol",
                "quirks"
            ]
        );
        assert_eq!(
            own.get("id"),
            Some(&RawValue::String("configured#list:responses".to_owned()))
        );
        assert_eq!(
            own.get("protocol"),
            Some(&RawValue::String("responses".to_owned()))
        );
        assert_eq!(
            own.get("location"),
            Some(&RawValue::String("env:LOCATION".to_owned()))
        );
        assert!(own.get("models").is_none());
        assert!(own.get("options").is_none());

        let (_, other, resolve_options) = list_only_raw(
            provider,
            &planned,
            &endpoint,
            Some(&model_plan),
            "openai-compat",
            Wire::Generative,
            "https://generative.example/v1beta",
            Credential::GoogApiKey,
        )
        .expect("other raw table");
        assert!(!resolve_options);
        assert_eq!(
            other.keys().collect::<Vec<_>>(),
            [
                "api_key",
                "base_url",
                "credential",
                "headers",
                "id",
                "preset",
                "quirks"
            ]
        );
        assert!(other.get("location").is_none());
        assert!(other.get("protocol").is_none());
        assert!(other.get("url").is_none());
    }

    #[test]
    fn merge_drops_an_id_reported_by_any_side() {
        let model = ModelSpec::new("shared", None).expect("model");
        let mut merged = Merged::default();
        merged.add(std::slice::from_ref(&model), &[]);
        merged.add(&[], &["shared".to_owned()]);
        merged.add(std::slice::from_ref(&model), &[]);
        assert!(merged.finish().is_empty());
    }

    #[test]
    fn kind_never_exceeds_the_factory_bound() {
        assert_eq!(
            bounded_kind(ListKind::Deployments, ListKind::Live),
            ListKind::Deployments
        );
        assert_eq!(
            bounded_kind(ListKind::Live, ListKind::Catalog),
            ListKind::Catalog
        );
        assert_eq!(
            bounded_kind(ListKind::Catalog, ListKind::Unsupported),
            ListKind::Unsupported
        );
        assert_eq!(
            bounded_kind(ListKind::Live, ListKind::Deployments),
            ListKind::Deployments
        );
    }

    #[test]
    fn provider_test_timeout_gives_named_models_longer() {
        assert_eq!(
            super::test_timeout(false),
            std::time::Duration::from_secs(30)
        );
        assert_eq!(
            super::test_timeout(true),
            std::time::Duration::from_secs(90)
        );
        assert_eq!(super::REFRESH_TIMEOUT, std::time::Duration::from_secs(30));
    }

    #[test]
    fn models_refreshed_emission_carries_kind_label_and_counts() {
        use std::sync::Arc;

        use hofund_control_proto::{EventEnvelope, EventPayload, decode_response};

        use crate::events::EventBus;

        let host = super::ModelsHost::new(
            std::time::Duration::from_secs(1),
            std::time::Duration::from_secs(1),
        );
        let bus = Arc::new(EventBus::with_boot_id(
            "00112233445566778899aabbccddeeff".into(),
        ));
        host.attach_events(Arc::clone(&bus));
        host.emit_refreshed("openai", ListKind::Live, 3, true, None);
        host.emit_refreshed(
            "openai",
            ListKind::Catalog,
            0,
            false,
            Some("upstream failed".into()),
        );
        let payloads = bus
            .control_frames()
            .into_iter()
            .map(|frame| {
                decode_response::<EventEnvelope>(
                    hofund_control_proto::API_VERSION,
                    frame.as_bytes(),
                )
                .expect("env")
                .payload
            })
            .collect::<Vec<_>>();
        match payloads.as_slice() {
            [
                EventPayload::ModelsRefreshed(ok),
                EventPayload::ModelsRefreshed(err),
            ] => {
                assert_eq!(ok.provider, "openai");
                assert_eq!(ok.list_kind, "live");
                assert_eq!(ok.model_count, 3);
                assert!(ok.ok);
                assert!(ok.error.is_none());
                assert_eq!(err.list_kind, "catalog");
                assert!(!err.ok);
                assert_eq!(err.error.as_deref(), Some("upstream failed"));
            }
            other => panic!("expected two models_refreshed, got {other:?}"),
        }
    }
}
