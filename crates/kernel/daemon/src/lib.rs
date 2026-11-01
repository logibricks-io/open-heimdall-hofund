//! Composition root for the local gateway process.
//!
//! This crate reads configuration, resolves credentials, builds providers, and
//! starts the HTTP server. The binary parses arguments and calls these
//! functions.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use hofund_codec::{AnthropicMessages, Codec, GenerativeLanguage, OpenAiChat, OpenAiResponses};
use hofund_config::{Config, ConfigError, Fallback, UpstreamTarget, Value, load_path};
use hofund_gateway::{GENERATIVE_STREAM, GENERATIVE_UNARY, Gateway};
use hofund_plugin_api::{
    AgentDescription, BuildContext, DynUpstreamFactory, PluginId, ProtocolId, RawTable, RawValue,
    UpstreamProvider,
};
use hofund_plugin_host::{HttpBackend, HttpLimits, HttpProfile, PooledHttp};

#[doc(hidden)]
pub use hofund_gateway::BindHook;
pub use hofund_gateway::Running;
use hofund_provider::{Credential, Endpoint, Provider, Wire};
use hofund_quirks::named;
use hofund_router::{Cooldowns, FailoverPolicy, Snapshot, Unit};
use hofund_secrets::{KeyStore, TokenFile};
use hofund_usage::Ledger;

mod adapt;
mod config_ops;
mod config_write;
mod control;
mod control_agents;
mod control_local;
pub(crate) mod control_map;
mod events;
mod measurements;
mod membership;
mod models;
mod models_state;
mod offline;
mod one_shot;
mod plans;
mod probe;
mod provider_keys;
mod quotas;
mod registry;
mod rests;
mod runtime;
mod sign_in;
mod subscriptions;
mod terminate;
mod watch;

pub use models::{
    Cached, ModelLimits, ModelsHost, Picks, ProbeOutcome, ProviderModels, RefreshTiming,
};
pub use registry::{CompiledPreset, DaemonRegistry, RegistryAdapter};
pub use sign_in::{
    AccountListing, INTERRUPTED_EXIT, QuotaLine, QuotaListing, SIGN_IN_DEADLINE, SignInError,
    SignInOptions, SubscriptionListing, quota_json, subscription_list, subscription_quota,
    subscription_refresh, subscription_sign_in, subscription_sign_out, subscriptions_json,
};
pub use terminate::{ConsoleEvent, Termination, TolerantWriter, finish, serve_until_signal};

/// A configuration, credential, or startup failure.
#[derive(Debug)]
pub struct DaemonError {
    message: String,
    retry_after: Option<std::time::Duration>,
    kind: ErrorKind,
}

/// What a [`DaemonError`] means to the control plane.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ErrorKind {
    /// The named provider is not configured.
    NotFound,
    /// No model could be chosen from the request and the provider's list.
    Selection,
    /// The provider answered with an error, or could not be reached.
    Upstream {
        kind: hofund_provider::ProviderErrorKind,
        status: Option<u16>,
    },
    /// The control route ran past its deadline.
    Timeout,
    /// Anything else.
    #[default]
    Internal,
}

impl DaemonError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retry_after: None,
            kind: ErrorKind::Internal,
        }
    }

    const fn with_kind(mut self, kind: ErrorKind) -> Self {
        self.kind = kind;
        self
    }

    const fn kind(&self) -> ErrorKind {
        self.kind
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
    let registry: Arc<dyn DaemonRegistry> = Arc::new(RegistryAdapter);
    let _ = providers(
        &config,
        path,
        None,
        Mode::Check,
        None,
        None,
        None,
        &registry,
    )?;
    let _ = subscriptions::build(&config, path, None, &registry, None)?;
    Ok(())
}

/// Reads the bearer token, creating the file on first use.
///
/// # Errors
///
/// Returns [`DaemonError`] when the config or the token file fails.
pub fn token_show(path: &Path) -> Result<String, DaemonError> {
    let store = KeyStore::beside_token(token_file(path)?.path());
    store.list().map_err(show)?;
    store.token_file().load_or_create().map_err(show)
}

/// Replaces the bearer token and returns the new value.
///
/// # Errors
///
/// Returns [`DaemonError`] when the config or the token file fails.
pub fn token_rotate(path: &Path) -> Result<String, DaemonError> {
    KeyStore::beside_token(token_file(path)?.path())
        .rotate_default()
        .map(|rotated| rotated.secret)
        .map_err(show)
}

