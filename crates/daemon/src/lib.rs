//! Composition root for the local gateway process.
//!
//! This crate reads configuration, resolves credentials, builds providers, and
//! starts the HTTP server. The binary parses arguments and calls these
//! functions.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hofund_codec::{AnthropicMessages, Codec, GenerativeLanguage, OpenAiChat, OpenAiResponses};
use hofund_config::{Config, Value, load_path};
use hofund_gateway::{GENERATIVE_STREAM, GENERATIVE_UNARY, Gateway};
use hofund_plugin_api::{
    AgentDescription, DynUpstreamFactory, PluginId, ProtocolId, RawTable, RawValue,
    UpstreamProvider,
};
use hofund_plugin_host::HttpLimits;

pub use hofund_gateway::Running;
use hofund_provider::{Credential, Endpoint, Provider, Wire, resolve};
use hofund_quirks::named;
use hofund_router::{CooldownPolicy, Cooldowns, FailoverPolicy, Snapshot};
use hofund_secrets::TokenFile;
use hofund_usage::Ledger;

mod adapt;
#[doc(hidden)]
pub mod control_map;
mod membership;
mod models;
mod models_state;

pub use models::{ModelLimits, ModelsHost, Picks, ProbeOutcome, ProviderModels, RefreshTiming};

/// A configuration, credential, or startup failure.
#[derive(Debug)]
pub struct DaemonError {
    message: String,
    retry_after: Option<std::time::Duration>,
}

impl DaemonError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retry_after: None,
        }
    }

    fn with_retry_after(mut self, retry_after: Option<std::time::Duration>) -> Self {
        self.retry_after = retry_after;
        self
    }

    fn retry_after(&self) -> Option<std::time::Duration> {
        self.retry_after
    }
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for DaemonError {}

/// Checks `path` and builds the providers it names, without binding a port.
///
/// # Errors
///
/// Returns [`DaemonError`] at the first structural or provider error.
pub fn check(path: &Path) -> Result<(), DaemonError> {
    let config = load_path(path).map_err(show)?;
    warn(&config);
    let _ = providers(&config, path, None, Mode::Check, None, None)?;
    Ok(())
}

/// Reads the bearer token, creating the file on first use.
///
/// # Errors
///
/// Returns [`DaemonError`] when the config or the token file fails.
pub fn token_show(path: &Path) -> Result<String, DaemonError> {
    token_file(path)?.load_or_create().map_err(show)
}

/// Replaces the bearer token and returns the new value.
///
/// # Errors
///
/// Returns [`DaemonError`] when the config or the token file fails.
pub fn token_rotate(path: &Path) -> Result<String, DaemonError> {
    token_file(path)?.rotate().map_err(show)
}

/// The listen URL and bearer token an agent adapter should write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalGateway {
    /// `http://` plus the configured listen address. IPv6 addresses keep their
    /// brackets, as in `http://[::1]:3425`.
    pub base_url: String,
    /// Bearer token from the token file. The file is created on first use.
    pub token: String,
}

/// Resolves the gateway URL and token for `path`'s config.
///
/// Agent adapters never read this file themselves. The CLI calls this and
/// passes the result down.
///
/// # Errors
///
/// Returns [`DaemonError`] when the config or the token file fails.
pub fn local_gateway(path: &Path) -> Result<LocalGateway, DaemonError> {
    let config = load_path(path).map_err(show)?;
    let token = TokenFile::new(beside(path, &config.token_file))
        .load_or_create()
        .map_err(show)?;
    Ok(LocalGateway {
        base_url: format!("http://{}", config.listen),
        token,
    })
}

/// The configuration file the menu-bar app reads.
///
/// `HOFUND_CONFIG` selects the file when it is set. Otherwise the path is
/// `hofund/hofund.toml` under the platform config directory.
pub use hofund_config::user_config_path;

/// Creates a starter configuration file when `path` is absent.
pub use hofund_config::ensure_config_file;

/// The listen address written in `path`, when the file parses.
#[must_use]
pub fn configured_listen(path: &Path) -> Option<String> {
    load_path(path).ok().map(|config| config.listen.to_string())
}

/// Static agent descriptions compiled into the plugin registry.
#[must_use]
pub fn agent_descriptions() -> &'static [&'static AgentDescription] {
    hofund_plugin_registry::agents()
}

/// Model names declared in `path`'s routes.
///
/// # Errors
///
/// Returns [`DaemonError`] when the config cannot be read.
pub fn route_models(path: &Path) -> Result<Vec<String>, DaemonError> {
    let config = load_path(path).map_err(show)?;
    Ok(config.routes.into_iter().map(|route| route.model).collect())
}

/// The usage table for the database named by `path`'s config.
///
/// # Errors
///
/// Returns [`DaemonError`] when the config or the database fails.
pub fn usage_report(path: &Path) -> Result<String, DaemonError> {
    let config = load_path(path).map_err(show)?;
    let ledger = Ledger::open(&beside(path, &config.usage_file)).map_err(show)?;
    ledger.table().map_err(show)
}

/// Binds the address in the config and serves until shutdown.
///
/// The bound address is available on the returned [`Running`] value.
///
/// # Errors
///
/// Returns [`DaemonError`] when startup fails. A later accept error stays on
/// the server task.
pub async fn start(path: &Path) -> Result<Running, DaemonError> {
    start_inner(path, None, None).await
}

