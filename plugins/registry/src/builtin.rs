//! Built-in factories for the adapters that still live in `hofund-provider`.
//!
//! The daemon looks every wire up here. Chat, Responses, Messages, Generative,
//! Vertex, and Bedrock each validate their own option schema and then call the
//! adapter. A later pull request can move one adapter into its own plugin crate.

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
    Anthropic, Bedrock, Completion as ProviderCompletion, CountOutcome as ProviderCount,
    Credential, Endpoint, Generative, OpenAiCompat, OpenAiProtocol, Provider, ProviderError,
    Vertex,
};
use hofund_quirks::named;

/// The built-in factory for `id`, if this crate owns that id.
#[must_use]
pub fn factory(id: &str) -> Option<Arc<dyn DynUpstreamFactory>> {
    match id {
        "openai-compat" => Some(Arc::new(OpenAiFactory)),
        "anthropic" => Some(Arc::new(AnthropicFactory)),
        "generative" => Some(Arc::new(GenerativeFactory)),
        "google-vertex" => Some(Arc::new(VertexFactory)),
        "bedrock" => Some(Arc::new(BedrockFactory)),
        _ => None,
    }
}

static OPENAI_SPEC: UpstreamSpec = UpstreamSpec::from_static(
    PluginId::from_static("openai-compat"),
    &[ProtocolId::OPENAI_CHAT, ProtocolId::OPENAI_RESPONSES],
    CredentialStyle::Bearer,
    ListKind::Live,
);

static ANTHROPIC_SPEC: UpstreamSpec = UpstreamSpec::from_static(
    PluginId::from_static("anthropic"),
    &[ProtocolId::ANTHROPIC_MESSAGES],
    CredentialStyle::XApiKey,
    ListKind::Live,
);

static GENERATIVE_SPEC: UpstreamSpec = UpstreamSpec::from_static(
    PluginId::from_static("generative"),
    &[ProtocolId::GENERATIVE_LANGUAGE],
    CredentialStyle::NamedHeader(hofund_plugin_api::HeaderName::new("x-goog-api-key")),
    ListKind::Live,
);

static VERTEX_SPEC: UpstreamSpec = UpstreamSpec::from_static(
    PluginId::from_static("google-vertex"),
    &[ProtocolId::GENERATIVE_LANGUAGE],
    CredentialStyle::Bearer,
    ListKind::Catalog,
);

static BEDROCK_SPEC: UpstreamSpec = UpstreamSpec::from_static(
    PluginId::from_static("bedrock"),
    &[ProtocolId::BEDROCK_CONVERSE],
    CredentialStyle::SigV4,
    ListKind::Catalog,
);

const VERTEX_OPTIONS: &[&str] = &[
    "project",
    "location",
    "credentials",
    "impersonate",
    "token_url",
    "iam_root",
];

const BEDROCK_OPTIONS: &[&str] = &[
    "region",
    "access_key_id",
    "secret_access_key",
    "session_token",
];