/// The listen URL and bearer token an agent adapter should write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalGateway {
    /// `http://` plus the model listen address: `HOFUND_ADDR` when set,
    /// otherwise the config's `listen`. IPv6 addresses keep their brackets, as
    /// in `http://[::1]:3425`.
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
    let store = KeyStore::beside_token(beside(path, &config.token_file));
    store.list().map_err(show)?;
    let token = store.token_file().load_or_create().map_err(show)?;
    let (listen, _) =
        hofund_config::effective_listen(&config, hofund_config::addr_env().as_deref())
            .map_err(show)?;
    Ok(LocalGateway {
        base_url: format!("http://{listen}"),
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
    RegistryAdapter.agents()
}

/// The session agent work in this process uses: the process environment and
/// the OS credential store, for agents that read hofund's key there.
#[must_use]
pub fn agent_session() -> hofund_agent::Session {
    hofund_agent::Session::from_process_env()
        .with_credential_store(hofund_plugin_host::native_credential_store())
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

pub use control_local::ControlRuntime;
pub use one_shot::{one_shot_stdio, serve_one_shot};

/// Optional listeners for a daemon start.
#[derive(Clone, Debug, Default)]
pub struct StartOptions {
    /// Numeric loopback address for the opt-in control listener.
    pub control_listen: Option<SocketAddr>,
    /// Enables local control even without a configured TCP address.
    pub control_enabled: bool,
    /// Isolated runtime roots, primarily for tests. `None` reads the environment.
    #[doc(hidden)]
    pub control_runtime: Option<ControlRuntime>,
    /// Home directory and environment for agent control calls, for tests.
    /// `None` reads the process environment on each call.
    #[doc(hidden)]
    pub agent_session: Option<hofund_agent::Session>,
    /// Receives one increment per successful control token rotation, for
    /// tests and the event stream. `None` uses a private channel.
    #[doc(hidden)]
    pub token_generation: Option<tokio::sync::watch::Sender<u64>>,
    /// Leaves hand edits on disk unpublished until an explicit reload, for
    /// tests that inspect an edit the daemon has not loaded. The file watcher
    /// runs whenever local control is enabled and this is `false`.
    #[doc(hidden)]
    pub skip_config_watch: bool,
    /// The `HOFUND_ADDR` value to use instead of the process environment, for
    /// tests. `None` reads the environment.
    #[doc(hidden)]
    pub hofund_addr: Option<String>,
    /// Runs before each relisten bind, for tests that need a bind to fail.
    #[doc(hidden)]
    pub relisten_hook: Option<hofund_gateway::BindHook>,
}

/// Binds the model address and serves until shutdown.
///
/// The address is `HOFUND_ADDR` when it is set and not empty, otherwise the
/// config's `listen`. The bound address is available on the returned [`Running`] value. This
/// entry point does not start a control listener.
///
/// # Errors
///
/// Returns [`DaemonError`] when startup fails. A later accept error stays on
/// the server task.
pub async fn start(path: &Path) -> Result<Running, DaemonError> {
    start_inner(
        path,
        None,
        None,
        Arc::new(RegistryAdapter),
        StartOptions::default(),
    )
    .await
}

/// Starts a daemon with opt-in local control listeners.
///
/// A configured control address enables TCP and, on Unix, the per-config
/// socket. `control_enabled` without an address prefers the socket and uses
/// default loopback TCP only when local IPC is unavailable. No configuration
/// field is introduced.
///
/// # Errors
///
/// Returns [`DaemonError`] when startup, locking, or a listener bind fails.
pub async fn start_with_options(
    path: &Path,
    options: StartOptions,
) -> Result<Running, DaemonError> {
    start_inner(path, None, None, Arc::new(RegistryAdapter), options).await
}

/// Starts a daemon with the caller's registry and listeners.
///
/// `hofundd` uses this entry point with the registry it links, so the
/// production registry link lives in that binary. The other start functions
/// use the compatibility [`RegistryAdapter`] until the desktop rework.
///
/// # Errors
///
/// Returns [`DaemonError`] when startup, locking, or a listener bind fails.
pub async fn start_linked(
    path: &Path,
    registry: Arc<dyn DaemonRegistry>,
    options: StartOptions,
) -> Result<Running, DaemonError> {
    start_inner(path, None, None, registry, options).await
}

/// Starts a daemon with an injected registry.
///
/// # Errors
///
/// Returns [`DaemonError`] when startup fails.
#[cfg(feature = "test-util")]
#[doc(hidden)]
pub async fn start_with_registry(
    path: &Path,
    registry: Arc<dyn DaemonRegistry>,
) -> Result<Running, DaemonError> {
    start_inner(path, Some(0), None, registry, StartOptions::default()).await
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
    start_inner(
        path,
        Some(0),
        None,
        Arc::new(RegistryAdapter),
        StartOptions::default(),
    )
    .await
}

/// Starts a test daemon with short refresh-loop timings.
#[cfg(feature = "test-util")]
#[doc(hidden)]
pub async fn start_for_test_with_refresh(
    path: &Path,
    timing: RefreshTiming,
) -> Result<Running, DaemonError> {
    start_inner(
        path,
        Some(0),
        Some(timing),
        Arc::new(RegistryAdapter),
        StartOptions::default(),
    )
    .await
}

async fn start_inner(
    path: &Path,
    max_idle_per_host: Option<usize>,
    refresh_timing: Option<RefreshTiming>,
    registry: Arc<dyn DaemonRegistry>,
    options: StartOptions,
) -> Result<Running, DaemonError> {
    let started_at_ms = control::unix_milliseconds();
    let config_text = std::fs::read_to_string(path)
        .map_err(|error| show(ConfigError::Read(error.to_string())))?;
    let config = hofund_config::load(&config_text).map_err(show)?;
    let runtime_revision = control::revision(config_text.as_bytes());
    let control_runtime = options
        .control_runtime
        .unwrap_or_else(ControlRuntime::from_env);
    let mut control_session = if options.control_enabled || options.control_listen.is_some() {
        let identity = control_local::ConfigIdentity::resolve(path).map_err(DaemonError::new)?;
        Some(
            control_local::ControlSession::acquire(identity, &control_runtime)
                .map_err(DaemonError::new)?,
        )
    } else {
        None
    };
    warn(&config);
    let token_path = beside(path, &config.token_file);
    let key_store = Arc::new(KeyStore::beside_token(&token_path));
    key_store.list().map_err(show)?;
    let token = TokenFile::new(&token_path).load_or_create().map_err(show)?;
    let usage = Arc::new(Ledger::open(&beside(path, &config.usage_file)).map_err(show)?);
    let runtime = runtime::Runtime::new(Arc::clone(&registry), path, max_idle_per_host);
    let mut models_host = ModelsHost::new(config.connect_timeout, config.read_timeout)
        .with_state_root(beside(path, ".hofund"))
        .with_registry(Arc::clone(&registry));
    let snapshot = runtime.initial_snapshot(&config, Some(&mut models_host))?;
    // Subscription members join before the port opens.
    runtime.refresh_subscriptions().await;
    let cooldowns = snapshot
        .load()
        .cooldowns()
        .cloned()
        .expect("daemon router has shared cooldowns");
    // Create the event bus and in-flight counter before bind so the gateway
    // can emit request summaries for the entire listen lifetime.
    let control_bus = control_session
        .as_ref()
        .map(|_| Arc::new(events::EventBus::new()));
    let in_flight = Arc::new(AtomicU64::new(0));
    let mut gateway = Gateway::with_token_file(
        snapshot,
        token.clone(),
        token_path.clone(),
        inbound_codecs(),
        Arc::clone(&usage),
    )
    .with_key_store(Arc::clone(&key_store))
    .with_surfaces(registry.surfaces())
    .with_surface_http(surface_http(&config, max_idle_per_host));
    let quotas = Arc::new(quotas::DaemonQuotas::new(
        Arc::clone(&runtime),
        Arc::clone(&usage),
    ));
    gateway = gateway.with_quotas(Arc::clone(&quotas) as Arc<dyn hofund_gateway::QuotaSource>);
    if let Some(bus) = &control_bus {
        gateway = gateway.with_request_events(
            Arc::new(events::RequestBridge::new(Arc::clone(bus))),
            Arc::clone(&in_flight),
        );
    }
    let env = match options.hofund_addr {
        Some(value) => Some(value),
        None => hofund_config::addr_env(),
    };
    let (listen, source) =
        hofund_config::effective_listen(&config, env.as_deref()).map_err(show)?;
    let mut running = gateway
        .bind(listen)
        .await
        .map_err(|error| DaemonError::new(error.to_string()))?;
    if let Some(hook) = options.relisten_hook {
        running = running.with_bind_hook(hook);
    }
    if let Some(bus) = &control_bus {
        models_host.attach_events(Arc::clone(bus));
    }
    let models_host = Arc::new(models_host);
    runtime.attach_measurements(Arc::clone(&usage), Arc::clone(&models_host));
    runtime.attach_models(Arc::clone(&models_host));
    if let Some(bus) = &control_bus {
        runtime.attach_events(Arc::clone(bus));
    }
    let config = Arc::new(config);
    if let Some(session) = control_session.as_mut() {
        let shutdown_request = tokio::sync::watch::channel(false).0;
        running = running.with_shutdown_request(shutdown_request.clone());
        let token_generation = options
            .token_generation
            .unwrap_or_else(|| tokio::sync::watch::channel(0).0);
        let events = control_bus.expect("control bus created with control session");
        events::spawn_lifecycle(
            Arc::clone(&events),
            token_generation.subscribe(),
            shutdown_request.subscribe(),
        );
        let (fatal, fatal_reported) = tokio::sync::watch::channel(None);
        running = running.with_fatal(fatal_reported);
        let listener = config_ops::ModelListener::new(
            (source == hofund_config::ListenSource::Environment).then_some(listen),
            running.relisten_handle(),
            fatal,
        );
        let ops = Arc::new(config_ops::ConfigOps::new(
            path,
            session.config_lock_fallback(&control_runtime),
            Arc::clone(&runtime),
            Arc::clone(&events),
            Arc::clone(&config),
            runtime_revision,
            listener,
        ));
        if !options.skip_config_watch {
            running = running.with_task(watch::spawn(Arc::clone(&ops), path.to_path_buf()));
        }
        let state = Arc::new(
            control::DaemonControl::new(
                path,
                started_at_ms,
                Arc::clone(&models_host),
                ops,
                Arc::clone(&registry),
                cooldowns,
                Arc::clone(&usage),
                Arc::clone(&key_store),
                token_path.clone(),
                shutdown_request,
            )
            .with_agent_hooks(options.agent_session, Some(token_generation))
            .with_quotas(Arc::clone(&quotas))
            .with_events(Arc::clone(&events))
            .with_in_flight(Arc::clone(&in_flight)),
        );
        let control = hofund_gateway::control::Control::with_token_file(state, token, token_path);
        let mut tcp_listen = options.control_listen;
        #[cfg(unix)]
        let mut socket_path = None;
        #[cfg(not(unix))]
        let socket_path = None;
        #[cfg(unix)]
        {
            if let Ok(candidate) = session.socket_path(&control_runtime) {
                session
                    .prepare_socket(&candidate)
                    .map_err(DaemonError::new)?;
                match control.clone().bind_unix(&candidate).await {
                    Ok(unix) => {
                        running = running.with_unix_control(unix, candidate.clone());
                        socket_path = Some(candidate);
                    }
                    Err(
                        error @ (hofund_gateway::GatewayError::SocketPathTooLong { .. }
                        | hofund_gateway::GatewayError::BindUnix { .. }),
                    ) => {
                        tracing::warn!(
                            path = %candidate.display(),
                            %error,
                            "Unix control socket unavailable; falling back to TCP"
                        );
                        if tcp_listen.is_none() {
                            tcp_listen = Some(hofund_gateway::control::DEFAULT_CONTROL_ADDR);
                        }
                    }
                    Err(error) => return Err(DaemonError::new(error.to_string())),
                }
            } else if tcp_listen.is_none() {
                tcp_listen = Some(hofund_gateway::control::DEFAULT_CONTROL_ADDR);
            }
        }
        #[cfg(not(unix))]
        if tcp_listen.is_none() {
            tcp_listen = Some(hofund_gateway::control::DEFAULT_CONTROL_ADDR);
        }
        if let Some(listen) = tcp_listen {
            let tcp = control
                .bind(listen)
                .await
                .map_err(|error| DaemonError::new(error.to_string()))?;
            running = running.with_control(tcp);
        }
        session
            .publish(
                running.control_local_addr().into_iter().collect(),
                socket_path,
            )
            .map_err(DaemonError::new)?;
    }
    if let Some(session) = control_session {
        running = running.with_resource(session);
    }
    let refresh = models_host.spawn_refresh(
        runtime.config_updates(),
        path.to_path_buf(),
        refresh_timing.unwrap_or_default(),
    );
    let maintenance = runtime.spawn_maintenance();
    let subscriptions = runtime.spawn_subscriptions();
    let probes = runtime.spawn_probes();
    let running = running.with_resource(AffinityFlush(runtime.affinity()));
    Ok(running
        .with_task(refresh)
        .with_task(maintenance)
        .with_task(subscriptions)
        .with_task(probes))
}

struct AffinityFlush(hofund_router::affinity::AffinityState);

impl Drop for AffinityFlush {
    fn drop(&mut self) {
        self.0.flush();
    }
}

/// Serves until a termination signal or a control shutdown, then drains.
///
/// See [`serve_until_signal`] for the signals. A signal or control shutdown
/// returns `Ok`; process-level exit handling is left to the caller.
///
/// # Errors
///
/// Returns [`DaemonError`] when startup fails or a fatal error stopped the
/// daemon.
pub async fn serve(path: &Path) -> Result<(), DaemonError> {
    match serve_reason(path).await? {
        Termination::Fatal(message) => Err(DaemonError::new(message)),
        _ => Ok(()),
    }
}

async fn serve_reason(path: &Path) -> Result<Termination, DaemonError> {
    let running = start(path).await?;
    terminate::notice(&format!("listening on http://{}", running.local_addr));
    Ok(serve_until_signal(running).await)
}

/// Waits for Ctrl-C or a control `POST /v1/hofund/shutdown`, then drains.
///
/// Linked control listeners stop first, the model listener then finishes its
/// in-flight requests, and daemon resources such as the control lock and
/// socket are released when the server is dropped.
///
/// # Errors
///
/// Returns [`DaemonError`] after a fatal error stopped the daemon, such as a
/// relisten that could not restore the old model listener. The process should
/// then exit non-zero so its supervisor restarts it.
pub async fn serve_until_shutdown(mut running: Running) -> Result<(), DaemonError> {
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        () = running.shutdown_requested() => {}
    }
    let fatal = running.fatal();
    running.shutdown().await;
    match fatal {
        Some(message) => Err(DaemonError::new(message)),
        None => Ok(()),
    }
}

/// Builds a runtime, serves like [`serve`], and ends the process after a
/// termination signal.
///
/// After the drain a Unix signal is raised again with its default
/// disposition, and a Windows console event exits with the code
/// [`finish`] maps it to, so this function does not return for them. A
/// control shutdown returns `Ok`.
///
/// # Errors
///
/// Returns [`DaemonError`] when the runtime or startup fails, or a fatal
/// error stopped the daemon.
pub fn serve_blocking(path: &Path) -> Result<(), DaemonError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(show)?;
    let reason = runtime.block_on(serve_reason(path))?;
    drop(runtime);
    terminate::conclude(reason).map_err(DaemonError::new)
}

