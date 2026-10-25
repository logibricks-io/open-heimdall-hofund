//! Built-in factories for Chat, Responses, Messages, and Generative.
//!
//! These still run the `hofund-provider` adapters. The daemon looks them up
//! here, so there is one factory path. Pull request E replaces each factory
//! with a plugin. Vertex and Bedrock are not in this module.

use std::sync::Arc;

use futures_util::StreamExt;

use hofund_plugin_api::{
    BuildError, Completion as PluginCompletion, ConfigError, CountOutcome as PluginCount,
    CredentialStyle, DynUpstreamFactory, HostContext, ListKind, PluginError, PluginId, ProtocolId,
    RawTable, RawValue, Request, Schema, UpstreamFactory, UpstreamProvider, UpstreamSpec,
    ValidConfig,
};
use hofund_plugin_sdk::async_trait;
use hofund_provider::{
    Anthropic, Completion as ProviderCompletion, CountOutcome as ProviderCount, Credential,
    Endpoint, Generative, OpenAiCompat, OpenAiProtocol, Provider, ProviderError,
};
use hofund_quirks::named;

/// The built-in factory for `id`, if this crate owns that id.
#[must_use]
pub fn factory(id: &str) -> Option<Arc<dyn DynUpstreamFactory>> {
    match id {
        "openai-compat" => Some(Arc::new(OpenAiFactory)),
        "anthropic" => Some(Arc::new(AnthropicFactory)),
        "generative" => Some(Arc::new(GenerativeFactory)),
        _ => None,
    }
}

static OPENAI_SPEC: UpstreamSpec = UpstreamSpec::from_static(
    PluginId::from_static("openai-compat"),
    &[ProtocolId::OPENAI_CHAT, ProtocolId::OPENAI_RESPONSES],
    CredentialStyle::Bearer,
    ListKind::Unsupported,
);

static ANTHROPIC_SPEC: UpstreamSpec = UpstreamSpec::from_static(
    PluginId::from_static("anthropic"),
    &[ProtocolId::ANTHROPIC_MESSAGES],
    CredentialStyle::XApiKey,
    ListKind::Unsupported,
);

static GENERATIVE_SPEC: UpstreamSpec = UpstreamSpec::from_static(
    PluginId::from_static("generative"),
    &[ProtocolId::GENERATIVE_LANGUAGE],
    CredentialStyle::NamedHeader(hofund_plugin_api::HeaderName::new("x-goog-api-key")),
    ListKind::Unsupported,
);

struct OpenAiFactory;
struct AnthropicFactory;
struct GenerativeFactory;

struct EndpointConfig {
    endpoint: Endpoint,
    protocol: Option<OpenAiProtocol>,
}

impl ValidConfig for EndpointConfig {}

impl UpstreamFactory for OpenAiFactory {
    type Config = EndpointConfig;

    fn spec(&self) -> &'static UpstreamSpec {
        &OPENAI_SPEC
    }

    fn config_schema(&self) -> Schema {
        Schema::empty()
    }

    fn validate(&self, raw: &RawTable) -> Result<Self::Config, ConfigError> {
        let mut config = read_endpoint(raw, true)?;
        config.protocol = Some(read_protocol(raw)?);
        Ok(config)
    }

    fn build(
        &self,
        cfg: &Self::Config,
        _host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, BuildError> {
        let protocol = cfg.protocol.unwrap_or(OpenAiProtocol::Chat);
        let adapter = OpenAiCompat::new(protocol, cfg.endpoint.clone()).map_err(build_error)?;
        Ok(Arc::new(Bridged {
            inner: Arc::new(adapter),
        }))
    }
}

impl UpstreamFactory for AnthropicFactory {
    type Config = EndpointConfig;

    fn spec(&self) -> &'static UpstreamSpec {
        &ANTHROPIC_SPEC
    }

    fn config_schema(&self) -> Schema {
        Schema::empty()
    }

    fn validate(&self, raw: &RawTable) -> Result<Self::Config, ConfigError> {
        read_endpoint(raw, false)
    }

    fn build(
        &self,
        cfg: &Self::Config,
        _host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, BuildError> {
        let adapter = Anthropic::new(cfg.endpoint.clone()).map_err(build_error)?;
        Ok(Arc::new(Bridged {
            inner: Arc::new(adapter),
        }))
    }
}

impl UpstreamFactory for GenerativeFactory {
    type Config = EndpointConfig;

    fn spec(&self) -> &'static UpstreamSpec {
        &GENERATIVE_SPEC
    }

    fn config_schema(&self) -> Schema {
        Schema::empty()
    }

    fn validate(&self, raw: &RawTable) -> Result<Self::Config, ConfigError> {
        read_endpoint(raw, false)
    }

    fn build(
        &self,
        cfg: &Self::Config,
        _host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, BuildError> {
        let adapter = Generative::new(cfg.endpoint.clone()).map_err(build_error)?;
        Ok(Arc::new(Bridged {
            inner: Arc::new(adapter),
        }))
    }
}

struct Bridged {
    inner: Arc<dyn Provider>,
}

