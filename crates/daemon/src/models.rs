//! Per-provider model aggregation and connectivity probes.
//!
//! The optional models.dev limits lookup is deliberately small. The kernel
//! cache that implements it arrives with feature-parity row 2.8.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use hofund_config::Config;
use hofund_plugin_api::{
    AuthChoice, CredentialStyle, DynUpstreamFactory, HostContext, ListKind, ListQuery, ModelSpec,
    PluginError, ProbeQuery, ProbeReport, ProtocolId, RawTable, RawValue, SecretString,
    UpstreamProvider,
};
use hofund_plugin_host::HttpLimits;
use hofund_plugin_sdk::{Call, PageError, after_id};
use hofund_provider::{Credential, Endpoint, ModelPlan, Vertex, Wire};
use tracing::Instrument;

use super::{
    DaemonError, Planned, beside, build_raw_plugin, credential_name, endpoint_for,
    implementation_id, insert_options, option_present, plan, plugin_for, protocol_of, resolve,
    show,
};

const NO_MODEL: &str = "no model to try: expose one, or refresh the model list";
const ALL_FAILED: &str = "type its model ids in by hand, or give the URL its list is at";
const PUBLIC_DRAWERS: &str = "https://aihubmix.com/api/v1/models";
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
#[derive(Clone, Debug)]
pub struct ProviderModels {
    /// Merged and planned models.
    pub models: Vec<ModelSpec>,
    /// Resolved list capability after applying the factory bound.
    pub kind: ListKind,
    /// A `/v1` base learned from a successful fallback, when one was found.
    pub corrected_base: Option<String>,
    /// Whether Azure returned its explicitly labelled base-model fallback.
    pub base_model_fallback: bool,
}

/// One selected model and its probe result.
#[derive(Clone, Debug)]
pub struct ProbeOutcome {
    /// Aggregated or exposed model id selected by the host.
    pub model: String,
    /// Result returned by the configured plugin instance.
    pub report: ProbeReport,
}

#[derive(Default)]
struct HostState {
    bases: HashMap<String, String>,
    auth: HashMap<String, AuthChoice>,
}

/// Host-side model list and probe composition, with process-local learned state.
pub struct ModelsHost {
    limits: HttpLimits,
    catalog_limits: Option<Arc<dyn ModelLimits>>,
    public_drawers_url: String,
    public_drawers_override: bool,
    plan_overrides: HashMap<String, ModelPlan>,
    host_overrides: HashMap<String, String>,
    state: Mutex<HostState>,
}