fn token_file(path: &Path) -> Result<TokenFile, DaemonError> {
    let config = load_path(path).map_err(show)?;
    Ok(TokenFile::new(beside(path, &config.token_file)))
}

/// What the first router build returns to the runtime.
pub(crate) struct RouterParts {
    pub(crate) snapshot: Snapshot,
    pub(crate) table: Arc<membership::Table>,
    pub(crate) fingerprints: HashMap<hofund_plugin_api::MemberId, String>,
    pub(crate) instances: HashMap<String, Instance>,
    /// Providers only; subscription members are added at each publication.
    pub(crate) built: BTreeMap<String, membership::BuiltProvider>,
    /// Routes before subscription expansion.
    pub(crate) routes: BTreeMap<String, Vec<membership::PlannedHop>>,
    pub(crate) policy: FailoverPolicy,
}

/// One built provider, kept so a reload can reuse it.
///
/// A reload reuses the instance only when the provider's configuration, the
/// global timeouts, the plugin, the planned base URL and the credential
/// fingerprint are all unchanged. Anything else builds a new instance; the
/// retired one is dropped when the last request or snapshot holding it ends.
#[derive(Clone)]
pub(crate) struct Instance {
    provider: hofund_config::Provider,
    timeouts: (std::time::Duration, std::time::Duration),
    plugin_id: String,
    fingerprint: String,
    pub(crate) adapter: Arc<dyn Provider>,
    pub(crate) upstream: Arc<dyn UpstreamProvider>,
    pub(crate) base_url: String,
    model_list: bool,
}

/// Providers built from one configuration.
pub(crate) struct Built {
    pub(crate) built: BTreeMap<String, membership::BuiltProvider>,
    /// Keyed by provider id.
    pub(crate) instances: HashMap<String, Instance>,
}

fn router(
    config: &Config,
    path: &Path,
    max_idle_per_host: Option<usize>,
    mut models_host: Option<&mut ModelsHost>,
    registry: &Arc<dyn DaemonRegistry>,
    subscriptions: &subscriptions::Subscriptions,
    shared: (
        Arc<Cooldowns>,
        hofund_router::affinity::AffinityState,
        Arc<hofund_router::quota::QuotaStore>,
    ),
) -> Result<RouterParts, DaemonError> {
    let Built { built, instances } = providers(
        config,
        path,
        max_idle_per_host,
        Mode::Serve,
        models_host.as_deref(),
        None,
        Some(Arc::clone(&shared.0)),
        registry,
    )?;
    if let Some(host) = models_host.as_deref_mut() {
        for instance in instances.values().filter(|instance| instance.model_list) {
            host.register_instance(
                instance.provider.id.clone(),
                Arc::clone(&instance.upstream),
                instance.base_url.clone(),
            );
        }
    }
    let fingerprints = built
        .iter()
        .map(|(id, provider)| {
            (
                provider
                    .key
                    .as_ref()
                    .map_or_else(|| membership::member_id(id), |key| key.member.clone()),
                provider.fingerprint.clone(),
            )
        })
        .collect();
    let (routes, policy) = route_plan(config, models_host.as_deref(), Some(&built));
    let (members, expanded) = subscriptions::expand(&built, &routes, subscriptions)?;
    let (snapshot, table) = membership::open_with_table_and_affinity(
        &members,
        expanded,
        policy.clone(),
        shared.0,
        shared.1,
        Some(shared.2),
    )?;
    Ok(RouterParts {
        snapshot,
        table,
        fingerprints,
        instances,
        built,
        routes,
        policy,
    })
}

fn route_plan(
    config: &Config,
    models_host: Option<&ModelsHost>,
    built: Option<&BTreeMap<String, membership::BuiltProvider>>,
) -> (
    BTreeMap<String, Vec<membership::PlannedHop>>,
    FailoverPolicy,
) {
    let context = RoutePlanContext {
        providers: config
            .providers
            .iter()
            .map(|provider| (provider.id.as_str(), provider))
            .collect(),
        subscriptions: config
            .subscriptions
            .iter()
            .map(|subscription| (subscription.id.as_str(), subscription))
            .collect(),
        routes: config
            .routes
            .iter()
            .map(|route| (route.model.as_str(), route))
            .collect(),
        models_host,
        built,
    };
    let mut routes = BTreeMap::new();
    for route in &config.routes {
        let follow_fallback = route.upstreams.len() == 1
            && matches!(route.upstreams[0].target, UpstreamTarget::Provider(_));
        let list = expand_route(
            route,
            route,
            &route.model,
            Vec::new(),
            follow_fallback,
            false,
            &context,
            &mut BTreeSet::new(),
        );
        routes.insert(route.model.clone(), list);
    }
    let policy = FailoverPolicy {
        connect_errors: config.failover.connect_errors,
        statuses: config.failover.statuses.clone(),
    };
    (routes, policy)
}

struct RoutePlanContext<'a> {
    providers: BTreeMap<&'a str, &'a hofund_config::Provider>,
    subscriptions: BTreeMap<&'a str, &'a hofund_config::Subscription>,
    routes: BTreeMap<&'a str, &'a hofund_config::Route>,
    models_host: Option<&'a ModelsHost>,
    built: Option<&'a BTreeMap<String, membership::BuiltProvider>>,
}

