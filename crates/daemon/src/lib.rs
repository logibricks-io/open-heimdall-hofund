//! Composition root for the local gateway process.
//!
//! This crate reads configuration, resolves credentials, builds providers, and
//! starts the HTTP server. The binary parses arguments and calls these
//! functions.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hofund_codec::{AnthropicMessages, Codec, GenerativeLanguage, OpenAiChat, OpenAiResponses};
use hofund_config::{Config, Kind, PresetWire, Protocol, load_path};
use hofund_gateway::{GENERATIVE_STREAM, GENERATIVE_UNARY, Gateway};

pub use hofund_gateway::Running;
use hofund_provider::{
    Anthropic, Bedrock, Credential, Endpoint, Generative, OpenAiCompat, OpenAiProtocol, Provider,
    Vertex, Wire, resolve,
};
use hofund_quirks::named;
use hofund_router::{FailoverPolicy, Snapshot};
use hofund_secrets::TokenFile;
use hofund_usage::Ledger;

mod membership;

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
    let config = load_path(path).map_err(show)?;
    let token_path = beside(path, &config.token_file);
    let token = TokenFile::new(&token_path).load_or_create().map_err(show)?;
    let usage = Arc::new(Ledger::open(&beside(path, &config.usage_file)).map_err(show)?);
    let gateway =
        Gateway::with_token_file(router(&config)?, token, token_path, inbound_codecs(), usage);
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

fn router(config: &Config) -> Result<Snapshot, DaemonError> {
    let built = providers(config)?;
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
    membership::open(&built, routes, policy)
}

fn providers(config: &Config) -> Result<BTreeMap<String, membership::BuiltProvider>, DaemonError> {
    let mut built = BTreeMap::new();
    for provider in &config.providers {
        let planned = plan(provider)?;
        let mut endpoint = Endpoint::new(provider.id.clone(), planned.base_url);
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
        let timeout = endpoint.timeout;
        let protocol = protocol_of(planned.wire);
        let adapter: Arc<dyn Provider> = match planned.wire {
            Wire::Chat => {
                Arc::new(OpenAiCompat::new(OpenAiProtocol::Chat, endpoint).map_err(show)?)
            }
            Wire::Responses => {
                Arc::new(OpenAiCompat::new(OpenAiProtocol::Responses, endpoint).map_err(show)?)
            }
            Wire::Messages => Arc::new(Anthropic::new(endpoint).map_err(show)?),
            Wire::Generative => Arc::new(Generative::new(endpoint).map_err(show)?),
            Wire::Vertex => Arc::new(
                Vertex::new(endpoint, &provider.credentials, &provider.impersonate)
                    .map_err(show)?,
            ),
            Wire::Converse => {
                let access_key_id =
                    hofund_secrets::resolve(&provider.access_key_id).map_err(show)?;
                let secret_access_key =
                    hofund_secrets::resolve(&provider.secret_access_key).map_err(show)?;
                let session_token = if provider.session_token.is_empty() {
                    String::new()
                } else {
                    hofund_secrets::resolve(&provider.session_token).map_err(show)?
                };
                Arc::new(
                    Bedrock::new(
                        endpoint,
                        provider.region.as_deref().unwrap_or(""),
                        &access_key_id,
                        &secret_access_key,
                        &session_token,
                    )
                    .map_err(show)?,
                )
            }
        };
        built.insert(
            provider.id.clone(),
            membership::BuiltProvider {
                adapter,
                protocol,
                timeout,
                plugin_id: implementation_id(planned.wire).to_owned(),
                preset_id: provider.preset.clone().unwrap_or_default(),
            },
        );
    }
    Ok(built)
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

struct Planned {
    base_url: String,
    wire: Wire,
    credential: Credential,
}

fn plan(provider: &hofund_config::Provider) -> Result<Planned, DaemonError> {
    if let Some(preset) = &provider.preset {
        let resolved = resolve(&hofund_provider::Resolve {
            id: preset,
            region: provider.region.as_deref(),
            wire: provider.preset_wire.map(|wire| match wire {
                PresetWire::Chat => Wire::Chat,
                PresetWire::Responses => Wire::Responses,
                PresetWire::Messages => Wire::Messages,
                PresetWire::Generative => Wire::Generative,
                PresetWire::Converse => Wire::Converse,
            }),
            base_url: &provider.base_url,
            project: &provider.project,
            location: &provider.location,
        })
        .map_err(show)?;
        return Ok(Planned {
            base_url: resolved.base_url,
            wire: resolved.wire,
            credential: resolved.credential,
        });
    }
    let kind = provider.kind.ok_or_else(|| {
        DaemonError::new(format!(
            "provider `{}` needs a kind or a preset",
            provider.id
        ))
    })?;
    let (wire, credential) = match kind {
        Kind::OpenAiCompat { protocol } => {
            let wire = match protocol {
                Protocol::Chat => Wire::Chat,
                Protocol::Responses => Wire::Responses,
            };
            (wire, Credential::Default)
        }
        Kind::Anthropic => (Wire::Messages, Credential::Default),
        Kind::Generative => (Wire::Generative, Credential::Default),
        Kind::Converse => (Wire::Converse, Credential::Default),
    };
    Ok(Planned {
        base_url: provider.base_url.clone(),
        wire,
        credential,
    })
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
            "[[providers]]\nid = \"gemini\"\npreset = \"google\"\nprotocol = \"chat\"\napi_key = \"test\"\n",
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
}
