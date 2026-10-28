//! `BuildContext` delivery through `UpstreamFactory::build_with` and the
//! erased `DynUpstreamFactory::validate_and_build_with` entry point.

use std::sync::Arc;

use async_trait::async_trait;
use hofund_kit_upstream::{block_on, test_host};
use hofund_plugin_api::{
    BodyStream, BuildContext, BuildError, Completion, ConfigError, CredentialStyle,
    DynUpstreamFactory, HostContext, ListKind, PluginError, PluginId, ProtocolId, RawTable,
    Request, Schema, UpstreamFactory, UpstreamProvider, UpstreamSpec, ValidConfig,
};

static SPEC: UpstreamSpec = UpstreamSpec::from_static(
    PluginId::from_static("multi"),
    &[ProtocolId::OPENAI_CHAT, ProtocolId::OPENAI_RESPONSES],
    CredentialStyle::Bearer,
    ListKind::Unsupported,
);

struct TestConfig;

impl ValidConfig for TestConfig {}

/// Declares two protocols and keeps the default `build_with`.
struct DefaultFactory;

/// Declares two protocols and overrides `build_with`.
struct OverridingFactory;

fn schema() -> Schema {
    Schema::object(Vec::new(), &[]).expect("empty schema")
}

impl UpstreamFactory for DefaultFactory {
    type Config = TestConfig;

    fn spec(&self) -> &'static UpstreamSpec {
        &SPEC
    }

    fn config_schema(&self) -> Schema {
        schema()
    }

    fn validate(&self, _raw: &RawTable) -> Result<Self::Config, ConfigError> {
        Ok(TestConfig)
    }

    fn build(
        &self,
        _cfg: &Self::Config,
        _host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, BuildError> {
        Ok(Arc::new(EchoProtocol(ProtocolId::OPENAI_CHAT)))
    }
}

impl UpstreamFactory for OverridingFactory {
    type Config = TestConfig;

    fn spec(&self) -> &'static UpstreamSpec {
        &SPEC
    }

    fn config_schema(&self) -> Schema {
        schema()
    }

    fn validate(&self, _raw: &RawTable) -> Result<Self::Config, ConfigError> {
        Ok(TestConfig)
    }

    fn build(
        &self,
        _cfg: &Self::Config,
        _host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, BuildError> {
        Ok(Arc::new(EchoProtocol(ProtocolId::OPENAI_CHAT)))
    }

    fn build_with(
        &self,
        _cfg: &Self::Config,
        _host: &HostContext,
        ctx: &BuildContext,
    ) -> Result<Arc<dyn UpstreamProvider>, BuildError> {
        Ok(Arc::new(EchoProtocol(ctx.protocol())))
    }
}

/// Answers every call with the protocol it was built for.
struct EchoProtocol(ProtocolId);

#[async_trait]
impl UpstreamProvider for EchoProtocol {
    async fn execute(
        &self,
        _request: Request,
        _inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        Ok(Completion::Raw(BodyStream::from_bytes(
            self.0.as_str().as_bytes(),
        )))
    }
}

fn host(test: &str) -> HostContext {
    let root = std::env::temp_dir().join(format!(
        "hofund-build-context-{}-{test}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("test root");
    test_host(PluginId::from_static("multi"), root)
}

fn built_protocol(provider: &Arc<dyn UpstreamProvider>) -> String {
    block_on(async {
        let completion = provider
            .execute(Request::new("model"), ProtocolId::OPENAI_CHAT)
            .await
            .expect("execute");
        let Completion::Raw(mut body) = completion else {
            panic!("echo returns a raw body");
        };
        let chunk = body.next_chunk().await.expect("chunk").expect("bytes");
        String::from_utf8(chunk.to_vec()).expect("utf-8")
    })
}

#[test]
fn default_build_with_only_builds_the_first_protocol() {
    let host = host("default");
    let first = DefaultFactory
        .build_with(
            &TestConfig,
            &host,
            &BuildContext::new(ProtocolId::OPENAI_CHAT),
        )
        .expect("first protocol");
    assert_eq!(built_protocol(&first), "openai-chat");
    let second = BuildContext::new(ProtocolId::OPENAI_RESPONSES);
    let Err(error) = DefaultFactory.build_with(&TestConfig, &host, &second) else {
        panic!("second protocol built without an override");
    };
    assert_eq!(
        error.message(),
        "plugin `multi` cannot build protocol `openai-responses` without overriding `build_with`"
    );
}

#[test]
fn dyn_build_rejects_an_undeclared_protocol() {
    let host = host("dyn");
    let ctx = BuildContext::new(ProtocolId::ANTHROPIC_MESSAGES);
    let Err(error) = DynUpstreamFactory::validate_and_build_with(
        &OverridingFactory,
        &RawTable::new(),
        &host,
        &ctx,
    ) else {
        panic!("undeclared protocol was accepted");
    };
    assert_eq!(
        error.message(),
        "plugin `multi` does not declare protocol `anthropic-messages`"
    );
}

#[test]
fn an_overriding_factory_receives_the_selected_protocol() {
    let host = host("override");
    for (protocol, expected) in [
        (ProtocolId::OPENAI_RESPONSES, "openai-responses"),
        (ProtocolId::OPENAI_CHAT, "openai-chat"),
    ] {
        let provider = DynUpstreamFactory::validate_and_build_with(
            &OverridingFactory,
            &RawTable::new(),
            &host,
            &BuildContext::new(protocol),
        )
        .expect("declared protocol");
        assert_eq!(built_protocol(&provider), expected);
    }
    let legacy =
        DynUpstreamFactory::validate_and_build(&OverridingFactory, &RawTable::new(), &host)
            .expect("legacy build");
    assert_eq!(built_protocol(&legacy), "openai-chat");
}