struct OpenAiFactory;
struct AnthropicFactory;
struct GenerativeFactory;
struct VertexFactory;
struct BedrockFactory;

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
        option_schema(&[])
    }

    fn validate(&self, raw: &RawTable) -> Result<Self::Config, ConfigError> {
        let mut config = read_endpoint(UpstreamFactory::spec(self), raw, true, &[])?;
        config.protocol = Some(read_protocol(raw, &config.endpoint.id)?);
        Ok(config)
    }

    fn build(
        &self,
        cfg: &Self::Config,
        host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, BuildError> {
        let protocol = cfg.protocol.unwrap_or(OpenAiProtocol::Chat);
        let mut endpoint = cfg.endpoint.clone();
        attach_client(&mut endpoint, host);
        let adapter = OpenAiCompat::new(protocol, endpoint).map_err(build_error)?;
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
        option_schema(&[])
    }

    fn validate(&self, raw: &RawTable) -> Result<Self::Config, ConfigError> {
        read_endpoint(UpstreamFactory::spec(self), raw, false, &[])
    }

    fn build(
        &self,
        cfg: &Self::Config,
        host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, BuildError> {
        let mut endpoint = cfg.endpoint.clone();
        attach_client(&mut endpoint, host);
        let adapter = Anthropic::new(endpoint).map_err(build_error)?;
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
        option_schema(&[])
    }

    fn validate(&self, raw: &RawTable) -> Result<Self::Config, ConfigError> {
        read_endpoint(UpstreamFactory::spec(self), raw, false, &[])
    }

    fn build(
        &self,
        cfg: &Self::Config,
        host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, BuildError> {
        let mut endpoint = cfg.endpoint.clone();
        attach_client(&mut endpoint, host);
        let adapter = Generative::new(endpoint).map_err(build_error)?;
        Ok(Arc::new(Bridged {
            inner: Arc::new(adapter),
        }))
    }
}

struct VertexConfig {
    endpoint: Endpoint,
    credentials: String,
    impersonate: String,
    token_url: String,
    iam_root: String,
}

impl ValidConfig for VertexConfig {}

impl UpstreamFactory for VertexFactory {
    type Config = VertexConfig;

    fn spec(&self) -> &'static UpstreamSpec {
        &VERTEX_SPEC
    }

    fn config_schema(&self) -> Schema {
        option_schema(VERTEX_OPTIONS)
    }

    fn validate(&self, raw: &RawTable) -> Result<Self::Config, ConfigError> {
        let config = read_endpoint(UpstreamFactory::spec(self), raw, false, VERTEX_OPTIONS)?;
        let id = config.endpoint.id.as_str();
        Ok(VertexConfig {
            credentials: option_string(raw, "credentials", id)?,
            impersonate: option_string(raw, "impersonate", id)?,
            token_url: option_string(raw, "token_url", id)?,
            iam_root: option_string(raw, "iam_root", id)?,
            endpoint: config.endpoint,
        })
    }

    fn preflight(&self, cfg: &Self::Config) -> Result<(), ConfigError> {
        hofund_provider::check_vertex_credentials_file(&cfg.endpoint.id, &cfg.credentials)
            .map_err(|error| ConfigError::new(error.to_string()))
    }

    fn build(
        &self,
        cfg: &Self::Config,
        host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, BuildError> {
        let mut endpoint = cfg.endpoint.clone();
        attach_client(&mut endpoint, host);
        let adapter = Vertex::with_overrides(
            endpoint,
            cfg.credentials.clone(),
            cfg.impersonate.clone(),
            &cfg.token_url,
            &cfg.iam_root,
        )
        .map_err(build_error)?;
        Ok(Arc::new(Bridged {
            inner: Arc::new(adapter),
        }))
    }
}

struct BedrockConfig {
    endpoint: Endpoint,
    region: String,
    access_key_id: String,
    secret_access_key: String,
    session_token: String,
}

impl ValidConfig for BedrockConfig {}

impl UpstreamFactory for BedrockFactory {
    type Config = BedrockConfig;

    fn spec(&self) -> &'static UpstreamSpec {
        &BEDROCK_SPEC
    }

    fn config_schema(&self) -> Schema {
        option_schema(BEDROCK_OPTIONS)
    }

    fn validate(&self, raw: &RawTable) -> Result<Self::Config, ConfigError> {
        let config = read_endpoint(UpstreamFactory::spec(self), raw, false, BEDROCK_OPTIONS)?;
        let id = config.endpoint.id.as_str();
        Ok(BedrockConfig {
            region: option_string(raw, "region", id)?,
            access_key_id: option_string(raw, "access_key_id", id)?,
            secret_access_key: option_string(raw, "secret_access_key", id)?,
            session_token: option_string(raw, "session_token", id)?,
            endpoint: config.endpoint,
        })
    }

    fn build(
        &self,
        cfg: &Self::Config,
        host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, BuildError> {
        let mut endpoint = cfg.endpoint.clone();
        attach_client(&mut endpoint, host);
        let adapter = Bedrock::new(
            endpoint,
            &cfg.region,
            &cfg.access_key_id,
            &cfg.secret_access_key,
            &cfg.session_token,
        )
        .map_err(build_error)?;
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

    async fn list_models(
        &self,
        query: &hofund_plugin_api::ListQuery,
    ) -> Result<hofund_plugin_api::ModelList, PluginError> {
        self.inner.list_models(query).await
    }

    async fn probe(
        &self,
        query: &hofund_plugin_api::ProbeQuery,
    ) -> Result<hofund_plugin_api::ProbeReport, PluginError> {
        self.inner.probe(query).await
    }
}

fn reject_options(
    raw: &RawTable,
    id: &str,
    protocol: bool,
    options: &[&str],
) -> Result<(), ConfigError> {
    for key in raw.keys() {
        if is_kernel(key, protocol) {
            continue;
        }
        if options.contains(&key) {
            if !matches!(raw.get(key), Some(RawValue::String(_))) {
                return Err(ConfigError::new(format!(
                    "provider `{id}` option `{key}` must be a string"
                ))
                .with_path(key));
            }
            continue;
        }
        return Err(
            ConfigError::new(format!("provider `{id}` has unknown option `{key}`")).with_path(key),
        );
    }
    Ok(())
}

fn is_kernel(key: &str, protocol: bool) -> bool {
    matches!(
        key,
        "id" | "base_url" | "api_key" | "credential" | "quirks" | "headers"
    ) || (protocol && key == "protocol")
}

fn required_id<'a>(raw: &'a RawTable, spec: &UpstreamSpec) -> Result<&'a str, ConfigError> {
    match raw.get("id") {
        Some(RawValue::String(id)) if !id.is_empty() => Ok(id.as_str()),
        _ => Err(
            ConfigError::new(format!("`{}` provider has no `id`", spec.plugin().as_str()))
                .with_path("id"),
        ),
    }
}

fn option_schema(fields: &[&str]) -> Schema {
    let listed = fields
        .iter()
        .map(|name| ((*name).to_owned(), Schema::string()))
        .collect();
    Schema::object(listed, &[]).expect("option schema")
}

fn option_string(raw: &RawTable, key: &str, id: &str) -> Result<String, ConfigError> {
    Ok(optional_string(raw, key, id)?.unwrap_or_default())
}

fn attach_client(endpoint: &mut Endpoint, host: &HostContext) {
    let Some(pooled) = host.http().pooled_client() else {
        return;
    };
    if let Some(client) = hofund_provider::pooled_reqwest(pooled.as_ref()) {
        endpoint.client = Some(client);
    }
}

fn read_endpoint(
    spec: &'static UpstreamSpec,
    raw: &RawTable,
    protocol: bool,
    options: &[&str],
) -> Result<EndpointConfig, ConfigError> {
    let id = required_id(raw, spec)?;
    reject_options(raw, id, protocol, options)?;
    let base_url = required_string(raw, "base_url", id)?;
    let mut endpoint = Endpoint::new(id, base_url);
    if let Some(value) = optional_string(raw, "api_key", id)? {
        endpoint.api_key = value;
    }
    if let Some(value) = optional_string(raw, "credential", id)? {
        endpoint.credential = read_credential(&value)?;
    }
    if let Some(value) = optional_string(raw, "quirks", id)? {
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

fn read_protocol(raw: &RawTable, id: &str) -> Result<OpenAiProtocol, ConfigError> {
    match optional_string(raw, "protocol", id)? {
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

fn required_string(raw: &RawTable, key: &str, id: &str) -> Result<String, ConfigError> {
    match optional_string(raw, key, id)? {
        Some(value) if !value.is_empty() => Ok(value),
        _ => Err(ConfigError::new("missing string").with_path(key)),
    }
}

fn optional_string(raw: &RawTable, key: &str, id: &str) -> Result<Option<String>, ConfigError> {
    match raw.get(key) {
        None => Ok(None),
        Some(RawValue::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(ConfigError::new(format!(
            "provider `{id}` option `{key}` must be a string"
        ))
        .with_path(key)),
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

#[cfg(test)]
mod tests {
    use super::{BedrockFactory, OpenAiFactory, VertexFactory};
    use hofund_plugin_api::{RawTable, RawValue, UpstreamFactory};

    fn raw(id: &str) -> RawTable {
        let mut table = RawTable::new();
        table.insert("id", RawValue::String(id.to_owned()));
        table.insert(
            "base_url",
            RawValue::String("https://example.test".to_owned()),
        );
        table
    }

    #[test]
    fn unknown_and_mistyped_options_name_the_provider_and_hide_the_value() {
        let mut openai = raw("primary");
        openai.insert(
            "secret_access_key",
            RawValue::String("SENTINEL-OPTION-VALUE".to_owned()),
        );
        let Err(error) = UpstreamFactory::validate(&OpenAiFactory, &openai) else {
            panic!("aws key was accepted");
        };
        let text = error.to_string();
        assert!(
            text.contains("provider `primary` has unknown option `secret_access_key`"),
            "{text}"
        );
        assert!(!text.contains("SENTINEL-OPTION-VALUE"), "{text}");

        let mut vertex = raw("vertex");
        let mut nested = RawTable::new();
        nested.insert("leak", RawValue::String("SENTINEL-NESTED".to_owned()));
        vertex.insert("project", RawValue::Table(nested));
        let Err(error) = UpstreamFactory::validate(&VertexFactory, &vertex) else {
            panic!("wrong type was accepted");
        };
        let text = error.to_string();
        assert!(
            text.contains("provider `vertex` option `project` must be a string"),
            "{text}"
        );
        assert!(!text.contains("SENTINEL-NESTED"), "{text}");

        let mut bedrock = raw("aws");
        bedrock.insert("project", RawValue::String("demo-project".to_owned()));
        let Err(error) = UpstreamFactory::validate(&BedrockFactory, &bedrock) else {
            panic!("vertex key was accepted");
        };
        let text = error.to_string();
        assert!(
            text.contains("provider `aws` has unknown option `project`"),
            "{text}"
        );
        assert!(!text.contains("demo-project"), "{text}");
    }

    #[test]
    fn a_missing_empty_or_mistyped_id_is_rejected_before_options() {
        for id in [
            None,
            Some(RawValue::String(String::new())),
            Some(RawValue::Integer(1)),
        ] {
            let mut vertex = match id {
                Some(id) => {
                    let mut table = raw("temporary");
                    table.insert("id", id);
                    table
                }
                None => RawTable::new(),
            };
            vertex.insert(
                "unknown_option",
                RawValue::String("SENTINEL-UNKNOWN-OPTION".to_owned()),
            );
            let Err(error) = UpstreamFactory::validate(&VertexFactory, &vertex) else {
                panic!("invalid id was accepted");
            };
            assert!(
                error
                    .to_string()
                    .contains("`google-vertex` provider has no `id`"),
                "{error}"
            );
            assert!(!error.to_string().contains("SENTINEL-UNKNOWN-OPTION"));
        }
    }

    #[test]
    fn raw_environment_references_pass_the_shape_check() {
        let mut vertex = raw("vertex");
        vertex.insert(
            "token_url",
            RawValue::String("env:HOFUND_VERTEX_TEST_URL".to_owned()),
        );
        UpstreamFactory::validate(&VertexFactory, &vertex).expect("shape");
    }

    #[test]
    fn schemas_list_only_that_plugins_options() {
        let vertex = UpstreamFactory::config_schema(&VertexFactory);
        let names = vertex.field_names();
        assert!(names.contains(&"credentials"));
        assert!(names.contains(&"token_url"));
        assert!(!names.contains(&"access_key_id"));
        let bedrock = UpstreamFactory::config_schema(&BedrockFactory);
        let names = bedrock.field_names();
        assert!(names.contains(&"secret_access_key"));
        assert!(!names.contains(&"project"));
        assert!(
            UpstreamFactory::config_schema(&OpenAiFactory)
                .field_names()
                .is_empty()
        );
    }

    #[test]
    fn builtin_specs_advertise_live_and_static_model_lists() {
        use hofund_plugin_api::ListKind;
        for (id, kind) in [
            ("openai-compat", ListKind::Live),
            ("anthropic", ListKind::Live),
            ("generative", ListKind::Live),
            ("google-vertex", ListKind::Catalog),
            ("bedrock", ListKind::Catalog),
        ] {
            assert_eq!(
                super::factory(id).expect("factory").spec().list_kind(),
                kind,
                "{id}"
            );
        }
    }

    #[tokio::test]
    async fn bridge_forwards_model_list_and_probe_to_the_adapter() {
        use hofund_plugin_api::{ListQuery, ProbeQuery, ProtocolId, UpstreamProvider};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"id": "chat"}]
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(1)
            .mount(&server)
            .await;
        let endpoint = hofund_provider::Endpoint::new("test", format!("{}/v1", server.uri()));
        let adapter =
            hofund_provider::OpenAiCompat::new(hofund_provider::OpenAiProtocol::Chat, endpoint)
                .expect("adapter");
        let bridged = super::Bridged {
            inner: std::sync::Arc::new(adapter),
        };
        let listed = bridged.list_models(&ListQuery::new()).await.expect("list");
        assert_eq!(listed.models()[0].id(), "chat");
        let query = ProbeQuery::new(ProtocolId::OPENAI_CHAT, "chat").expect("query");
        bridged.probe(&query).await.expect("probe");
        assert_eq!(server.received_requests().await.expect("requests").len(), 2);
    }
}
