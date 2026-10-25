//! Composition root for the local gateway process.
//!
//! This crate reads configuration, resolves credentials, builds providers, and
//! starts the HTTP server. The binary parses arguments and calls these
//! functions.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hofund_codec::{AnthropicMessages, Codec, GenerativeLanguage, OpenAiChat, OpenAiResponses};
use hofund_config::{Config, load_path};
use hofund_gateway::{GENERATIVE_STREAM, GENERATIVE_UNARY, Gateway};
use hofund_plugin_api::{DynUpstreamFactory, PluginId, ProtocolId, RawTable, RawValue};
use hofund_plugin_host::HttpLimits;

pub use hofund_gateway::Running;
use hofund_provider::{Bedrock, Credential, Endpoint, Provider, Vertex, Wire, resolve};
use hofund_quirks::named;
use hofund_router::{FailoverPolicy, Snapshot};
use hofund_secrets::TokenFile;
use hofund_usage::Ledger;

mod adapt;
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
    warn(&config);
    let _ = providers(&config, path)?;
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
    warn(&config);
    let token_path = beside(path, &config.token_file);
    let token = TokenFile::new(&token_path).load_or_create().map_err(show)?;
    let usage = Arc::new(Ledger::open(&beside(path, &config.usage_file)).map_err(show)?);
    let gateway = Gateway::with_token_file(
        router(&config, path)?,
        token,
        token_path,
        inbound_codecs(),
        usage,
    )
    .with_surfaces(hofund_plugin_registry::surfaces());
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

