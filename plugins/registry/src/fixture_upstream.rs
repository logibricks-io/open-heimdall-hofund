use std::sync::Arc;

use hofund_plugin_api::{
    BodyStream, BuildContext, BuildError, Completion, ConfigError, CredentialStyle, HostContext,
    ListKind, PluginError, PluginId, ProtocolId, RawTable, RawValue, Request, Schema,
    UpstreamFactory, UpstreamProvider, UpstreamSpec, ValidConfig,
};
use hofund_plugin_sdk::Contribution;

pub(crate) fn contribute() -> Contribution {
    Contribution::new().upstream(MultiFactory)
}

static SPEC: UpstreamSpec = UpstreamSpec::from_static(
    PluginId::from_static("fixture-multi-listed"),
    &[ProtocolId::OPENAI_CHAT, ProtocolId::OPENAI_RESPONSES],
    CredentialStyle::Bearer,
    ListKind::Unsupported,
);

struct MultiFactory;
struct MultiConfig;

impl ValidConfig for MultiConfig {}

impl UpstreamFactory for MultiFactory {
    type Config = MultiConfig;

    fn spec(&self) -> &'static UpstreamSpec {
        &SPEC
    }

    fn config_schema(&self) -> Schema {
        Schema::object(
            vec![("base_url".to_owned(), Schema::string())],
            &["base_url"],
        )
        .expect("fixture schema")
    }

    fn validate(&self, raw: &RawTable) -> Result<Self::Config, ConfigError> {
        for key in raw.keys() {
            if key != "base_url" {
                return Err(ConfigError::new("unknown key").with_path(key));
            }
        }
        match raw.get("base_url") {
            Some(RawValue::String(_)) => Ok(MultiConfig),
            Some(_) => Err(ConfigError::new("base_url must be a string").with_path("base_url")),
            None => Err(ConfigError::new("base_url is required").with_path("base_url")),
        }
    }

    fn build(
        &self,
        _cfg: &Self::Config,
        _host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, BuildError> {
        Ok(Arc::new(MultiProvider {
            protocol: ProtocolId::OPENAI_CHAT,
        }))
    }

    fn build_with(
        &self,
        _cfg: &Self::Config,
        _host: &HostContext,
        ctx: &BuildContext,
    ) -> Result<Arc<dyn UpstreamProvider>, BuildError> {
        Ok(Arc::new(MultiProvider {
            protocol: ctx.protocol(),
        }))
    }
}

struct MultiProvider {
    protocol: ProtocolId,
}

#[hofund_plugin_sdk::async_trait]
impl UpstreamProvider for MultiProvider {
    async fn execute(
        &self,
        _request: Request,
        _inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        Ok(Completion::Raw(BodyStream::from_bytes(
            self.protocol.as_str().as_bytes(),
        )))
    }
}