impl ModelsHost {
    /// Creates a host with no models.dev limits lookup.
    #[must_use]
    pub fn new(connect_timeout: std::time::Duration, read_timeout: std::time::Duration) -> Self {
        Self {
            limits: HttpLimits::new(connect_timeout, read_timeout),
            catalog_limits: None,
            public_drawers_url: PUBLIC_DRAWERS.to_owned(),
            public_drawers_override: false,
            plan_overrides: HashMap::new(),
            host_overrides: HashMap::new(),
            state: Mutex::new(HostState::default()),
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
                .list_inner(config_path, provider, &planned, &endpoint)
                .await;
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
        let provider = provider(config, provider_id)?;
        let planned = self.planned(provider)?;
        let endpoint = endpoint_for(provider, &planned)?;
        let redactor = Redactor::new(provider, &endpoint, None);
        let listed = self.list_models(config, config_path, provider_id).await;
        let models = listed
            .as_ref()
            .map_or(&[][..], |list| list.models.as_slice());
        let selected = if named.is_empty() {
            vec![select_model(config, provider, &planned, models, None)?]
        } else {
            named
                .into_iter()
                .map(|name| select_model(config, provider, &planned, models, Some(name)))
                .collect::<Result<Vec<_>, _>>()?
        };
        let upstream = self.configured(provider, &planned, &endpoint, config_path)?;
        let protocol = ProtocolId::try_from(protocol_of(planned.wire)).map_err(show)?;
        let learned = self.learned_auth(&provider.id, planned.wire);
        let extra = self.extra_body(provider)?;
        let results = futures_util::stream::iter(selected.into_iter().map(|selected| {
            let upstream = Arc::clone(&upstream);
            let extra = extra.clone();
            let planned = planned.clone();
            let redactor = redactor.clone();
            async move {
                self.probe_one(
                    provider, &planned, upstream, protocol, selected, learned, extra,
                )
                .await
                .map_err(|error| redactor.clean_error(error))
            }
        }))
        .buffered(4)
        .collect::<Vec<_>>()
        .await;
        Ok(results)
    }

    async fn list_inner(
        &self,
        path: &Path,
        provider: &hofund_config::Provider,
        planned: &Planned,
        endpoint: &Endpoint,
    ) -> Result<ProviderModels, DaemonError> {
        let model_plan = self.model_plan(provider)?;
        let configured_plugin = plugin_for(provider, planned.wire);
        let kind = self.kind(provider, planned, model_plan.as_ref(), &configured_plugin)?;
        tracing::Span::current().record("kind", format_args!("{kind:?}"));
        let configured = self.configured(provider, planned, endpoint, path)?;
        let host = self.runtime_context(provider, endpoint, path, &configured_plugin)?;
        let custom_url = if provider.model_list.url.trim().is_empty() {
            None
        } else {
            Some(hofund_secrets::resolve(provider.model_list.url.trim()).map_err(show)?)
        };
        let redactor = Redactor::new(provider, endpoint, custom_url.as_deref());
        let result = if let Some(url) = custom_url.as_deref().filter(|_| kind != ListKind::Catalog)
        {
            let messages_only = planned.wire == Wire::Messages
                && model_plan.as_ref().is_none_or(|plan| {
                    plan.chat.trim().is_empty() && plan.responses.trim().is_empty()
                });
            self.fetch_exact(provider, planned, endpoint, &host, url, messages_only)
                .await
                .map(|list| (list, url.to_owned()))
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
            )
            .await
        };
        let (mut merged, winning_base) = result.map_err(|error| redactor.clean_error(error))?;
        let models = self.plan_models(
            provider,
            model_plan.as_ref(),
            std::mem::take(&mut merged.models),
        )?;
        merged.models = models;
        self.append_public_drawers(&host, &winning_base, &mut merged.models)
            .await;
        merged.kind = kind;
        Ok(merged)
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
    ) -> Result<(ProviderModels, String), DaemonError> {
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
                },
                planned.base_url.clone(),
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
                if !base.trim().is_empty() {
                    sides.push((wire, base.clone(), wire == planned.wire));
                }
            }
        }
        if sides.is_empty() {
            sides.push((planned.wire, planned.base_url.clone(), true));
        }

        let has_openai = sides
            .iter()
            .any(|(wire, _, _)| matches!(wire, Wire::Chat | Wire::Responses));
        let mut seen_bases = HashSet::new();
        let mut merged_lists = HashSet::new();
        let mut merged = Merged::default();
        let mut errors = Vec::new();
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
                self.learned_auth(&provider.id, wire),
                endpoint.headers
            );
            if !seen_bases.insert((base.trim_end_matches('/').to_owned(), auth_key)) {
                continue;
            }
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
                        continue;
                    }
                }
            };
            let query = match self.learned_auth(&provider.id, wire) {
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
                        continue;
                    }
                    let auth_key = format!(
                        "{}\0{:?}\0{:?}",
                        endpoint.api_key,
                        list.auth_choice()
                            .or_else(|| self.learned_auth(&provider.id, wire)),
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
                    if winning_base.is_none() {
                        winning_base = Some(base.clone());
                        if list.v1_fallback() && matches!(wire, Wire::Chat | Wire::Responses) {
                            let fixed = format!("{}/v1", base.trim_end_matches('/'));
                            self.state
                                .lock()
                                .unwrap_or_else(|error| error.into_inner())
                                .bases
                                .insert(provider.id.clone(), fixed.clone());
                            corrected_base = Some(fixed);
                        }
                    }
                    merged.add(list.models(), list.dropped());
                }
                Err(error) => {
                    let message = error.to_string();
                    if !errors.contains(&message) {
                        errors.push(message);
                    }
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
            if kind == ListKind::Deployments {
                return Err(DaemonError::new(errors.join("; ")));
            }
            return Err(DaemonError::new(format!(
                "{} — {ALL_FAILED}",
                errors.join("; ")
            )));
        }
        Ok((
            ProviderModels {
                models: merged.finish(),
                kind,
                corrected_base,
                base_model_fallback: fallback,
            },
            winning_base.unwrap_or_else(|| planned.base_url.clone()),
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
            &endpoint.api_key,
            path,
            self.limits,
            own,
            false,
        )
    }

    fn configured(
        &self,
        provider: &hofund_config::Provider,
        planned: &Planned,
        endpoint: &Endpoint,
        path: &Path,
    ) -> Result<Arc<dyn UpstreamProvider>, DaemonError> {
        let plugin_id = plugin_for(provider, planned.wire);
        let raw = if let Some(preset) = &provider.preset
            && let Some(compiled) = hofund_plugin_registry::preset(preset)
        {
            super::raw_from_preset(compiled, provider)?
        } else {
            super::raw_from_endpoint(provider, planned, endpoint)?
        };
        build_raw_plugin(
            provider,
            &plugin_id,
            raw,
            &endpoint.api_key,
            path,
            self.limits,
            true,
            false,
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

    fn planned(&self, provider: &hofund_config::Provider) -> Result<Planned, DaemonError> {
        let mut planned = plan(provider)?;
        if provider.base_url.trim().is_empty()
            && let Some(model_plan) = self.model_plan(provider)?
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
        if let Some(base) = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .bases
            .get(&provider.id)
            .cloned()
        {
            planned.base_url = base;
        }
        Ok(planned)
    }

    fn model_plan(
        &self,
        provider: &hofund_config::Provider,
    ) -> Result<Option<ModelPlan>, DaemonError> {
        if let Some(plan) = self.plan_overrides.get(&provider.id) {
            return Ok(Some(plan.clone()));
        }
        let Some(preset) = provider.preset.as_deref() else {
            return Ok(None);
        };
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
        let factory = hofund_plugin_registry::factory(plugin_id)
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
        let Some(preset) = provider.preset.as_deref() else {
            return Ok(Credential::Default);
        };
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
        let learned = Arc::new(Mutex::new(self.learned_auth(&provider.id, planned.wire)));
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
        .await
        .map_err(|error| redactor.clean(error.0))?;
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
                    Err(show(error))
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

    fn learned_auth(&self, provider: &str, wire: Wire) -> Option<AuthChoice> {
        (wire == Wire::Messages)
            .then(|| {
                self.state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .auth
                    .get(provider)
                    .copied()
            })
            .flatten()
    }

    fn set_auth(&self, provider: &str, choice: AuthChoice) {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .auth
            .insert(provider.to_owned(), choice);
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

fn provider<'a>(config: &'a Config, id: &str) -> Result<&'a hofund_config::Provider, DaemonError> {
    config
        .providers
        .iter()
        .find(|provider| provider.id == id)
        .ok_or_else(|| DaemonError::new(format!("unknown provider `{id}`")))
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
            )));
        }
        if listed
            .iter()
            .any(|model| model.id() == named && model.decides())
        {
            return Err(DaemonError::new(NO_MODEL));
        }
        return Ok(Selected {
            exposed: named.to_owned(),
            upstream: upstream.to_owned(),
        });
    }

    let mut pools = Vec::new();
    let exposed = exposed(config, &provider.id);
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
    Err(DaemonError::new(NO_MODEL))
}