/// Starts the daemon without retaining idle upstream sockets.
///
/// This entry point is for integration tests whose mock servers release and
/// reuse local ports. Production callers use [`start`].
///
/// # Errors
///
/// Returns [`DaemonError`] when startup fails.
#[cfg(feature = "test-util")]
#[doc(hidden)]
pub async fn start_for_test(path: &Path) -> Result<Running, DaemonError> {
    start_inner(path, Some(0), None).await
}

/// Starts a test daemon with short refresh-loop timings.
#[cfg(feature = "test-util")]
#[doc(hidden)]
pub async fn start_for_test_with_refresh(
    path: &Path,
    timing: RefreshTiming,
) -> Result<Running, DaemonError> {
    start_inner(path, Some(0), Some(timing)).await
}

async fn start_inner(
    path: &Path,
    max_idle_per_host: Option<usize>,
    refresh_timing: Option<RefreshTiming>,
) -> Result<Running, DaemonError> {
    let config = load_path(path).map_err(show)?;
    warn(&config);
    let token_path = beside(path, &config.token_file);
    let token = TokenFile::new(&token_path).load_or_create().map_err(show)?;
    let usage = Arc::new(Ledger::open(&beside(path, &config.usage_file)).map_err(show)?);
    let mut models_host = ModelsHost::new(config.connect_timeout, config.read_timeout)
        .with_state_root(beside(path, ".hofund"));
    let gateway = Gateway::with_token_file(
        router(&config, path, max_idle_per_host, Some(&mut models_host))?,
        token,
        token_path,
        inbound_codecs(),
        usage,
    )
    .with_surfaces(hofund_plugin_registry::surfaces());
    let running = gateway
        .bind(config.listen)
        .await
        .map_err(|error| DaemonError::new(error.to_string()))?;
    let models_host = Arc::new(models_host);
    let refresh = models_host.spawn_refresh(
        config,
        path.to_path_buf(),
        refresh_timing.unwrap_or_default(),
    );
    Ok(running.with_task(refresh))
}

/// Serves until the process receives a shutdown signal.
///
/// # Errors
///
/// Returns [`DaemonError`] when startup fails.
pub async fn serve(path: &Path) -> Result<(), DaemonError> {
    let running = start(path).await?;
    eprintln!("listening on http://{}", running.local_addr);
    let _ = tokio::signal::ctrl_c().await;
    running.shutdown().await;
    Ok(())
}

/// Builds a runtime and calls [`serve`].
///
/// # Errors
///
/// Returns [`DaemonError`] when the runtime or startup fails.
pub fn serve_blocking(path: &Path) -> Result<(), DaemonError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(show)?;
    runtime.block_on(serve(path))
}

fn token_file(path: &Path) -> Result<TokenFile, DaemonError> {
    let config = load_path(path).map_err(show)?;
    Ok(TokenFile::new(beside(path, &config.token_file)))
}

fn router(
    config: &Config,
    path: &Path,
    max_idle_per_host: Option<usize>,
    models_host: Option<&mut ModelsHost>,
) -> Result<Snapshot, DaemonError> {
    let cooldowns = Arc::new(Cooldowns::with_clock(
        CooldownPolicy::default(),
        Arc::new(|| tokio::time::Instant::now().into_std()),
    ));
    let built = providers(
        config,
        path,
        max_idle_per_host,
        Mode::Serve,
        models_host,
        Some(Arc::clone(&cooldowns)),
    )?;
    let caps: BTreeMap<&str, u64> = config
        .providers
        .iter()
        .filter_map(|provider| {
            provider
                .max_output_tokens
                .map(|cap| (provider.id.as_str(), cap))
        })
        .collect();
    let mut routes = BTreeMap::new();
    for route in &config.routes {
        let mut list = Vec::new();
        for upstream in &route.upstreams {
            list.push(membership::PlannedHop {
                provider: upstream.provider.clone(),
                wire_model: upstream.model.clone(),
                max_output_tokens: upstream
                    .max_output_tokens
                    .or_else(|| caps.get(upstream.provider.as_str()).copied()),
            });
        }
        routes.insert(route.model.clone(), list);
    }
    let policy = FailoverPolicy {
        connect_errors: config.failover.connect_errors,
        statuses: config.failover.statuses.clone(),
    };
    membership::open(&built, routes, policy, cooldowns)
}

#[derive(Clone, Copy)]
enum Mode {
    Check,
    Serve,
}

fn providers(
    config: &Config,
    path: &Path,
    max_idle_per_host: Option<usize>,
    mode: Mode,
    mut models_host: Option<&mut ModelsHost>,
    cooldowns: Option<Arc<Cooldowns>>,
) -> Result<BTreeMap<String, membership::BuiltProvider>, DaemonError> {
    let mut limits = HttpLimits::new(config.connect_timeout, config.read_timeout);
    if let Some(max_idle_per_host) = max_idle_per_host {
        limits = limits.with_max_idle_per_host(max_idle_per_host);
    }
    let mut built = BTreeMap::new();
    for provider in &config.providers {
        let planned = match models_host.as_deref() {
            Some(host) => host.planned(provider)?,
            None => plan(provider)?,
        };
        let endpoint = endpoint_for(provider, &planned)?;
        let timeout = endpoint.timeout;
        let protocol = protocol_of(planned.wire);
        let plugin_id = plugin_for(provider, planned.wire);
        let adapter = build_plugin(
            provider,
            &planned,
            &endpoint,
            path,
            limits,
            mode,
            cooldowns.clone(),
        )?;
        if let Some(host) = models_host.as_deref_mut() {
            host.register_instance(
                provider.id.clone(),
                adapter.upstream(),
                planned.base_url.clone(),
            );
        }
        let adapter: Arc<dyn Provider> = Arc::new(adapter);
        built.insert(
            provider.id.clone(),
            membership::BuiltProvider {
                adapter,
                protocol,
                timeout,
                plugin_id,
                preset_id: provider.preset.clone().unwrap_or_default(),
            },
        );
    }
    Ok(built)
}