#[allow(clippy::too_many_arguments)]
fn expand_route(
    route: &hofund_config::Route,
    root: &hofund_config::Route,
    bare: &str,
    parents: Vec<Unit>,
    follow_fallback: bool,
    fallback: bool,
    context: &RoutePlanContext<'_>,
    seen: &mut BTreeSet<(String, String)>,
) -> Vec<membership::PlannedHop> {
    let mut out = Vec::new();
    for upstream in &route.upstreams {
        match &upstream.target {
            UpstreamTarget::Provider(provider) => {
                push_provider(
                    root,
                    bare,
                    provider,
                    upstream.model.clone(),
                    upstream.max_output_tokens,
                    upstream.weight.unwrap_or(1),
                    fallback,
                    parents.clone(),
                    context,
                    seen,
                    &mut out,
                );
                if follow_fallback && let Some(config) = context.providers.get(provider.as_str()) {
                    for fallback in &config.fallback {
                        match fallback {
                            Fallback::Model {
                                provider: fallback_provider,
                                model,
                            } => push_provider(
                                root,
                                bare,
                                fallback_provider,
                                Some(model.clone()),
                                None,
                                1,
                                true,
                                parents.clone(),
                                context,
                                seen,
                                &mut out,
                            ),
                            Fallback::Group(group) => expand_group(
                                group,
                                1,
                                true,
                                root,
                                bare,
                                parents.clone(),
                                context,
                                seen,
                                &mut out,
                            ),
                        }
                    }
                }
            }
            UpstreamTarget::Subscription(subscription) => {
                let model = upstream.model.as_deref().unwrap_or(bare);
                if !seen.insert((format!("subscription:{subscription}"), model.to_owned())) {
                    continue;
                }
                let Some(config) = context.subscriptions.get(subscription.as_str()) else {
                    continue;
                };
                let mut units = parents.clone();
                units.push(Unit {
                    key: format!("subscription/{subscription}/{model}"),
                    pool: format!("subscription/{subscription}"),
                    routing: config.routing.clone(),
                    weight: upstream.weight.unwrap_or(1),
                    sink: false,
                    fallback,
                });
                out.push(membership::PlannedHop {
                    provider: String::new(),
                    subscription: Some(subscription.clone()),
                    member_id: None,
                    wire_model: upstream.model.clone(),
                    max_output_tokens: upstream.max_output_tokens,
                    context_window: None,
                    routing: root.routing.clone(),
                    sink: root.sink,
                    patience: root.patience.duration(),
                    weight: upstream.weight.unwrap_or(1),
                    free: false,
                    stays: effective_stays(root, &context.providers, &context.subscriptions),
                    cold_after: if subscription == "codex" {
                        hofund_router::affinity::CACHE_HOUR
                    } else {
                        hofund_router::affinity::CACHE_COLD
                    },
                    units,
                });
            }
            UpstreamTarget::Group(group) => expand_group(
                group,
                upstream.weight.unwrap_or(1),
                fallback,
                root,
                bare,
                parents.clone(),
                context,
                seen,
                &mut out,
            ),
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn expand_group(
    id: &str,
    weight: u64,
    fallback: bool,
    root: &hofund_config::Route,
    bare: &str,
    mut parents: Vec<Unit>,
    context: &RoutePlanContext<'_>,
    seen: &mut BTreeSet<(String, String)>,
    out: &mut Vec<membership::PlannedHop>,
) {
    let name = format!("group/{id}");
    let group = context.routes[name.as_str()];
    parents.push(Unit {
        key: name.clone(),
        pool: name,
        routing: group.routing.clone(),
        weight,
        sink: group.sink,
        fallback,
    });
    out.extend(expand_route(
        group, root, bare, parents, false, fallback, context, seen,
    ));
}

#[allow(clippy::too_many_arguments)]
fn push_provider(
    root: &hofund_config::Route,
    bare: &str,
    id: &str,
    model: Option<String>,
    max_output_tokens: Option<u64>,
    weight: u64,
    fallback: bool,
    mut parents: Vec<Unit>,
    context: &RoutePlanContext<'_>,
    seen: &mut BTreeSet<(String, String)>,
    out: &mut Vec<membership::PlannedHop>,
) {
    let (provider_cap, provider_free, cold_after) = context.providers.get(id).map_or(
        (None, false, hofund_router::affinity::CACHE_COLD),
        |provider| {
            (
                provider.max_output_tokens,
                provider.free,
                provider_cold_after(provider),
            )
        },
    );
    let model_name = model.as_deref().unwrap_or(bare);
    if !seen.insert((id.to_owned(), model_name.to_owned())) {
        return;
    }
    let listed = context
        .models_host
        .and_then(|host| listed_model(host.cached(id).models, model_name));
    parents.push(Unit {
        key: format!("provider/{id}/{model_name}"),
        pool: format!("provider/{id}"),
        routing: "smart".to_owned(),
        weight,
        sink: false,
        fallback,
    });
    let mut keys = context
        .built
        .into_iter()
        .flat_map(|built| built.values())
        .filter_map(|candidate| candidate.key.as_ref().filter(|key| key.provider == *id))
        .collect::<Vec<_>>();
    keys.sort_by_key(|key| key.rank);
    let candidates = if keys.is_empty()
        && (context.built.is_none_or(|built| built.contains_key(id))
            || context
                .providers
                .get(id)
                .is_none_or(|provider| provider.key_entries().is_empty()))
    {
        vec![(None, weight)]
    } else {
        keys.into_iter()
            .map(|key| (Some(key.member.clone()), key.weight))
            .collect()
    };
    for (member_id, key_weight) in candidates {
        out.push(membership::PlannedHop {
            provider: id.to_owned(),
            subscription: None,
            member_id,
            wire_model: model.clone(),
            max_output_tokens: max_output_tokens.or(provider_cap),
            context_window: hop_window(listed.as_ref()),
            routing: root.routing.clone(),
            sink: root.sink,
            patience: root.patience.duration(),
            weight: key_weight,
            free: free_hop(provider_free, listed.as_ref()),
            stays: effective_stays(root, &context.providers, &context.subscriptions),
            cold_after,
            units: parents.clone(),
        });
    }
}

pub(crate) fn route_stays(
    config: &Config,
    route: &hofund_config::Route,
) -> hofund_router::affinity::Stays {
    let providers = config
        .providers
        .iter()
        .map(|provider| (provider.id.as_str(), provider))
        .collect();
    let subscriptions = config
        .subscriptions
        .iter()
        .map(|subscription| (subscription.id.as_str(), subscription))
        .collect();
    effective_stays(route, &providers, &subscriptions)
}

fn effective_stays(
    route: &hofund_config::Route,
    providers: &BTreeMap<&str, &hofund_config::Provider>,
    subscriptions: &BTreeMap<&str, &hofund_config::Subscription>,
) -> hofund_router::affinity::Stays {
    let configured = if route.stays_explicit {
        route.stays
    } else if route.upstreams.len() == 1 {
        match &route.upstreams[0].target {
            UpstreamTarget::Provider(id) => providers
                .get(id.as_str())
                .map_or(hofund_config::Stays::Auto, |provider| provider.stays),
            UpstreamTarget::Subscription(id) => subscriptions
                .get(id.as_str())
                .map_or(hofund_config::Stays::Auto, |subscription| {
                    subscription.stays
                }),
            UpstreamTarget::Group(_) => hofund_config::Stays::Auto,
        }
    } else {
        hofund_config::Stays::Auto
    };
    hofund_router::affinity::Stays::parse(configured.as_str()).expect("validated stays mode")
}

fn provider_cold_after(provider: &hofund_config::Provider) -> std::time::Duration {
    let own_host = models::url_host(&provider.base_url).as_deref() == Some("api.deepseek.com");
    if own_host || provider.preset.as_deref() == Some("deepseek") {
        hofund_router::affinity::CACHE_HOUR
    } else {
        hofund_router::affinity::CACHE_COLD
    }
}

/// The catalog entry for `model`, matched without regard to case as the
/// reference's free-model lookup does.
fn listed_model(
    models: Vec<hofund_plugin_api::ModelSpec>,
    model: &str,
) -> Option<hofund_plugin_api::ModelSpec> {
    models
        .into_iter()
        .find(|listed| listed.id().eq_ignore_ascii_case(model))
}

/// The input window the model list gives a hop's model, when it gives one.
fn hop_window(listed: Option<&hofund_plugin_api::ModelSpec>) -> Option<u64> {
    listed
        .map(hofund_plugin_api::ModelSpec::context)
        .filter(|window| *window > 0)
}

fn free_hop(manual: bool, listed: Option<&hofund_plugin_api::ModelSpec>) -> bool {
    manual || listed.is_some_and(hofund_plugin_api::ModelSpec::costs_nothing)
}

#[derive(Clone, Copy)]
enum Mode {
    Check,
    Serve,
}

#[allow(clippy::too_many_arguments)]
fn providers(
    config: &Config,
    path: &Path,
    max_idle_per_host: Option<usize>,
    mode: Mode,
    models_host: Option<&ModelsHost>,
    reuse: Option<&HashMap<String, Instance>>,
    cooldowns: Option<Arc<Cooldowns>>,
    registry: &Arc<dyn DaemonRegistry>,
) -> Result<Built, DaemonError> {
    let limits = http_limits(config, max_idle_per_host);
    let timeouts = (config.connect_timeout, config.read_timeout);
    let mut built = BTreeMap::new();
    let mut instances = HashMap::new();
    for provider in &config.providers {
        let planned = match models_host {
            Some(host) => host.planned(provider)?,
            None => plan_using(provider, |id| registry.find_preset(id), registry)?,
        };
        let protocol = protocol_of(planned.wire);
        let plugin_id = plugin_for(provider, &planned, registry);
        let (several, mut keys) = provider_keys::enabled(provider, protocol)?;
        if keys.is_empty() && provider.key_entries().is_empty() {
            keys.push(provider_keys::ResolvedKey {
                entry: hofund_config::ProviderKey {
                    key: String::new(),
                    name: String::new(),
                    on: true,
                    protocol: None,
                    plan: None,
                    weight: 1,
                },
                id: String::new(),
            });
        }
        for (rank, key) in keys.into_iter().enumerate() {
            let instance_id = if several {
                provider_keys::built_id(&provider.id, &key.id)
            } else {
                provider.id.clone()
            };
            let mut selected = provider.clone();
            selected.api_key.clone_from(&key.entry.key);
            selected.key_name.clone_from(&key.entry.name);
            selected.key_protocol.clone_from(&key.entry.protocol);
            selected.key_weight = key.entry.weight;
            selected.keys.clear();
            let endpoint = endpoint_for(&selected, &planned)?;
            let timeout = endpoint.timeout;
            let fingerprint = provider_fingerprint(&selected, &endpoint)?;
            let reused = reuse
                .and_then(|previous| previous.get(&instance_id))
                .filter(|previous| {
                    previous.fingerprint == fingerprint
                        && previous.base_url == planned.base_url
                        && previous.plugin_id == plugin_id
                        && previous.timeouts == timeouts
                        && previous.provider == selected
                })
                .cloned();
            let mut instance = match reused {
                Some(instance) => instance,
                None => {
                    let adapter = build_plugin(
                        &selected,
                        &planned,
                        &endpoint,
                        path,
                        limits,
                        mode,
                        cooldowns.clone(),
                        registry,
                    )?;
                    let upstream = adapter.upstream();
                    Instance {
                        provider: selected,
                        timeouts,
                        plugin_id: plugin_id.clone(),
                        fingerprint: fingerprint.clone(),
                        adapter: Arc::new(adapter),
                        upstream,
                        base_url: planned.base_url.clone(),
                        model_list: rank == 0,
                    }
                }
            };
            instance.model_list = rank == 0;
            let key_member = several.then(|| provider_keys::KeyMember {
                provider: provider.id.clone(),
                member: membership::key_member_id(&provider.id, &key.id),
                weight: key.entry.weight,
                rank,
            });
            built.insert(
                instance_id.clone(),
                membership::BuiltProvider {
                    adapter: Arc::clone(&instance.adapter),
                    protocol,
                    timeout,
                    kernel: registry.is_kernel_factory(&plugin_id),
                    plugin_id: plugin_id.clone(),
                    preset_id: provider.preset.clone().unwrap_or_default(),
                    fingerprint,
                    patience: provider.patience.duration(),
                    base_url: endpoint.base_url.clone(),
                    key: key_member,
                },
            );
            instances.insert(instance_id, instance);
        }
    }
    Ok(Built { built, instances })
}

fn provider_fingerprint(
    provider: &hofund_config::Provider,
    endpoint: &Endpoint,
) -> Result<String, DaemonError> {
    use sha2::{Digest, Sha256};

    fn field(hash: &mut Sha256, label: &str, value: &str) {
        hash.update((label.len() as u64).to_be_bytes());
        hash.update(label.as_bytes());
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value.as_bytes());
    }

    let mut hash = Sha256::new();
    hash.update(b"hofund-provider-fingerprint-v2\0");
    field(&mut hash, "api_key", &endpoint.api_key);
    for name in ["access_key_id", "secret_access_key", "session_token"] {
        let value = match provider.options.get(name) {
            None => String::new(),
            Some(Value::String(value)) => {
                hofund_secrets::resolve(value.trim()).map_err(|error| {
                    DaemonError::new(format!(
                        "provider `{}` option `{name}`: {error}",
                        provider.id
                    ))
                })?
            }
            Some(_) => {
                return Err(DaemonError::new(format!(
                    "provider `{}` option `{name}` must be a string",
                    provider.id
                )));
            }
        };
        field(&mut hash, name, &value);
    }
    for (name, value) in &endpoint.headers {
        let name = name.to_ascii_lowercase();
        field(&mut hash, &format!("header:{name}"), value);
    }
    field(&mut hash, "base_url", &endpoint.base_url);
    Ok(hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
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

fn plugin_for(
    provider: &hofund_config::Provider,
    planned: &Planned,
    registry: &Arc<dyn DaemonRegistry>,
) -> String {
    plugin_for_host(provider, planned, &planned_host(planned), registry)
}

/// [`plugin_for`] with the base URL's host given, so the models host can
/// apply its test host overrides.
fn plugin_for_host(
    provider: &hofund_config::Provider,
    planned: &Planned,
    host: &str,
    registry: &Arc<dyn DaemonRegistry>,
) -> String {
    plugin_for_using(
        provider,
        planned.wire,
        host,
        |id| registry.find_preset(id),
        registry,
    )
}

/// The lowercase host of the planned base URL, or an empty string.
fn planned_host(planned: &Planned) -> String {
    models::url_host(&planned.base_url).unwrap_or_default()
}

#[cfg(test)]
fn plugin_for_with(
    provider: &hofund_config::Provider,
    wire: Wire,
    lookup: impl Fn(&str) -> Option<(&'static registry::CompiledPreset, Option<&'static str>)>,
) -> String {
    let registry: Arc<dyn DaemonRegistry> = Arc::new(RegistryAdapter);
    let host = models::url_host(&provider.base_url).unwrap_or_default();
    plugin_for_using(provider, wire, &host, lookup, &registry)
}

fn plugin_for_using(
    provider: &hofund_config::Provider,
    wire: Wire,
    host: &str,
    lookup: impl Fn(&str) -> Option<(&'static registry::CompiledPreset, Option<&'static str>)>,
    registry: &Arc<dyn DaemonRegistry>,
) -> String {
    if let Some(plugin) = host_plugin_using(provider, wire, host, &lookup, registry) {
        return plugin.to_owned();
    }
    if let Some(preset) = &provider.preset
        && let Some((compiled, _)) = lookup(preset)
    {
        if registry.builtin_factory_ids().contains(&compiled.plugin) {
            return implementation_id(wire).to_owned();
        }
        return compiled.plugin.to_owned();
    }
    implementation_id(wire).to_owned()
}

/// The linked plugin that serves this provider's base by its host.
///
/// Only a provider a kernel factory would build is claimed: one with no
/// preset, a catalog row, or a builtin-owned compiled preset. A listed
/// plugin's own preset keeps its plugin. An Azure OpenAI host on Chat or
/// Responses goes to the `azure` plugin.
fn host_plugin_using(
    provider: &hofund_config::Provider,
    wire: Wire,
    host: &str,
    lookup: &impl Fn(&str) -> Option<(&'static registry::CompiledPreset, Option<&'static str>)>,
    registry: &Arc<dyn DaemonRegistry>,
) -> Option<&'static str> {
    if let Some(preset) = &provider.preset
        && let Some((compiled, _)) = lookup(preset)
        && !registry.builtin_factory_ids().contains(&compiled.plugin)
    {
        return None;
    }
    let protocol = registry::protocol_for(wire)?;
    let plugin = registry.host_plugin(protocol, host)?;
    registry.factory(plugin).is_some().then_some(plugin)
}

#[allow(clippy::too_many_arguments)]
fn build_plugin(
    provider: &hofund_config::Provider,
    planned: &Planned,
    endpoint: &Endpoint,
    path: &Path,
    limits: HttpLimits,
    mode: Mode,
    cooldowns: Option<Arc<Cooldowns>>,
    registry: &Arc<dyn DaemonRegistry>,
) -> Result<adapt::ProviderAdapter, DaemonError> {
    let plugin_id = plugin_for(provider, planned, registry);
    let raw = raw_for(provider, planned, endpoint, registry)?;
    let upstream = build_raw_plugin(
        provider,
        &plugin_id,
        raw,
        planned.protocol,
        &endpoint.api_key,
        path,
        limits,
        true,
        matches!(mode, Mode::Check),
        cooldowns,
        registry,
    )?;
    Ok(adapt::ProviderAdapter::new(provider.id.clone(), upstream))
}

#[allow(clippy::too_many_arguments)]
fn build_raw_plugin(
    provider: &hofund_config::Provider,
    plugin_id: &str,
    mut raw: RawTable,
    protocol: Option<ProtocolId>,
    api_key: &str,
    path: &Path,
    limits: HttpLimits,
    resolve_own_options: bool,
    preflight: bool,
    cooldowns: Option<Arc<Cooldowns>>,
    registry: &Arc<dyn DaemonRegistry>,
) -> Result<Arc<dyn UpstreamProvider>, DaemonError> {
    let factory = registry
        .factory(plugin_id)
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
    // Kernel-owned factories (`openai-compat`, `anthropic`, `generative`,
    // `bedrock`) are not linked plugins: they read the wire
    // from the `protocol` raw key or are single-wire catalog factories, and
    // keep the default `build_with`, so they build through `validate_and_build`.
    // Only a linked (listed) plugin receives the selected protocol in
    // `BuildContext`, and a listed plugin without one is an error.
    if registry.is_kernel_factory(plugin_id) {
        return factory.validate_and_build(&raw, &host).map_err(show);
    }
    let protocol = protocol.ok_or_else(|| {
        DaemonError::new(format!(
            "provider `{}` has no selected protocol for plugin `{plugin_id}`",
            provider.id
        ))
    })?;
    factory
        .validate_and_build_with(&raw, &host, &BuildContext::new(protocol))
        .map_err(show)
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
    registry: &Arc<dyn DaemonRegistry>,
) -> Result<RawTable, DaemonError> {
    raw_for_host(
        provider,
        planned,
        endpoint,
        &planned_host(planned),
        registry,
    )
}

/// [`raw_for`] with the base URL's host given, as [`plugin_for_host`].
fn raw_for_host(
    provider: &hofund_config::Provider,
    planned: &Planned,
    endpoint: &Endpoint,
    host: &str,
    registry: &Arc<dyn DaemonRegistry>,
) -> Result<RawTable, DaemonError> {
    raw_for_using(
        provider,
        planned,
        endpoint,
        host,
        |id| registry.find_preset(id),
        registry,
    )
}

#[cfg(test)]
fn raw_for_with(
    provider: &hofund_config::Provider,
    planned: &Planned,
    endpoint: &Endpoint,
    lookup: impl Fn(&str) -> Option<(&'static registry::CompiledPreset, Option<&'static str>)>,
) -> Result<RawTable, DaemonError> {
    let registry: Arc<dyn DaemonRegistry> = Arc::new(RegistryAdapter);
    raw_for_using(
        provider,
        planned,
        endpoint,
        &planned_host(planned),
        lookup,
        &registry,
    )
}

fn raw_for_using(
    provider: &hofund_config::Provider,
    planned: &Planned,
    endpoint: &Endpoint,
    host: &str,
    lookup: impl Fn(&str) -> Option<(&'static registry::CompiledPreset, Option<&'static str>)>,
    registry: &Arc<dyn DaemonRegistry>,
) -> Result<RawTable, DaemonError> {
    if let Some(plugin) = host_plugin_using(provider, planned.wire, host, &lookup, registry) {
        let config = registry
            .find_preset(plugin)
            .map_or(&[][..], |(compiled, _)| compiled.config);
        return raw_listed(config, provider, &planned.base_url, endpoint);
    }
    if let Some(preset) = &provider.preset
        && let Some((compiled, _)) = lookup(preset)
        && !registry.builtin_factory_ids().contains(&compiled.plugin)
    {
        return raw_from_preset(compiled, provider, &planned.base_url, endpoint);
    }
    raw_from_endpoint(provider, planned, endpoint)
}

/// A listed plugin's preset options: the preset's config, the base URL, the
/// provider's `headers` as a table (a plugin that takes none rejects them as
/// an unknown key rather than dropping them), and the provider's options.
fn raw_from_preset(
    preset: &registry::CompiledPreset,
    provider: &hofund_config::Provider,
    base_url: &str,
    endpoint: &Endpoint,
) -> Result<RawTable, DaemonError> {
    raw_listed(preset.config, provider, base_url, endpoint)
}

/// [`raw_from_preset`] from a preset's config pairs.
fn raw_listed(
    config: &[(&str, &str)],
    provider: &hofund_config::Provider,
    base_url: &str,
    endpoint: &Endpoint,
) -> Result<RawTable, DaemonError> {
    let mut raw = RawTable::new();
    for (key, value) in config {
        raw.insert((*key).to_owned(), RawValue::String((*value).to_owned()));
    }
    if !base_url.is_empty() {
        raw.insert("base_url", RawValue::String(base_url.to_owned()));
    }
    insert_headers(&mut raw, endpoint);
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
    insert_headers(&mut raw, endpoint);
    match planned.wire {
        Wire::Chat => raw.insert("protocol", RawValue::String("chat".to_owned())),
        Wire::Responses => raw.insert("protocol", RawValue::String("responses".to_owned())),
        Wire::Messages | Wire::Generative | Wire::Vertex | Wire::Converse => {}
    }
    insert_options(&mut raw, provider)?;
    Ok(raw)
}

fn insert_headers(raw: &mut RawTable, endpoint: &Endpoint) {
    if !endpoint.headers.is_empty() {
        let mut headers = RawTable::new();
        for (name, value) in &endpoint.headers {
            headers.insert(name.clone(), RawValue::String(value.clone()));
        }
        raw.insert("headers", RawValue::Table(headers));
    }
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
            raw_value(
                value,
                Some(("provider", provider.id.as_str(), key.as_str())),
            )?,
        );
    }
    Ok(())
}

/// Converts a config value. With `environment` (owner kind, owner id, option
/// key), strings resolve `env:` references.
fn raw_value(
    value: &Value,
    environment: Option<(&str, &str, &str)>,
) -> Result<RawValue, DaemonError> {
    Ok(match value {
        Value::String(text) => {
            let text = text.trim();
            let resolved = match environment {
                Some((kind, owner, key)) => hofund_secrets::resolve(text).map_err(|error| {
                    DaemonError::new(format!("{kind} `{owner}` option `{key}`: {error}"))
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
    protocol: Option<ProtocolId>,
    credential: Credential,
}

fn compiled_credential(
    compiled: &'static registry::CompiledPreset,
    region: Option<&str>,
    wire: Wire,
    registry: &Arc<dyn DaemonRegistry>,
) -> Result<Credential, DaemonError> {
    if !registry.builtin_factory_ids().contains(&compiled.plugin) {
        return Ok(Credential::Default);
    }
    let protocol = registry::protocol_for(wire)
        .ok_or_else(|| DaemonError::new(format!("preset `{}` cannot use {wire:?}", compiled.id)))?;
    let selection = registry::select(
        compiled,
        region,
        &registry.builtin_protocols(),
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

/// Every preset row is a compiled fragment, so a preset id the registry does
/// not know has no other source.
fn unknown_preset(preset: &str) -> DaemonError {
    DaemonError::new(format!("unknown preset `{preset}`"))
}

fn served_for(plugin: &str, protocols: &[ProtocolId]) -> Result<Vec<ProtocolId>, DaemonError> {
    if protocols.is_empty() {
        return Err(DaemonError::new(format!(
            "plugin `{plugin}` declares no protocol"
        )));
    }
    Ok(protocols.to_vec())
}

#[cfg(test)]
fn plan(provider: &hofund_config::Provider) -> Result<Planned, DaemonError> {
    let registry: Arc<dyn DaemonRegistry> = Arc::new(RegistryAdapter);
    plan_using(provider, hofund_plugin_registry::find_preset, &registry)
}

#[cfg(test)]
fn plan_with(
    provider: &hofund_config::Provider,
    lookup: impl Fn(&str) -> Option<(&'static registry::CompiledPreset, Option<&'static str>)>,
) -> Result<Planned, DaemonError> {
    let registry: Arc<dyn DaemonRegistry> = Arc::new(RegistryAdapter);
    plan_using(provider, lookup, &registry)
}

fn plan_using(
    provider: &hofund_config::Provider,
    lookup: impl Fn(&str) -> Option<(&'static registry::CompiledPreset, Option<&'static str>)>,
    registry: &Arc<dyn DaemonRegistry>,
) -> Result<Planned, DaemonError> {
    let selected = match provider.protocol.as_deref() {
        Some(protocol) => Some(selected_protocol(&provider.id, protocol)?),
        None => None,
    };
    if let Some(preset) = &provider.preset
        && let Some((compiled, alias_region)) = lookup(preset)
    {
        let region = option_present(provider, "region")?.or(alias_region);
        let builtin = registry.builtin_factory_ids().contains(&compiled.plugin);
        let served = if builtin {
            registry.builtin_protocols()
        } else {
            let factory = registry.factory(compiled.plugin).ok_or_else(|| {
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
        let selection = registry::select(
            compiled,
            region,
            &served,
            selected.map(|(protocol, _)| protocol),
            &provider.base_url,
        )
        .map_err(show)?;
        // The credential follows the final base, so a
        // `base_url` override applies its host rules.
        let credential = if builtin {
            hofund_provider::credential_for(compiled.id, selection.wire, &selection.base_url)
        } else {
            Credential::Default
        };
        return Ok(Planned {
            base_url: selection.base_url,
            wire: selection.wire,
            protocol: Some(selection.protocol),
            credential,
        });
    }
    if let Some(preset) = &provider.preset {
        return Err(unknown_preset(preset));
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
        protocol: selected.map(|(protocol, _)| protocol),
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
    let wire =
        registry::wire_for_protocol(id).ok_or_else(|| unknown_protocol(provider_id, protocol))?;
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

/// The pooled client inbound surfaces relay on, with the configured
/// connect and idle-read limits.
fn surface_http(config: &Config, max_idle_per_host: Option<usize>) -> Arc<dyn HttpBackend> {
    Arc::new(PooledHttp::with_limits(
        HttpProfile::direct(),
        http_limits(config, max_idle_per_host),
    ))
}

/// The connect, idle-read and idle-pool limits every pooled client uses.
fn http_limits(config: &Config, max_idle_per_host: Option<usize>) -> HttpLimits {
    let mut limits = HttpLimits::new(config.connect_timeout, config.read_timeout);
    if let Some(max_idle_per_host) = max_idle_per_host {
        limits = limits.with_max_idle_per_host(max_idle_per_host);
    }
    limits
}

fn warn(config: &Config) {
    for warning in &config.warnings {
        terminate::notice(&format!("warning: {warning}"));
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

    fn registry() -> std::sync::Arc<dyn super::DaemonRegistry> {
        std::sync::Arc::new(super::RegistryAdapter)
    }

    #[test]
    fn free_hop_uses_manual_catalog_and_zero_price_flags() {
        let free = hofund_plugin_api::ModelSpec::new("free", None)
            .expect("model")
            .with_free(true);
        let zero = hofund_plugin_api::ModelSpec::new("zero", None)
            .expect("model")
            .with_prices(0, 0, 0);
        let paid = hofund_plugin_api::ModelSpec::new("paid", None)
            .expect("model")
            .with_prices(1, 2, 0);
        // A false catalog flag does not hide an all-zero price.
        let flagged_paid_zero = hofund_plugin_api::ModelSpec::new("paid", None)
            .expect("model")
            .with_prices(0, 0, 0)
            .with_free(false);
        let flagged_paid = hofund_plugin_api::ModelSpec::new("paid", None)
            .expect("model")
            .with_prices(0, 1, 0)
            .with_free(false);

        assert!(super::free_hop(true, None));
        assert!(super::free_hop(false, Some(&free)));
        assert!(super::free_hop(false, Some(&zero)));
        assert!(!super::free_hop(false, Some(&paid)));
        assert!(super::free_hop(false, Some(&flagged_paid_zero)));
        assert!(!super::free_hop(false, Some(&flagged_paid)));
        assert!(!super::free_hop(false, None));
    }

    #[test]
    fn a7_catalog_lookup_for_free_ignores_model_case() {
        let models = vec![
            hofund_plugin_api::ModelSpec::new("Other", None).expect("model"),
            hofund_plugin_api::ModelSpec::new("Mixed-Case", None)
                .expect("model")
                .with_free(true),
        ];
        let found = super::listed_model(models.clone(), "mixed-case").expect("listed");
        assert_eq!(found.id(), "Mixed-Case");
        assert!(super::free_hop(false, Some(&found)));
        assert!(super::listed_model(models, "absent").is_none());
    }

    #[test]
    fn route_plan_carries_routing_weight_and_free_flag() {
        let config = hofund_config::load(
            "[[providers]]\nid = \"one\"\npreset = \"openai\"\nfree = true\n[[routes]]\nmodel = \"demo\"\nrouting = \"weight\"\nupstreams = [{ provider = \"one\", weight = 3 }]\n",
        )
        .expect("config");
        let (routes, _) = super::route_plan(&config, None, None);
        let hop = &routes["demo"][0];
        assert_eq!(hop.routing, "weight");
        assert_eq!(hop.weight, 3);
        assert!(hop.free);
    }

    #[test]
    fn d3_route_plan_carries_sink_to_provider_hops() {
        let config = hofund_config::load(
            "[[providers]]\nid = \"one\"\npreset = \"openai\"\n[[routes]]\nmodel = \"demo\"\nsink = true\nupstreams = [{ provider = \"one\" }]\n",
        )
        .expect("config");
        let (routes, _) = super::route_plan(&config, None, None);
        assert!(routes["demo"][0].sink);
    }

    #[test]
    fn k3_candidates_keep_key_order_ids_and_protocol_filters() {
        let config = hofund_config::load(
            "[[providers]]\nid = \"one\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key = \"first\"\nkey_weight = 2\n[[providers.keys]]\nkey = \"off\"\non = false\n[[providers.keys]]\nkey = \"responses\"\nprotocol = \"openai-responses\"\n[[providers.keys]]\nkey = \"last\"\nweight = 3\n[[routes]]\nmodel = \"demo\"\nrouting = \"weight\"\nupstreams = [{ provider = \"one\", weight = 9 }]\n",
        )
        .expect("config");
        let dir = tempfile::tempdir().expect("dir");
        let registry = registry();
        let built = super::providers(
            &config,
            &dir.path().join("hofund.toml"),
            None,
            super::Mode::Check,
            None,
            None,
            None,
            &registry,
        )
        .expect("providers");
        assert_eq!(built.built.len(), 2);
        let (routes, _) = super::route_plan(&config, None, Some(&built.built));
        let hops = &routes["demo"];
        assert_eq!(hops.len(), 2);
        assert_eq!(
            hops[0].member_id,
            Some(super::membership::key_member_id(
                "one",
                &super::provider_keys::key_id("first")
            ))
        );
        assert_eq!(
            hops[1].member_id,
            Some(super::membership::key_member_id(
                "one",
                &super::provider_keys::key_id("last")
            ))
        );
        assert_eq!(hops[0].weight, 2);
        assert_eq!(hops[1].weight, 3);
    }

    #[test]
    fn k3_one_enabled_key_keeps_the_provider_member_id() {
        let config = hofund_config::load(
            "[[providers]]\nid = \"one\"\npreset = \"openai\"\napi_key = \"only\"\n[[routes]]\nmodel = \"demo\"\nupstreams = [\"one\"]\n",
        )
        .expect("config");
        let dir = tempfile::tempdir().expect("dir");
        let registry = registry();
        let built = super::providers(
            &config,
            &dir.path().join("hofund.toml"),
            None,
            super::Mode::Check,
            None,
            None,
            None,
            &registry,
        )
        .expect("providers");
        let (routes, _) = super::route_plan(&config, None, Some(&built.built));
        assert_eq!(routes["demo"][0].member_id, None);
        assert!(built.built.contains_key("one"));
    }

    #[test]
    fn d1_fallback_group_expands_whole_in_order_and_deduplicates_members() {
        let config = hofund_config::load(
            r#"
            [[providers]]
            id = "plan"
            preset = "openai"
            fallback = ["group/backup", "a/x"]
            [[providers]]
            id = "a"
            preset = "openai"
            [[providers]]
            id = "b"
            preset = "openai"
            [[routes]]
            model = "group/backup"
            routing = "order"
            upstreams = [
              { provider = "plan", model = "m" },
              { provider = "b", model = "x" },
              { provider = "a", model = "x" },
            ]
            [[routes]]
            model = "demo"
            upstreams = [{ provider = "plan", model = "m" }]
            "#,
        )
        .expect("fallback group config");
        let (routes, _) = super::route_plan(&config, None, None);
        let hops = &routes["demo"];
        assert_eq!(
            hops.iter()
                .map(|hop| (hop.provider.as_str(), hop.wire_model.as_deref()))
                .collect::<Vec<_>>(),
            [("plan", Some("m")), ("b", Some("x")), ("a", Some("x"))]
        );
        assert_eq!(hops[1].units[0].key, "group/backup");
        assert_eq!(hops[1].units[0].routing, "order");
        assert_eq!(hops[2].units[0].key, "group/backup");
    }

    #[test]
    fn b11_route_plan_carries_the_routes_patience_not_the_providers() {
        let config = hofund_config::load(
            "[[providers]]\nid = \"one\"\npreset = \"openai\"\npatience = \"off\"\n[[routes]]\nmodel = \"demo\"\npatience = 90\nupstreams = [{ provider = \"one\" }]\n[[routes]]\nmodel = \"plain\"\nupstreams = [{ provider = \"one\" }]\n",
        )
        .expect("config");
        let (routes, _) = super::route_plan(&config, None, None);
        assert_eq!(
            routes["demo"][0].patience,
            Some(std::time::Duration::from_secs(90))
        );
        assert_eq!(
            routes["plain"][0].patience,
            Some(std::time::Duration::from_secs(60)),
            "a route's default is 60 seconds whatever its providers say"
        );
    }

    #[test]
    fn b10d_route_plan_gives_a_provider_hop_its_listed_window() {
        let config = hofund_config::load(
            "[[providers]]\nid = \"one\"\npreset = \"openai\"\n[[routes]]\nmodel = \"Demo\"\nupstreams = [{ provider = \"one\" }, { provider = \"one\", model = \"bare\" }]\n",
        )
        .expect("config");
        let host = super::ModelsHost::new(
            std::time::Duration::from_secs(1),
            std::time::Duration::from_secs(1),
        );
        let sized = hofund_plugin_api::ModelSpec::new("demo", None)
            .expect("model")
            .with_context(64_000);
        let bare = hofund_plugin_api::ModelSpec::new("bare", None).expect("model");
        host.seed_models("one", &[sized, bare]);
        let (routes, _) = super::route_plan(&config, Some(&host), None);
        let hops = &routes["Demo"];
        assert_eq!(hops[0].context_window, Some(64_000));
        assert_eq!(hops[1].context_window, None);
        let (routes, _) = super::route_plan(&config, None, None);
        assert_eq!(routes["Demo"][0].context_window, None);
    }

    #[test]
    fn the_agent_session_carries_the_os_credential_store() {
        // Building the store does not touch the OS store.
        let session = format!("{:?}", super::agent_session());
        assert!(session.contains("credential_store: true"), "{session}");
    }

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
    fn the_remote_gateway_preset_plans_through_its_plugin() {
        let config = hofund_config::load(
            "[[providers]]\nid = \"lan\"\npreset = \"remote-gateway\"\nprotocol = \"anthropic-messages\"\nbase_url = \"http://10.0.0.7:3425\"\napi_key = \"test-key\"\n",
        )
        .expect("config");
        let planned = super::plan(&config.providers[0]).expect("plan");
        assert_eq!(planned.wire, hofund_provider::Wire::Messages);
        assert_eq!(
            planned.protocol,
            Some(hofund_plugin_api::ProtocolId::ANTHROPIC_MESSAGES)
        );
        assert_eq!(planned.base_url, "http://10.0.0.7:3425");
        assert_eq!(planned.credential, hofund_provider::Credential::Default);
        let missing = hofund_config::load(
            "[[providers]]\nid = \"lan\"\npreset = \"remote-gateway\"\napi_key = \"test-key\"\n",
        )
        .expect("config");
        let Err(error) = super::plan(&missing.providers[0]) else {
            panic!("a remote gateway needs an address");
        };
        assert!(error.to_string().contains("remote-gateway"), "{error}");
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
    fn moved_generative_and_messages_presets_plan_through_the_real_registry() {
        for (preset, extra, wire, base_url, credential, plugin) in [
            (
                "google",
                "",
                hofund_provider::Wire::Generative,
                "https://generativelanguage.googleapis.com/v1beta",
                hofund_provider::Credential::GoogApiKey,
                "generative",
            ),
            (
                "anthropic",
                "",
                hofund_provider::Wire::Messages,
                "https://api.anthropic.com",
                hofund_provider::Credential::XApiKey,
                "anthropic",
            ),
            (
                "anthropic",
                "base_url = \"https://relay.example.test\"\n",
                hofund_provider::Wire::Messages,
                "https://relay.example.test",
                hofund_provider::Credential::MessagesBoth,
                "anthropic",
            ),
        ] {
            let config = hofund_config::load(&format!(
                "[[providers]]\nid = \"{preset}\"\npreset = \"{preset}\"\n{extra}api_key = \"test-key\"\n"
            ))
            .expect("config");
            let provider = &config.providers[0];
            let planned = super::plan(provider).expect("real registry plan");
            assert_eq!(planned.wire, wire);
            let protocol = match wire {
                hofund_provider::Wire::Generative => {
                    hofund_plugin_api::ProtocolId::GENERATIVE_LANGUAGE
                }
                _ => hofund_plugin_api::ProtocolId::ANTHROPIC_MESSAGES,
            };
            assert_eq!(planned.protocol, Some(protocol), "{preset} {extra}");
            assert_eq!(planned.base_url, base_url);
            assert_eq!(planned.credential, credential);
            assert_eq!(super::plugin_for(provider, &planned, &registry()), plugin);
        }
    }

    // reference: internal/provider/azure.go:41-55
    #[test]
    fn a_custom_chat_or_responses_base_on_an_azure_host_goes_to_the_azure_plugin() {
        for (settings, plugin) in [
            (
                "protocol = \"openai-chat\"\nbase_url = \"https://mine.openai.azure.com/openai/v1\"\n",
                "azure",
            ),
            (
                "protocol = \"openai-responses\"\nbase_url = \"https://mine.cognitiveservices.azure.com/openai/deployments/d?api-version=1\"\n",
                "azure",
            ),
            (
                "preset = \"openai\"\nbase_url = \"https://mine.services.ai.azure.com/openai/v1\"\n",
                "azure",
            ),
            (
                "protocol = \"anthropic-messages\"\nbase_url = \"https://mine.services.ai.azure.com/anthropic\"\n",
                "anthropic",
            ),
            (
                "protocol = \"openai-chat\"\nbase_url = \"https://openai.azure.com/v1\"\n",
                "openai-compat",
            ),
            (
                "protocol = \"openai-chat\"\nbase_url = \"https://api.example.test/v1\"\n",
                "openai-compat",
            ),
        ] {
            let config = hofund_config::load(&format!(
                "[[providers]]\nid = \"custom\"\n{settings}api_key = \"test-key\"\n[providers.headers]\nx-ms-client = \"hofund\"\n"
            ))
            .expect("config");
            let provider = &config.providers[0];
            let planned = super::plan(provider).expect("plan");
            assert_eq!(
                super::plugin_for(provider, &planned, &registry()),
                plugin,
                "{settings}"
            );
            let endpoint = super::endpoint_for(provider, &planned).expect("endpoint");
            let raw = super::raw_for(provider, &planned, &endpoint, &registry()).expect("raw");
            if plugin == "azure" {
                assert_eq!(
                    raw.get("base_url"),
                    Some(&hofund_plugin_api::RawValue::String(
                        planned.base_url.clone()
                    )),
                    "{settings}"
                );
                assert!(raw.get("api_key").is_none(), "{settings}");
                assert!(raw.get("protocol").is_none(), "{settings}");
                assert!(
                    matches!(
                        raw.get("headers"),
                        Some(hofund_plugin_api::RawValue::Table(_))
                    ),
                    "{settings}"
                );
                registry()
                    .factory("azure")
                    .expect("azure")
                    .validate(&raw)
                    .expect("azure accepts the custom table");
            } else {
                assert!(raw.get("api_key").is_some(), "{settings}");
            }
        }
    }

    #[test]
    fn listed_plugins_select_each_declared_protocol_and_the_default_order() {
        let protocols = [
            hofund_plugin_api::ProtocolId::OPENAI_CHAT,
            hofund_plugin_api::ProtocolId::OPENAI_RESPONSES,
        ];
        let served = super::served_for("multi", &protocols).expect("served protocols");
        assert_eq!(served, protocols);
        let error = super::served_for("empty", &[]).expect_err("missing protocol");
        assert_eq!(error.to_string(), "plugin `empty` declares no protocol");

        for (setting, wire, protocol, base_url) in [
            (
                "protocol = \"openai-responses\"\n",
                hofund_provider::Wire::Responses,
                hofund_plugin_api::ProtocolId::OPENAI_RESPONSES,
                "https://listed-responses.example.test/v1",
            ),
            (
                "protocol = \"openai-chat\"\n",
                hofund_provider::Wire::Chat,
                hofund_plugin_api::ProtocolId::OPENAI_CHAT,
                "https://listed-chat.example.test/v1",
            ),
            (
                "",
                hofund_provider::Wire::Chat,
                hofund_plugin_api::ProtocolId::OPENAI_CHAT,
                "https://listed-chat.example.test/v1",
            ),
        ] {
            let config = hofund_config::load(&format!(
                "[[providers]]\nid = \"multi\"\npreset = \"fixture-listed-multi\"\n{setting}api_key = \"test-key\"\n"
            ))
            .expect("config");
            let planned = super::plan_with(
                &config.providers[0],
                hofund_plugin_registry::find_fixture_preset,
            )
            .expect("plan");
            assert_eq!(planned.wire, wire);
            assert_eq!(planned.protocol, Some(protocol));
            assert_eq!(planned.base_url, base_url);
        }
    }

    #[test]
    fn a_listed_plugin_rejects_an_undeclared_protocol() {
        let config = hofund_config::load(
            "[[providers]]\nid = \"multi\"\npreset = \"fixture-listed-multi\"\nprotocol = \"anthropic-messages\"\napi_key = \"test-key\"\n",
        )
        .expect("config");
        let Err(error) = super::plan_with(
            &config.providers[0],
            hofund_plugin_registry::find_fixture_preset,
        ) else {
            panic!("undeclared protocol was accepted");
        };
        assert_eq!(
            error.to_string(),
            "preset `fixture-listed-multi` is served by plugin `fixture-multi-listed`, which does not speak `anthropic-messages`; expected openai-chat, or openai-responses"
        );
    }

    struct Scratch(tempfile::TempDir);

    impl std::ops::Deref for Scratch {
        type Target = std::path::Path;

        fn deref(&self) -> &Self::Target {
            self.0.path()
        }
    }

    impl AsRef<std::path::Path> for Scratch {
        fn as_ref(&self) -> &std::path::Path {
            self.0.path()
        }
    }

    fn scratch() -> Scratch {
        Scratch(tempfile::tempdir().expect("temp dir"))
    }

    #[tokio::test]
    async fn a_listed_preset_build_receives_the_selected_protocol() {
        let config = hofund_config::load(
            "[[providers]]\nid = \"multi\"\npreset = \"fixture-listed-multi\"\nprotocol = \"openai-responses\"\napi_key = \"test-key\"\n",
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
        let dir = scratch();
        let upstream = super::build_raw_plugin(
            provider,
            "fixture-multi-listed",
            raw,
            planned.protocol,
            &endpoint.api_key,
            &dir.join("hofund.toml"),
            hofund_plugin_host::HttpLimits::new(
                std::time::Duration::from_secs(1),
                std::time::Duration::from_secs(1),
            ),
            true,
            false,
            None,
            &registry(),
        )
        .expect("build");
        let completion = upstream
            .execute(
                hofund_plugin_api::Request::new("fixture-model"),
                hofund_plugin_api::ProtocolId::OPENAI_RESPONSES,
            )
            .await
            .expect("execute");
        let hofund_plugin_api::Completion::Raw(mut body) = completion else {
            panic!("fixture returns a raw body");
        };
        let chunk = body.next_chunk().await.expect("chunk").expect("bytes");
        assert_eq!(&chunk[..], b"openai-responses");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_listed_preset_receives_the_provider_headers() {
        let config = hofund_config::load(
            "[[providers]]\nid = \"az\"\npreset = \"azure\"\nbase_url = \"my-resource\"\napi_key = \"test-key\"\n[providers.headers]\nx-ms-client = \"hofund\"\n",
        )
        .expect("config");
        let provider = &config.providers[0];
        let planned =
            super::plan_with(provider, hofund_plugin_registry::find_preset).expect("plan");
        let endpoint = super::endpoint_for(provider, &planned).expect("endpoint");
        let raw = super::raw_for_with(
            provider,
            &planned,
            &endpoint,
            hofund_plugin_registry::find_preset,
        )
        .expect("raw");
        assert_eq!(
            raw.get("base_url"),
            Some(&super::RawValue::String("my-resource".to_owned()))
        );
        let Some(super::RawValue::Table(headers)) = raw.get("headers") else {
            panic!("headers were dropped: {raw:?}");
        };
        assert_eq!(
            headers.get("x-ms-client"),
            Some(&super::RawValue::String("hofund".to_owned()))
        );
        assert!(raw.get("api_key").is_none());
        assert!(raw.get("quirks").is_none());
    }

    #[test]
    fn a_listed_plugin_without_a_selected_protocol_is_rejected() {
        let config = hofund_config::load(
            "[[providers]]\nid = \"multi\"\npreset = \"fixture-listed-multi\"\nprotocol = \"openai-responses\"\napi_key = \"test-key\"\n",
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
        let dir = scratch();
        let Err(error) = super::build_raw_plugin(
            provider,
            "fixture-multi-listed",
            raw,
            None,
            &endpoint.api_key,
            &dir.join("hofund.toml"),
            hofund_plugin_host::HttpLimits::new(
                std::time::Duration::from_secs(1),
                std::time::Duration::from_secs(1),
            ),
            true,
            false,
            None,
            &registry(),
        ) else {
            panic!("listed plugin built without a selected protocol");
        };
        assert!(
            error.to_string().contains(
                "provider `multi` has no selected protocol for plugin `fixture-multi-listed`"
            ),
            "{error}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn kernel_catalog_factories_build_through_the_legacy_path_without_a_protocol() {
        let dir = scratch();
        let plugin_id = "openai-compat";
        let source = "[[providers]]\nid = \"openai\"\npreset = \"openai\"\nprotocol = \"openai-responses\"\napi_key = \"test\"\n";
        {
            let config = hofund_config::load(source).expect("config");
            let provider = &config.providers[0];
            let planned = super::plan(provider).expect("plan");
            let endpoint = super::endpoint_for(provider, &planned).expect("endpoint");
            let registry = registry();
            let raw = super::raw_for(provider, &planned, &endpoint, &registry).expect("raw");
            super::build_raw_plugin(
                provider,
                plugin_id,
                raw,
                None,
                &endpoint.api_key,
                &dir.join("hofund.toml"),
                hofund_plugin_host::HttpLimits::new(
                    std::time::Duration::from_secs(1),
                    std::time::Duration::from_secs(1),
                ),
                true,
                false,
                None,
                &registry,
            )
            .unwrap_or_else(|error| panic!("{plugin_id} legacy build failed: {error}"));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_preset_builds_and_an_unfinished_adapter_does_not() {
        let dir = scratch();
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
        let dir = scratch();
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
        let dir = scratch();
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
        let dir = scratch();
        let path = dir.join("hofund.toml");
        std::fs::write(
            &path,
            "[[providers]]\nid = \"aws\"\npreset = \"bedrock\"\n[providers.options]\nregion = 1\n",
        )
        .expect("write");
        let error = check(&path).expect_err("region type");
        assert!(
            error
                .to_string()
                .contains("provider `aws` option `region` must be a string"),
            "{error}"
        );
        std::fs::write(
            &path,
            "[[providers]]\nid = \"vertex\"\npreset = \"google-vertex\"\n[providers.options]\nproject = 1\n",
        )
        .expect("write");
        let error = check(&path).expect_err("project type");
        assert!(
            error.to_string().contains("project: must be a string"),
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
        let dir = scratch();
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
        let dir = scratch();
        let path = dir.join("hofund.toml");
        std::fs::copy(&source, &path).expect("copy fixture");
        check(&path).expect("v0.1.0");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn echo_is_mounted_behind_auth() {
        let dir = scratch();
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