fn router(config: &Config, path: &Path) -> Result<Snapshot, DaemonError> {
    let built = providers(config, path)?;
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

fn providers(
    config: &Config,
    path: &Path,
) -> Result<BTreeMap<String, membership::BuiltProvider>, DaemonError> {
    let limits = HttpLimits::new(config.connect_timeout, config.read_timeout);
    let mut built = BTreeMap::new();
    for provider in &config.providers {
        let planned = plan(provider)?;
        let mut endpoint = Endpoint::new(provider.id.clone(), planned.base_url.clone());
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
        let plugin_id = plugin_for(provider, planned.wire);
        let adapter: Arc<dyn Provider> = match planned.wire {
            Wire::Chat | Wire::Responses | Wire::Messages | Wire::Generative => {
                Arc::new(build_plugin(provider, &planned, &endpoint, path, limits)?)
            }
            Wire::Vertex => Arc::new(
                Vertex::new(
                    endpoint,
                    option(provider, "credentials"),
                    option(provider, "impersonate"),
                )
                .map_err(show)?,
            ),
            Wire::Converse => {
                let access_key_id =
                    hofund_secrets::resolve(option(provider, "access_key_id")).map_err(show)?;
                let secret_access_key =
                    hofund_secrets::resolve(option(provider, "secret_access_key")).map_err(show)?;
                let session_token = option(provider, "session_token");
                let session_token = if session_token.is_empty() {
                    String::new()
                } else {
                    hofund_secrets::resolve(session_token).map_err(show)?
                };
                Arc::new(
                    Bedrock::new(
                        endpoint,
                        option(provider, "region"),
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
                plugin_id,
                preset_id: provider.preset.clone().unwrap_or_default(),
            },
        );
    }
    Ok(built)
}

fn plugin_for(provider: &hofund_config::Provider, wire: Wire) -> String {
    if let Some(preset) = &provider.preset
        && let Some(compiled) = hofund_plugin_registry::preset(preset)
    {
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
) -> Result<adapt::ProviderAdapter, DaemonError> {
    let plugin_id = plugin_for(provider, planned.wire);
    let factory = hofund_plugin_registry::factory(&plugin_id)
        .ok_or_else(|| DaemonError::new(format!("no factory for plugin `{plugin_id}`")))?;
    let raw = if let Some(preset) = &provider.preset
        && let Some(compiled) = hofund_plugin_registry::preset(preset)
    {
        raw_from_preset(compiled, provider)
    } else {
        raw_from_endpoint(provider, planned, endpoint)
    };
    let plugin = PluginId::try_from(plugin_id.as_str()).map_err(show)?;
    let host = hofund_plugin_host::runtime_host(
        plugin,
        endpoint.api_key.clone(),
        beside(path, ".hofund"),
        limits,
    )
    .map_err(show)?;
    let upstream = factory.validate_and_build(&raw, &host).map_err(show)?;
    Ok(adapt::ProviderAdapter::new(provider.id.clone(), upstream))
}

fn raw_from_preset(
    preset: &hofund_plugin_registry::CompiledPreset,
    provider: &hofund_config::Provider,
) -> RawTable {
    let mut raw = RawTable::new();
    for (key, value) in preset.config {
        raw.insert((*key).to_owned(), RawValue::String((*value).to_owned()));
    }
    if !provider.base_url.is_empty() {
        raw.insert("base_url", RawValue::String(provider.base_url.clone()));
    }
    raw
}

fn raw_from_endpoint(
    provider: &hofund_config::Provider,
    planned: &Planned,
    endpoint: &Endpoint,
) -> RawTable {
    let mut raw = RawTable::new();
    raw.insert("id", RawValue::String(provider.id.clone()));
    raw.insert("base_url", RawValue::String(endpoint.base_url.clone()));
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
    raw
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

struct Planned {
    base_url: String,
    wire: Wire,
    credential: Credential,
}

fn plan(provider: &hofund_config::Provider) -> Result<Planned, DaemonError> {
    let selected = match provider.protocol.as_deref() {
        Some(protocol) => Some(wire_of_protocol(protocol)?),
        None => None,
    };
    if let Some(preset) = &provider.preset
        && let Some(compiled) = hofund_plugin_registry::preset(preset)
    {
        let wire = wire_of_plugin(compiled.plugin)?;
        if let Some(selected) = selected
            && selected != wire
        {
            return Err(DaemonError::new(format!(
                "provider `{}` protocol does not match plugin `{}`",
                provider.id, compiled.plugin
            )));
        }
        return Ok(Planned {
            base_url: provider.base_url.clone(),
            wire,
            credential: Credential::Default,
        });
    }
    if let Some(preset) = &provider.preset {
        let resolved = resolve(&hofund_provider::Resolve {
            id: preset,
            region: option_present(provider, "region"),
            wire: selected,
            base_url: &provider.base_url,
            project: option(provider, "project"),
            location: option(provider, "location"),
        })
        .map_err(show)?;
        return Ok(Planned {
            base_url: resolved.base_url,
            wire: resolved.wire,
            credential: resolved.credential,
        });
    }
    let wire = selected.ok_or_else(|| {
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

/// Option keys the unmigrated Vertex and Bedrock constructors still read.
/// Config does not name them. E deletes this once those plugins validate
/// `options` themselves.
fn option<'a>(provider: &'a hofund_config::Provider, key: &str) -> &'a str {
    provider.options.get(key).map(String::as_str).unwrap_or("")
}

fn option_present<'a>(provider: &'a hofund_config::Provider, key: &str) -> Option<&'a str> {
    match option(provider, key) {
        "" => None,
        value => Some(value),
    }
}

fn wire_of_protocol(protocol: &str) -> Result<Wire, DaemonError> {
    let id = ProtocolId::try_from(protocol)
        .map_err(|_| DaemonError::new(format!("unknown protocol id `{protocol}`")))?;
    Ok(match id {
        ProtocolId::OPENAI_CHAT => Wire::Chat,
        ProtocolId::OPENAI_RESPONSES => Wire::Responses,
        ProtocolId::ANTHROPIC_MESSAGES => Wire::Messages,
        ProtocolId::GENERATIVE_LANGUAGE => Wire::Generative,
        ProtocolId::BEDROCK_CONVERSE => Wire::Converse,
        _ => {
            return Err(DaemonError::new(format!(
                "unknown protocol id `{protocol}`"
            )));
        }
    })
}

fn warn(config: &Config) {
    for warning in &config.warnings {
        eprintln!("warning: {warning}");
    }
}

fn wire_of_plugin(plugin: &str) -> Result<Wire, DaemonError> {
    let factory = hofund_plugin_registry::factory(plugin)
        .ok_or_else(|| DaemonError::new(format!("preset plugin `{plugin}` has no factory")))?;
    let Some(protocol) = DynUpstreamFactory::spec(factory.as_ref())
        .protocols()
        .first()
        .copied()
    else {
        return Err(DaemonError::new(format!(
            "plugin `{plugin}` declares no protocol"
        )));
    };
    Ok(match protocol {
        ProtocolId::OPENAI_CHAT => Wire::Chat,
        ProtocolId::OPENAI_RESPONSES => Wire::Responses,
        ProtocolId::ANTHROPIC_MESSAGES => Wire::Messages,
        ProtocolId::GENERATIVE_LANGUAGE => Wire::Generative,
        ProtocolId::BEDROCK_CONVERSE => Wire::Converse,
        _ => {
            return Err(DaemonError::new(format!(
                "plugin `{plugin}` speaks an unknown protocol"
            )));
        }
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