fn exposed(config: &Config, provider: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for route in &config.routes {
        for upstream in &route.upstreams {
            if upstream.provider == provider {
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
        if style != CredentialStyle::MessagesBoth || !pinned_auth(provider) {
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
        if message.contains("time budget") {
            Self(PluginError::timeout(message))
        } else {
            Self(PluginError::decode(message))
        }
    }
}

#[derive(Clone)]
struct Redactor {
    secrets: Vec<String>,
    query: Option<String>,
    models_url: Option<String>,
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
        Self {
            secrets,
            query: models_url
                .and_then(|url| url.split_once('?'))
                .map(|(_, query)| query.to_owned()),
            models_url: models_url.map(str::to_owned),
        }
    }

    fn clean(&self, error: PluginError) -> DaemonError {
        DaemonError::new(self.text(error.message()))
    }

    fn clean_error(&self, error: DaemonError) -> DaemonError {
        DaemonError::new(self.text(&error.to_string()))
    }

    fn text(&self, text: &str) -> String {
        let mut clean = text.to_owned();
        if let Some(url) = &self.models_url {
            clean = clean.replace(url, &safe_url(url));
        }
        if let Some(query) = &self.query
            && !query.is_empty()
        {
            clean = clean.replace(query, "<redacted>");
        }
        for secret in &self.secrets {
            clean = clean.replace(secret, "[redacted]");
        }
        clean
    }
}

fn safe_url(url: &str) -> String {
    let without_fragment = url.split('#').next().unwrap_or(url);
    without_fragment.split_once('?').map_or_else(
        || without_fragment.to_owned(),
        |(base, _)| format!("{base}?<redacted>"),
    )
}

fn url_host(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
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
}
