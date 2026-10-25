//! Composition root for the local gateway process.
//!
//! This crate reads configuration, resolves credentials, builds providers, and
//! starts the HTTP server. The binary parses arguments and calls these
//! functions.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hofund_codec::{AnthropicMessages, Codec, GenerativeLanguage, OpenAiChat, OpenAiResponses};
use hofund_config::{Config, Kind, Protocol, load_path};
use hofund_gateway::{GENERATIVE_STREAM, GENERATIVE_UNARY, Gateway};

pub use hofund_gateway::Running;
use hofund_provider::{Anthropic, Endpoint, OpenAiCompat, OpenAiProtocol, Provider};
use hofund_quirks::named;
use hofund_router::{FailoverPolicy, Router};
use hofund_secrets::{TokenFile, resolve};
use hofund_usage::Ledger;

/// A configuration, credential, or startup failure.
#[derive(Debug)]
pub struct DaemonError {
    message: String,
}

impl DaemonError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
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
    let _ = providers(&config)?;
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
    let config = load_path(path).map_err(show)?;
    let token = TokenFile::new(beside(path, &config.token_file))
        .load_or_create()
        .map_err(show)?;
    let usage = Arc::new(Ledger::open(&beside(path, &config.usage_file)).map_err(show)?);
    let gateway = Gateway::new(router(&config)?, token, inbound_codecs(), usage);
    gateway
        .bind(config.listen)
        .await
        .map_err(|error| DaemonError::new(error.to_string()))
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

fn router(config: &Config) -> Result<Router, DaemonError> {
    let built = providers(config)?;
    let mut routes = BTreeMap::new();
    for route in &config.routes {
        let mut list = Vec::new();
        for id in &route.upstreams {
            let provider = built.get(id).ok_or_else(|| {
                DaemonError::new(format!(
                    "route `{}` names unknown provider `{id}`",
                    route.model
                ))
            })?;
            list.push(Arc::clone(provider));
        }
        routes.insert(route.model.clone(), list);
    }
    let policy = FailoverPolicy {
        connect_errors: config.failover.connect_errors,
        statuses: config.failover.statuses.clone(),
    };
    Ok(Router::new(routes, policy))
}

fn providers(config: &Config) -> Result<BTreeMap<String, Arc<dyn Provider>>, DaemonError> {
    let mut built = BTreeMap::new();
    for provider in &config.providers {
        let mut endpoint = Endpoint::new(provider.id.clone(), provider.base_url.clone());
        endpoint.api_key = resolve(&provider.api_key).map_err(show)?;
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
        let adapter: Arc<dyn Provider> = match provider.kind {
            Kind::OpenAiCompat { protocol } => {
                let protocol = match protocol {
                    Protocol::Chat => OpenAiProtocol::Chat,
                    Protocol::Responses => OpenAiProtocol::Responses,
                };
                Arc::new(OpenAiCompat::new(protocol, endpoint).map_err(show)?)
            }
            Kind::Anthropic => Arc::new(Anthropic::new(endpoint).map_err(show)?),
        };
        built.insert(provider.id.clone(), adapter);
    }
    Ok(built)
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