fn endpoint_for(
    provider: &hofund_config::Provider,
    planned: &Planned,
) -> Result<Endpoint, DaemonError> {
    let mut endpoint = Endpoint::new(provider.id.clone(), planned.base_url.clone());
    endpoint.preset = provider.preset.clone();
    endpoint.api_key = hofund_secrets::resolve(&provider.api_key).map_err(show)?;
    endpoint.credential = planned.credential;
    endpoint.headers = provider
        .headers
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    if let Some(profile) = &provider.quirks {
        endpoint.quirks = Some(named(profile).ok_or_else(|| {
            DaemonError::new(format!(
                "provider `{}` names unknown quirk profile `{profile}`",
                provider.id
            ))
        })?);
    }
    Ok(endpoint)
}

type PresetLookup = fn(
    &str,
) -> Option<(
    &'static hofund_plugin_registry::CompiledPreset,
    Option<&'static str>,
)>;

fn plugin_for(provider: &hofund_config::Provider, wire: Wire) -> String {
    plugin_for_with(provider, wire, hofund_plugin_registry::find_preset)
}

fn plugin_for_with(provider: &hofund_config::Provider, wire: Wire, lookup: PresetLookup) -> String {
    if let Some(preset) = &provider.preset
        && let Some((compiled, _)) = lookup(preset)
    {
        if hofund_plugin_registry::builtin_factory_ids().contains(&compiled.plugin) {
            return implementation_id(wire).to_owned();
        }
        return compiled.plugin.to_owned();
    }
    implementation_id(wire).to_owned()
}

fn build_plugin(
    provider: &hofund_config::Provider,
    planned: &Planned,
    endpoint: &Endpoint,
    path: &Path,
    limits: HttpLimits,
    mode: Mode,
    cooldowns: Option<Arc<Cooldowns>>,
) -> Result<adapt::ProviderAdapter, DaemonError> {
    let plugin_id = plugin_for(provider, planned.wire);
    let raw = raw_for(provider, planned, endpoint)?;
    let upstream = build_raw_plugin(
        provider,
        &plugin_id,
        raw,
        &endpoint.api_key,
        path,
        limits,
        true,
        matches!(mode, Mode::Check),
        cooldowns,
    )?;
    Ok(adapt::ProviderAdapter::new(provider.id.clone(), upstream))
}

#[allow(clippy::too_many_arguments)]
fn build_raw_plugin(
    provider: &hofund_config::Provider,
    plugin_id: &str,
    mut raw: RawTable,
    api_key: &str,
    path: &Path,
    limits: HttpLimits,
    resolve_own_options: bool,
    preflight: bool,
    cooldowns: Option<Arc<Cooldowns>>,
) -> Result<Arc<dyn UpstreamProvider>, DaemonError> {
    let factory = hofund_plugin_registry::factory(plugin_id)
        .ok_or_else(|| DaemonError::new(format!("no factory for plugin `{plugin_id}`")))?;
    factory.validate(&raw).map_err(show)?;
    if resolve_own_options {
        resolve_options(&mut raw, provider)?;
    }
    if preflight {
        factory.preflight(&raw).map_err(show)?;
    }
    let plugin = PluginId::try_from(plugin_id).map_err(show)?;
    let root = beside(path, ".hofund");
    let host = if let Some(cooldowns) = cooldowns {
        hofund_plugin_host::runtime_host_with_observe(
            plugin,
            api_key.to_owned(),
            root,
            limits,
            membership::observe_backend(&provider.id, cooldowns),
        )
    } else {
        hofund_plugin_host::runtime_host(plugin, api_key.to_owned(), root, limits)
    }
    .map_err(show)?;
    factory.validate_and_build(&raw, &host).map_err(show)
}

/// The raw plugin table for a provider.
///
/// Catalog rows and builtin-owned compiled presets use the endpoint table,
/// which carries the wire, credential, and preset id. Other compiled presets
/// use their own fragment config.
fn raw_for(
    provider: &hofund_config::Provider,
    planned: &Planned,
    endpoint: &Endpoint,
) -> Result<RawTable, DaemonError> {
    raw_for_with(
        provider,
        planned,
        endpoint,
        hofund_plugin_registry::find_preset,
    )
}

fn raw_for_with(
    provider: &hofund_config::Provider,
    planned: &Planned,
    endpoint: &Endpoint,
    lookup: PresetLookup,
) -> Result<RawTable, DaemonError> {
    if let Some(preset) = &provider.preset
        && let Some((compiled, _)) = lookup(preset)
        && !hofund_plugin_registry::builtin_factory_ids().contains(&compiled.plugin)
    {
        return raw_from_preset(compiled, provider, &planned.base_url);
    }
    raw_from_endpoint(provider, planned, endpoint)
}