#[async_trait]
impl UpstreamProvider for Bridged {
    async fn execute(
        &self,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<PluginCompletion, PluginError> {
        let completion = self
            .inner
            .execute(request, inbound.as_str())
            .await
            .map_err(plugin_error)?;
        Ok(plugin_completion(completion))
    }

    async fn count_input(
        &self,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<PluginCount, PluginError> {
        let outcome = self
            .inner
            .count_input(request, inbound.as_str())
            .await
            .map_err(plugin_error)?;
        Ok(match outcome {
            ProviderCount::Counted(tokens) => PluginCount::Counted(tokens),
            ProviderCount::Unsupported => PluginCount::Unsupported,
        })
    }
}

fn read_endpoint(raw: &RawTable, protocol: bool) -> Result<EndpointConfig, ConfigError> {
    for key in raw.keys() {
        let allowed = matches!(
            key,
            "id" | "base_url" | "api_key" | "credential" | "quirks" | "headers"
        ) || (protocol && key == "protocol");
        if !allowed {
            return Err(ConfigError::new("unknown key").with_path(key));
        }
    }
    let id = required_string(raw, "id")?;
    let base_url = required_string(raw, "base_url")?;
    let mut endpoint = Endpoint::new(id, base_url);
    if let Some(value) = optional_string(raw, "api_key")? {
        endpoint.api_key = value;
    }
    if let Some(value) = optional_string(raw, "credential")? {
        endpoint.credential = read_credential(&value)?;
    }
    if let Some(value) = optional_string(raw, "quirks")? {
        endpoint.quirks = Some(
            named(&value)
                .ok_or_else(|| ConfigError::new("unknown quirk profile").with_path("quirks"))?,
        );
    }
    endpoint.headers = read_headers(raw)?;
    Ok(EndpointConfig {
        endpoint,
        protocol: None,
    })
}

fn read_protocol(raw: &RawTable) -> Result<OpenAiProtocol, ConfigError> {
    match optional_string(raw, "protocol")? {
        Some(value) if value == "chat" => Ok(OpenAiProtocol::Chat),
        Some(value) if value == "responses" => Ok(OpenAiProtocol::Responses),
        Some(_) => {
            Err(ConfigError::new("protocol must be chat or responses").with_path("protocol"))
        }
        None => Err(ConfigError::new("protocol is required").with_path("protocol")),
    }
}

fn read_credential(value: &str) -> Result<Credential, ConfigError> {
    match value {
        "default" => Ok(Credential::Default),
        "bearer" => Ok(Credential::Bearer),
        "api-key" => Ok(Credential::ApiKey),
        "x-api-key" => Ok(Credential::XApiKey),
        "messages-both" => Ok(Credential::MessagesBoth),
        "goog-api-key" => Ok(Credential::GoogApiKey),
        _ => Err(ConfigError::new("unknown credential").with_path("credential")),
    }
}

fn read_headers(raw: &RawTable) -> Result<Vec<(String, String)>, ConfigError> {
    let Some(value) = raw.get("headers") else {
        return Ok(Vec::new());
    };
    let RawValue::Table(table) = value else {
        return Err(ConfigError::new("headers must be a table").with_path("headers"));
    };
    let mut headers = Vec::new();
    for (name, value) in table.iter() {
        let RawValue::String(text) = value else {
            return Err(ConfigError::new("header values must be strings").with_path("headers"));
        };
        headers.push((name.to_owned(), text.clone()));
    }
    Ok(headers)
}

fn required_string(raw: &RawTable, key: &str) -> Result<String, ConfigError> {
    match optional_string(raw, key)? {
        Some(value) if !value.is_empty() => Ok(value),
        _ => Err(ConfigError::new("missing string").with_path(key)),
    }
}

fn optional_string(raw: &RawTable, key: &str) -> Result<Option<String>, ConfigError> {
    match raw.get(key) {
        None => Ok(None),
        Some(RawValue::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(ConfigError::new("must be a string").with_path(key)),
    }
}

fn build_error(error: ProviderError) -> BuildError {
    BuildError::new(error.to_string())
}

fn plugin_error(error: ProviderError) -> PluginError {
    match error.kind {
        hofund_provider::ProviderErrorKind::Connect => PluginError::connect(error.message),
        hofund_provider::ProviderErrorKind::Timeout => PluginError::timeout(error.message),
        hofund_provider::ProviderErrorKind::Status => {
            PluginError::status_error(error.status.unwrap_or(502), error.message)
        }
        hofund_provider::ProviderErrorKind::Decode => PluginError::decode(error.message),
    }
}

fn plugin_completion(completion: ProviderCompletion) -> PluginCompletion {
    match completion {
        ProviderCompletion::Unary(response) => PluginCompletion::Unary(response),
        ProviderCompletion::Stream(stream) => {
            PluginCompletion::Stream(Box::pin(stream.map(|item| item.map_err(plugin_error))))
        }
        ProviderCompletion::Raw(stream) => {
            PluginCompletion::Raw(hofund_plugin_api::BodyStream::from_stream(
                stream.map(|item| item.map_err(plugin_error)),
            ))
        }
    }
}
