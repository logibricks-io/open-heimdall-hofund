//! One-shot control for a CLI with no running daemon.
//!
//! [`serve_one_shot`] builds the same control state the daemon serves, from
//! the config on disk, and answers the same control routes on one trusted
//! connection: the standard input and output of `hofundd one-shot`, started by
//! the CLI. It never takes the daemon lock, binds a listener, writes
//! discovery, or starts the watcher and background loops. Config writes still
//! take the `<resolved-config>.lock` sidecar lock (or its runtime fallback)
//! and recheck the expected revision, and model host-state writes (picks,
//! refresh) take the per-provider `<state-file>.lock` and reread the state
//! under it, so they are safe beside other one-shot processes.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use hofund_config::ConfigError;
use hofund_gateway::control::{Control, ControlState};
use hofund_usage::Ledger;

use super::config_ops::{ConfigOps, ModelListener};
use super::control::{self, DaemonControl};
use super::control_local::{self, ControlRuntime};
use super::events::EventBus;
use super::{DaemonError, DaemonRegistry, ModelsHost, beside, runtime, show, warn};

/// Serves control requests for `path` on standard input and output until the
/// CLI closes its end.
///
/// # Errors
///
/// Returns [`DaemonError`] when the control state cannot be built (see
/// [`serve_one_shot`]) or the connection fails.
pub async fn one_shot_stdio(
    path: &Path,
    registry: Arc<dyn DaemonRegistry>,
) -> Result<(), DaemonError> {
    let io = tokio::io::join(tokio::io::stdin(), tokio::io::stdout());
    serve_one_shot(path, registry, None, io).await
}

/// Serves control requests for `path` on `io` until the peer closes it.
///
/// `control_runtime` names the runtime roots for the config-lock fallback;
/// `None` reads the environment.
///
/// # Errors
///
/// Returns [`DaemonError`] when the config cannot be read or loaded, the
/// token, usage ledger, or provider snapshot cannot be prepared, or the
/// connection fails.
pub async fn serve_one_shot<IO>(
    path: &Path,
    registry: Arc<dyn DaemonRegistry>,
    control_runtime: Option<ControlRuntime>,
    io: IO,
) -> Result<(), DaemonError>
where
    IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (state, token, token_path) = state(path, registry, control_runtime).await?;
    Control::with_token_file(state, token, token_path)
        .serve_trusted(io)
        .await
        .map_err(|error| DaemonError::new(format!("one-shot control connection: {error}")))
}

async fn state(
    path: &Path,
    registry: Arc<dyn DaemonRegistry>,
    control_runtime: Option<ControlRuntime>,
) -> Result<(Arc<dyn ControlState>, String, PathBuf), DaemonError> {
    let started_at_ms = control::unix_milliseconds();
    let config_text = std::fs::read_to_string(path)
        .map_err(|error| show(ConfigError::Read(error.to_string())))?;
    let config = hofund_config::load(&config_text).map_err(show)?;
    let runtime_revision = control::revision(config_text.as_bytes());
    let control_runtime = control_runtime.unwrap_or_else(ControlRuntime::from_env);
    let identity = control_local::ConfigIdentity::resolve(path).map_err(DaemonError::new)?;
    warn(&config);
    let token_path = beside(path, &config.token_file);
    let token = hofund_secrets::TokenFile::new(&token_path)
        .load_or_create()
        .map_err(show)?;
    let usage = Arc::new(Ledger::open(&beside(path, &config.usage_file)).map_err(show)?);
    let runtime = runtime::Runtime::new(Arc::clone(&registry), path, None);
    let mut models_host = ModelsHost::new(config.connect_timeout, config.read_timeout)
        .with_state_root(beside(path, ".hofund"))
        .with_shared_state(control_local::runtime_lock_dir(&control_runtime))
        .with_registry(Arc::clone(&registry));
    let snapshot = runtime.initial_snapshot(&config, Some(&mut models_host))?;
    runtime.refresh_subscriptions().await;
    let cooldowns = snapshot
        .load()
        .cooldowns()
        .cloned()
        .expect("daemon router has shared cooldowns");
    let env = hofund_config::addr_env();
    let (listen, source) =
        hofund_config::effective_listen(&config, env.as_deref()).map_err(show)?;
    let models_host = Arc::new(models_host);
    runtime.attach_models(Arc::clone(&models_host));
    let events = Arc::new(EventBus::new());
    let (fatal, _fatal_reported) = tokio::sync::watch::channel(None);
    let listener = ModelListener::new(
        (source == hofund_config::ListenSource::Environment).then_some(listen),
        None,
        fatal,
    );
    let ops = Arc::new(ConfigOps::new(
        path,
        control_local::config_lock_fallback(&identity, &control_runtime),
        Arc::clone(&runtime),
        Arc::clone(&events),
        Arc::new(config),
        runtime_revision,
        listener,
    ));
    let (shutdown, _) = tokio::sync::watch::channel(false);
    let state: Arc<dyn ControlState> = Arc::new(
        DaemonControl::new(
            path,
            started_at_ms,
            models_host,
            ops,
            registry,
            cooldowns,
            usage,
            token_path.clone(),
            shutdown,
        )
        .with_events(events),
    );
    Ok((state, token, token_path))
}