fn raw_from_preset(
    preset: &hofund_plugin_registry::CompiledPreset,
    provider: &hofund_config::Provider,
    base_url: &str,
) -> Result<RawTable, DaemonError> {
    let mut raw = RawTable::new();
    for (key, value) in preset.config {
        raw.insert((*key).to_owned(), RawValue::String((*value).to_owned()));
    }
    if !base_url.is_empty() {
        raw.insert("base_url", RawValue::String(base_url.to_owned()));
    }
    insert_options(&mut raw, provider)?;
    Ok(raw)
}

fn raw_from_endpoint(
    provider: &hofund_config::Provider,
    planned: &Planned,
    endpoint: &Endpoint,
) -> Result<RawTable, DaemonError> {
    let mut raw = RawTable::new();
    raw.insert("id", RawValue::String(provider.id.clone()));
    raw.insert("base_url", RawValue::String(endpoint.base_url.clone()));
    if let Some(preset) = &provider.preset {
        raw.insert("preset", RawValue::String(preset.clone()));
    }
    raw.insert("api_key", RawValue::String(endpoint.api_key.clone()));
    raw.insert(
        "credential",
        RawValue::String(credential_name(endpoint.credential).to_owned()),
    );
    if let Some(profile) = &provider.quirks {
        raw.insert("quirks", RawValue::String(profile.clone()));
    }
    if !endpoint.headers.is_empty() {
        let mut headers = RawTable::new();
        for (name, value) in &endpoint.headers {
            headers.insert(name.clone(), RawValue::String(value.clone()));
        }
        raw.insert("headers", RawValue::Table(headers));
    }
    match planned.wire {
        Wire::Chat => raw.insert("protocol", RawValue::String("chat".to_owned())),
        Wire::Responses => raw.insert("protocol", RawValue::String("responses".to_owned())),
        Wire::Messages | Wire::Generative | Wire::Vertex | Wire::Converse => {}
    }
    insert_options(&mut raw, provider)?;
    Ok(raw)
}

const RESERVED_OPTIONS: &[&str] = &[
    "id",
    "base_url",
    "preset",
    "api_key",
    "credential",
    "quirks",
    "headers",
    "protocol",
];

fn insert_options(
    raw: &mut RawTable,
    provider: &hofund_config::Provider,
) -> Result<(), DaemonError> {
    for (key, value) in &provider.options {
        if RESERVED_OPTIONS.contains(&key.as_str()) {
            return Err(DaemonError::new(format!(
                "provider `{}` option `{key}` is reserved",
                provider.id
            )));
        }
        raw.insert(key.clone(), raw_value(value, None)?);
    }
    Ok(())
}

fn resolve_options(
    raw: &mut RawTable,
    provider: &hofund_config::Provider,
) -> Result<(), DaemonError> {
    for (key, value) in &provider.options {
        raw.insert(
            key.clone(),
            raw_value(value, Some((provider.id.as_str(), key.as_str())))?,
        );
    }
    Ok(())
}

fn raw_value(value: &Value, environment: Option<(&str, &str)>) -> Result<RawValue, DaemonError> {
    Ok(match value {
        Value::String(text) => {
            let text = text.trim();
            let resolved = match environment {
                Some((provider, key)) => hofund_secrets::resolve(text).map_err(|error| {
                    DaemonError::new(format!("provider `{provider}` option `{key}`: {error}"))
                })?,
                None => text.to_owned(),
            };
            RawValue::String(resolved)
        }
        Value::Integer(number) => RawValue::Integer(*number),
        Value::Boolean(flag) => RawValue::Boolean(*flag),
        Value::Array(items) => {
            let mut raw_items = Vec::with_capacity(items.len());
            for item in items {
                raw_items.push(raw_value(item, environment)?);
            }
            RawValue::Array(raw_items)
        }
        Value::Table(table) => {
            let mut nested = RawTable::new();
            for (key, item) in table {
                nested.insert(key.clone(), raw_value(item, environment)?);
            }
            RawValue::Table(nested)
        }
    })
}

fn credential_name(credential: Credential) -> &'static str {
    match credential {
        Credential::Default => "default",
        Credential::Bearer => "bearer",
        Credential::ApiKey => "api-key",
        Credential::XApiKey => "x-api-key",
        Credential::MessagesBoth => "messages-both",
        Credential::GoogApiKey => "goog-api-key",
    }
}

fn protocol_of(wire: Wire) -> &'static str {
    match wire {
        Wire::Chat => "openai-chat",
        Wire::Responses => "openai-responses",
        Wire::Messages => "anthropic-messages",
        Wire::Generative | Wire::Vertex => "generative-language",
        Wire::Converse => "bedrock-converse",
    }
}

fn implementation_id(wire: Wire) -> &'static str {
    match wire {
        Wire::Chat | Wire::Responses => "openai-compat",
        Wire::Messages => "anthropic",
        Wire::Generative => "generative",
        Wire::Vertex => "google-vertex",
        Wire::Converse => "bedrock",
    }
}

#[derive(Clone)]
struct Planned {
    base_url: String,
    wire: Wire,
    credential: Credential,
}

fn compiled_credential(
    compiled: &'static hofund_plugin_registry::CompiledPreset,
    region: Option<&str>,
    wire: Wire,
) -> Result<Credential, DaemonError> {
    if !hofund_plugin_registry::builtin_factory_ids().contains(&compiled.plugin) {
        return Ok(Credential::Default);
    }
    let protocol = hofund_plugin_registry::protocol_for(wire)
        .ok_or_else(|| DaemonError::new(format!("preset `{}` cannot use {wire:?}", compiled.id)))?;
    let selection = hofund_plugin_registry::select(
        compiled,
        region,
        &hofund_plugin_registry::builtin_protocols(),
        Some(protocol),
        "",
    )
    .map_err(show)?;
    Ok(hofund_provider::credential_for(
        compiled.id,
        wire,
        &selection.base_url,
    ))
}

fn served_for(plugin: &str, protocols: &[ProtocolId]) -> Result<Vec<ProtocolId>, DaemonError> {
    let Some(protocol) = protocols.first().copied() else {
        return Err(DaemonError::new(format!(
            "plugin `{plugin}` declares no protocol"
        )));
    };
    // Passing the selected protocol through plugin-api is a follow-up that
    // must land before any fragment batch adds a multi-protocol listed plugin.
    Ok(vec![protocol])
}

fn plan(provider: &hofund_config::Provider) -> Result<Planned, DaemonError> {
    plan_with(provider, hofund_plugin_registry::find_preset)
}

fn plan_with(
    provider: &hofund_config::Provider,
    lookup: PresetLookup,
) -> Result<Planned, DaemonError> {
    let selected = match provider.protocol.as_deref() {
        Some(protocol) => Some(selected_protocol(&provider.id, protocol)?),
        None => None,
    };
    if let Some(preset) = &provider.preset
        && let Some((compiled, alias_region)) = lookup(preset)
    {
        let region = option_present(provider, "region")?.or(alias_region);
        let builtin = hofund_plugin_registry::builtin_factory_ids().contains(&compiled.plugin);
        let served = if builtin {
            hofund_plugin_registry::builtin_protocols()
        } else {
            let factory = hofund_plugin_registry::factory(compiled.plugin).ok_or_else(|| {
                DaemonError::new(format!(
                    "preset plugin `{}` has no factory",
                    compiled.plugin
                ))
            })?;
            served_for(
                compiled.plugin,
                DynUpstreamFactory::spec(factory.as_ref()).protocols(),
            )?
        };
        if !builtin
            && selected.is_some_and(|(protocol, _)| Some(protocol) != served.first().copied())
        {
            return Err(DaemonError::new(format!(
                "provider `{}` protocol does not match plugin `{}`",
                provider.id, compiled.plugin
            )));
        }
        let selection = hofund_plugin_registry::select(
            compiled,
            region,
            &served,
            selected.map(|(protocol, _)| protocol),
            &provider.base_url,
        )
        .map_err(show)?;
        // Same as `resolve`: the credential follows the final base, so a
        // `base_url` override applies its host rules.
        let credential = if builtin {
            hofund_provider::credential_for(compiled.id, selection.wire, &selection.base_url)
        } else {
            Credential::Default
        };
        return Ok(Planned {
            base_url: selection.base_url,
            wire: selection.wire,
            credential,
        });
    }
    if let Some(preset) = &provider.preset {
        let resolved = resolve(&hofund_provider::Resolve {
            id: preset,
            region: option_present(provider, "region")?,
            wire: selected.map(|(_, wire)| wire),
            base_url: &provider.base_url,
            project: option(provider, "project")?,
            location: option(provider, "location")?,
        })
        .map_err(show)?;
        return Ok(Planned {
            base_url: resolved.base_url,
            wire: resolved.wire,
            credential: resolved.credential,
        });
    }
    let wire = selected.map(|(_, wire)| wire).ok_or_else(|| {
        DaemonError::new(format!(
            "provider `{}` needs a protocol or a preset",
            provider.id
        ))
    })?;
    Ok(Planned {
        base_url: provider.base_url.clone(),
        wire,
        credential: Credential::Default,
    })
}

/// String options `plan` still reads while it builds a catalog URL.
///
/// A missing key is an empty string. Any other shape is an error that names
/// the provider and the key. The factory repeats this check for its schema.
fn option<'a>(provider: &'a hofund_config::Provider, key: &str) -> Result<&'a str, DaemonError> {
    match provider.options.get(key) {
        None => Ok(""),
        Some(Value::String(value)) => Ok(value.as_str()),
        Some(_) => Err(DaemonError::new(format!(
            "provider `{}` option `{key}` must be a string",
            provider.id
        ))),
    }
}

fn option_present<'a>(
    provider: &'a hofund_config::Provider,
    key: &str,
) -> Result<Option<&'a str>, DaemonError> {
    match option(provider, key)? {
        "" => Ok(None),
        value => Ok(Some(value)),
    }
}

fn selected_protocol(provider_id: &str, protocol: &str) -> Result<(ProtocolId, Wire), DaemonError> {
    let id = ProtocolId::try_from(protocol).map_err(|_| unknown_protocol(provider_id, protocol))?;
    let wire = hofund_plugin_registry::wire_for_protocol(id)
        .ok_or_else(|| unknown_protocol(provider_id, protocol))?;
    Ok((id, wire))
}

fn unknown_protocol(provider_id: &str, protocol: &str) -> DaemonError {
    DaemonError::new(format!(
        "provider `{provider_id}` has unknown protocol id `{protocol}`; expected {}",
        expected_protocols()
    ))
}

fn expected_protocols() -> String {
    let ids: Vec<&str> = ProtocolId::ALL.iter().map(|id| id.as_str()).collect();
    let Some((last, rest)) = ids.split_last() else {
        return String::new();
    };
    if rest.is_empty() {
        return (*last).to_owned();
    }
    format!("{}, or {last}", rest.join(", "))
}

fn warn(config: &Config) {
    for warning in &config.warnings {
        eprintln!("warning: {warning}");
    }
}

fn inbound_codecs() -> HashMap<String, Arc<dyn Codec>> {
    let mut codecs = HashMap::new();
    codecs.insert(
        "/v1/messages".to_owned(),
        Arc::new(AnthropicMessages) as Arc<dyn Codec>,
    );
    codecs.insert("/v1/chat/completions".to_owned(), Arc::new(OpenAiChat));
    codecs.insert("/v1/responses".to_owned(), Arc::new(OpenAiResponses));
    codecs.insert(GENERATIVE_UNARY.to_owned(), Arc::new(GenerativeLanguage));
    codecs.insert(GENERATIVE_STREAM.to_owned(), Arc::new(GenerativeLanguage));
    codecs
}

fn beside(config_path: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(path)
    }
}

fn show<E: std::fmt::Display>(error: E) -> DaemonError {
    DaemonError::new(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::check;

    #[test]
    fn the_raw_endpoint_table_carries_the_preset_id() {
        let config = hofund_config::load(
            "[[providers]]\nid = \"mantle\"\npreset = \"bedrock-mantle\"\nprotocol = \"openai-chat\"\nbase_url = \"https://relay.example/v1\"\napi_key = \"test-key\"\n",
        )
        .expect("config");
        let provider = &config.providers[0];
        let planned = super::plan(provider).expect("plan");
        let endpoint = super::endpoint_for(provider, &planned).expect("endpoint");
        assert_eq!(endpoint.preset.as_deref(), Some("bedrock-mantle"));
        let raw = super::raw_from_endpoint(provider, &planned, &endpoint).expect("raw");
        assert_eq!(
            raw.get("preset"),
            Some(&hofund_plugin_api::RawValue::String(
                "bedrock-mantle".to_owned()
            ))
        );
    }

    #[test]
    fn a_compiled_preset_accepts_a_later_factory_protocol() {
        let config = hofund_config::load(
            "[[providers]]\nid = \"multi\"\npreset = \"fixture-multi\"\nprotocol = \"openai-responses\"\napi_key = \"test-key\"\n",
        )
        .expect("config");
        let provider = &config.providers[0];
        let planned = super::plan_with(provider, hofund_plugin_registry::find_fixture_preset)
            .expect("responses plan");
        assert_eq!(planned.wire, hofund_provider::Wire::Responses);
        assert_eq!(planned.base_url, "https://first.example.test/v1");
        assert_eq!(planned.credential, hofund_provider::Credential::Bearer);
        let endpoint = super::endpoint_for(provider, &planned).expect("endpoint");
        let raw = super::raw_from_endpoint(provider, &planned, &endpoint).expect("raw");
        assert_eq!(
            raw.get("protocol"),
            Some(&hofund_plugin_api::RawValue::String("responses".to_owned()))
        );
    }

    #[test]
    fn builtin_owned_presets_build_from_the_endpoint_table() {
        let config = hofund_config::load(
            "[[providers]]\nid = \"fixture\"\npreset = \"fixture-equivalence\"\napi_key = \"test-key\"\n",
        )
        .expect("config");
        let provider = &config.providers[0];
        let planned =
            super::plan_with(provider, hofund_plugin_registry::find_fixture_preset).expect("plan");
        let endpoint = super::endpoint_for(provider, &planned).expect("endpoint");
        let raw = super::raw_for_with(
            provider,
            &planned,
            &endpoint,
            hofund_plugin_registry::find_fixture_preset,
        )
        .expect("raw");
        assert_eq!(
            raw.get("protocol"),
            Some(&hofund_plugin_api::RawValue::String("chat".to_owned()))
        );
        assert_eq!(
            raw.get("preset"),
            Some(&hofund_plugin_api::RawValue::String(
                "fixture-equivalence".to_owned()
            ))
        );
    }

    #[test]
    fn builtin_owned_presets_match_the_catalog_on_every_selected_wire() {
        for (settings, wire, base_url, plugin) in [
            (
                "protocol = \"anthropic-messages\"\n",
                hofund_provider::Wire::Messages,
                "https://api.lkeap.cloud.tencent.com/plan/anthropic",
                "anthropic",
            ),
            (
                "protocol = \"anthropic-messages\"\nbase_url = \"https://api.anthropic.com\"\n",
                hofund_provider::Wire::Messages,
                "https://api.anthropic.com",
                "anthropic",
            ),
            (
                "[providers.options]\nregion = \"cn\"\n",
                hofund_provider::Wire::Chat,
                "https://tokenhub.tencentmaas.com/v1",
                "openai-compat",
            ),
        ] {
            let fixture = hofund_config::load(&format!(
                "[[providers]]\nid = \"fixture\"\npreset = \"fixture-equivalence\"\napi_key = \"test-key\"\n{settings}"
            ))
            .expect("fixture config");
            let provider = &fixture.providers[0];
            let planned = super::plan_with(provider, hofund_plugin_registry::find_fixture_preset)
                .expect("fixture plan");
            assert_eq!(planned.wire, wire);
            assert_eq!(planned.base_url, base_url);
            assert_eq!(
                super::plugin_for_with(provider, wire, hofund_plugin_registry::find_fixture_preset),
                plugin
            );

            let catalog = hofund_config::load(&format!(
                "[[providers]]\nid = \"catalog\"\npreset = \"tencent-cloud\"\napi_key = \"test-key\"\n{settings}"
            ))
            .expect("catalog config");
            let expected = super::plan(&catalog.providers[0]).expect("catalog plan");
            assert_eq!(planned.base_url, expected.base_url);
            assert_eq!(planned.wire, expected.wire);
            assert_eq!(planned.credential, expected.credential);
        }
    }

    #[test]
    fn production_plan_does_not_resolve_fixture_presets() {
        let config = hofund_config::load(
            "[[providers]]\nid = \"fixture\"\npreset = \"fixture-equivalence\"\napi_key = \"test-key\"\n",
        )
        .expect("config");
        let Err(error) = super::plan(&config.providers[0]) else {
            panic!("production plan resolved a fixture preset");
        };
        assert_eq!(error.to_string(), "unknown preset `fixture-equivalence`");
    }

    #[test]
    fn non_builtin_plugins_use_only_their_first_protocol() {
        let served = super::served_for(
            "multi",
            &[
                hofund_plugin_api::ProtocolId::OPENAI_CHAT,
                hofund_plugin_api::ProtocolId::OPENAI_RESPONSES,
            ],
        )
        .expect("served protocol");
        assert_eq!(served, [hofund_plugin_api::ProtocolId::OPENAI_CHAT]);
        let error = super::served_for("empty", &[]).expect_err("missing protocol");
        assert_eq!(error.to_string(), "plugin `empty` declares no protocol");

        let config = hofund_config::load(
            "[[providers]]\nid = \"relay\"\npreset = \"relay\"\nprotocol = \"openai-responses\"\nbase_url = \"https://relay.example.test/v1\"\n",
        )
        .expect("config");
        let Err(error) = super::plan(&config.providers[0]) else {
            panic!("protocol mismatch was accepted");
        };
        assert_eq!(
            error.to_string(),
            "provider `relay` protocol does not match plugin `relay`"
        );
    }

    #[test]
    fn a_preset_builds_and_an_unfinished_adapter_does_not() {
        let dir = std::env::temp_dir().join(format!("hofund-preset-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("hofund.toml");
        std::fs::write(
            &path,
            "[[providers]]\nid = \"openai\"\npreset = \"openai\"\napi_key = \"test\"\n",
        )
        .expect("write");
        check(&path).expect("openai");
        std::fs::write(
            &path,
            "[[providers]]\nid = \"local\"\npreset = \"ollama\"\n",
        )
        .expect("write");
        check(&path).expect("ollama");
        std::fs::write(
            &path,
            "[[providers]]\nid = \"azure\"\npreset = \"azure\"\nbase_url = \"my-resource\"\napi_key = \"test\"\n",
        )
        .expect("write");
        check(&path).expect("azure");
        std::fs::write(
            &path,
            "[[providers]]\nid = \"gemini\"\npreset = \"google\"\napi_key = \"test\"\n",
        )
        .expect("write");
        check(&path).expect("google");
        std::fs::write(
            &path,
            "[[providers]]\nid = \"gemini\"\npreset = \"google\"\nprotocol = \"openai-chat\"\napi_key = \"test\"\n",
        )
        .expect("write");
        check(&path).expect("google chat");
        std::fs::write(
            &path,
            "[[providers]]\nid = \"vertex\"\npreset = \"google-vertex\"\n",
        )
        .expect("write");
        let error = check(&path).expect_err("vertex");
        assert!(
            error.to_string().contains("Google Cloud project"),
            "{error}"
        );
        std::fs::write(
            &path,
            "[[providers]]\nid = \"vertex\"\npreset = \"google-vertex\"\nproject = \"my-project\"\n",
        )
        .expect("write");
        check(&path).expect("vertex project");
        std::fs::write(
            &path,
            "[[providers]]\nid = \"vertex\"\npreset = \"google-vertex\"\nproject = \"my-project\"\ncredentials = \"relative.json\"\n",
        )
        .expect("write");
        let error = check(&path).expect_err("relative credentials");
        assert!(error.to_string().contains("full path"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_registry_preset_builds_through_the_factory() {
        let dir = std::env::temp_dir().join(format!("hofund-relay-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("hofund.toml");
        std::fs::write(
            &path,
            "[[providers]]\nid = \"box\"\npreset = \"relay\"\napi_key = \"test\"\n",
        )
        .expect("write");
        let error = check(&path).expect_err("missing base url");
        assert!(error.to_string().contains("base_url"), "{error}");
        std::fs::write(
            &path,
            "[[providers]]\nid = \"box\"\npreset = \"relay\"\nbase_url = \"http://127.0.0.1:9\"\napi_key = \"test\"\n",
        )
        .expect("write");
        check(&path).expect("relay");
        std::fs::write(
            &path,
            "connect_timeout_ms = 1500\nread_timeout_ms = 2500\n[[providers]]\nid = \"box\"\npreset = \"relay\"\nbase_url = \"http://127.0.0.1:9\"\napi_key = \"test\"\n",
        )
        .expect("write");
        check(&path).expect("pooled timeouts");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unknown_protocol_names_the_provider_and_the_codec_ids() {
        let dir = std::env::temp_dir().join(format!("hofund-protocol-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("hofund.toml");
        std::fs::write(
            &path,
            "[[providers]]\nid = \"primary\"\nprotocol = \"nope\"\nbase_url = \"http://127.0.0.1:9\"\n",
        )
        .expect("write");
        let error = check(&path).expect_err("protocol");
        let text = error.to_string();
        assert!(text.contains("provider `primary`"), "{text}");
        assert!(text.contains("unknown protocol id `nope`"), "{text}");
        assert!(text.contains("openai-chat, openai-responses"), "{text}");
        assert!(text.contains("or bedrock-converse"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_non_string_option_names_the_provider_and_the_key() {
        let dir = std::env::temp_dir().join(format!("hofund-option-type-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("hofund.toml");
        std::fs::write(
            &path,
            "[[providers]]\nid = \"vertex\"\npreset = \"google-vertex\"\n[providers.options]\nproject = 1\n",
        )
        .expect("write");
        let error = check(&path).expect_err("project type");
        assert!(
            error
                .to_string()
                .contains("provider `vertex` option `project` must be a string"),
            "{error}"
        );
        std::fs::write(
            &path,
            "[[providers]]\nid = \"primary\"\npreset = \"openai\"\napi_key = \"test\"\n[providers.options]\nsecret_access_key = \"SENTINEL-OPTION-VALUE\"\n",
        )
        .expect("write");
        let error = check(&path).expect_err("aws key on openai");
        let text = error.to_string();
        assert!(
            text.contains("provider `primary` has unknown option `secret_access_key`"),
            "{text}"
        );
        assert!(!text.contains("SENTINEL-OPTION-VALUE"), "{text}");
        std::fs::write(
            &path,
            "[[providers]]\nid = \"primary\"\npreset = \"openai\"\napi_key = \"test\"\n[providers.options]\nunknown_key = \"env:HOFUND_UNKNOWN_OPTION_UNSET_692A\"\n",
        )
        .expect("write");
        let error = check(&path).expect_err("unknown option before env resolution");
        let text = error.to_string();
        assert!(
            text.contains("provider `primary` has unknown option `unknown_key`"),
            "{text}"
        );
        assert!(!text.contains("environment variable"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn aws_key_specs_are_trimmed_before_lookup() {
        let dir = std::env::temp_dir().join(format!("hofund-aws-trim-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("hofund.toml");
        let base = "protocol = \"bedrock-converse\"\nbase_url = \"https://bedrock-runtime.us-east-1.amazonaws.com\"\nregion = \"us-east-1\"\n";
        std::fs::write(
            &path,
            format!(
                "[[providers]]\nid = \"aws\"\n{base}access_key_id = \" env:HOFUND_CONFIG_AWS_KEY_UNSET_692A\"\nsecret_access_key = \"secret\"\n"
            ),
        )
        .expect("write");
        let error = check(&path).expect_err("access key");
        let text = error.to_string();
        assert!(
            text.contains("provider `aws` option `access_key_id`"),
            "{text}"
        );
        assert!(
            text.contains("environment variable `HOFUND_CONFIG_AWS_KEY_UNSET_692A` is unset"),
            "{text}"
        );
        std::fs::write(
            &path,
            format!(
                "[[providers]]\nid = \"aws\"\n{base}access_key_id = \"AKIDEXAMPLE\"\nsecret_access_key = \" env:HOFUND_CONFIG_AWS_SECRET_UNSET_692A\"\n"
            ),
        )
        .expect("write");
        let error = check(&path).expect_err("secret key");
        assert!(
            error
                .to_string()
                .contains("environment variable `HOFUND_CONFIG_AWS_SECRET_UNSET_692A` is unset"),
            "{error}"
        );
        std::fs::write(
            &path,
            format!(
                "[[providers]]\nid = \"aws\"\n{base}access_key_id = \"AKIDEXAMPLE\"\nsecret_access_key = \"secret\"\nsession_token = \" env:HOFUND_CONFIG_AWS_TOKEN_UNSET_692A\"\n"
            ),
        )
        .expect("write");
        let error = check(&path).expect_err("session token");
        assert!(
            error
                .to_string()
                .contains("environment variable `HOFUND_CONFIG_AWS_TOKEN_UNSET_692A` is unset"),
            "{error}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_v0_1_0_example_checks_with_warnings_only() {
        let source = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../config/fixtures/v0.1.0-hofund.toml");
        let dir = std::env::temp_dir().join(format!("hofund-v010-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("hofund.toml");
        std::fs::copy(&source, &path).expect("copy fixture");
        check(&path).expect("v0.1.0");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn echo_is_mounted_behind_auth() {
        let dir = std::env::temp_dir().join(format!("hofund-echo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("hofund.toml");
        std::fs::write(&path, "listen = \"127.0.0.1:0\"\n").expect("write");
        let running = super::start(&path).await.expect("start");
        let token = std::fs::read_to_string(dir.join("hofund.token")).expect("token");
        let url = format!("http://{}/hofund/echo", running.local_addr);
        let client = reqwest::Client::new();
        let denied = client.get(&url).send().await.expect("denied");
        assert_eq!(denied.status(), reqwest::StatusCode::UNAUTHORIZED);
        let ok = client
            .get(&url)
            .bearer_auth(token.trim())
            .send()
            .await
            .expect("echo");
        assert_eq!(ok.status(), reqwest::StatusCode::OK);
        assert_eq!(ok.text().await.expect("body"), "echo");
        let nested = client
            .get(format!("{url}/nested"))
            .bearer_auth(token.trim())
            .send()
            .await
            .expect("nested");
        assert_eq!(nested.status(), reqwest::StatusCode::OK);
        let missed = client
            .post(&url)
            .bearer_auth(token.trim())
            .send()
            .await
            .expect("post");
        assert_eq!(missed.status(), reqwest::StatusCode::NOT_FOUND);
        running.shutdown().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
