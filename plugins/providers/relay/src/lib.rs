//! Sample upstream plugin.
//!
//! `contribute` is the only registration path. The call goes through
//! [`hofund_plugin_sdk::Call`] on the host HTTP handle. The preset id is the
//! plugin id.

#![deny(missing_docs)]

use std::sync::Arc;

use hofund_plugin_api::{
    BodyStream, BuildError, Completion, ConfigError, CredentialStyle, HostContext, ListKind,
    PluginError, PluginId, ProtocolId, RawTable, RawValue, Request, Schema, UpstreamFactory,
    UpstreamProvider, UpstreamSpec, ValidConfig,
};
use hofund_plugin_sdk::{Call, Contribution};

/// Registers the relay factory.
#[must_use]
pub fn contribute() -> Contribution {
    Contribution::new().upstream(RelayFactory)
}

static SPEC: UpstreamSpec = UpstreamSpec::from_static(
    PluginId::from_static("relay"),
    &[ProtocolId::OPENAI_CHAT],
    CredentialStyle::Bearer,
    ListKind::Unsupported,
);

struct RelayConfig {
    base_url: String,
}

impl ValidConfig for RelayConfig {}

struct RelayFactory;

impl UpstreamFactory for RelayFactory {
    type Config = RelayConfig;

    fn spec(&self) -> &'static UpstreamSpec {
        &SPEC
    }

    fn config_schema(&self) -> Schema {
        Schema::object(
            vec![("base_url".to_owned(), Schema::string())],
            &["base_url"],
        )
        .expect("relay schema")
    }

    fn validate(&self, raw: &RawTable) -> Result<Self::Config, ConfigError> {
        for key in raw.keys() {
            if key != "base_url" {
                return Err(ConfigError::new("unknown key").with_path(key));
            }
        }
        match raw.get("base_url") {
            Some(RawValue::String(url))
                if url.starts_with("https://") || url.starts_with("http://") =>
            {
                Ok(RelayConfig {
                    base_url: url.clone(),
                })
            }
            Some(RawValue::String(_)) => Err(ConfigError::new(
                "base_url must start with http:// or https://",
            )
            .with_path("base_url")),
            Some(_) => Err(ConfigError::new("base_url must be a string").with_path("base_url")),
            None => Err(ConfigError::new("base_url is required").with_path("base_url")),
        }
    }

    fn build(
        &self,
        cfg: &Self::Config,
        host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, BuildError> {
        Ok(Arc::new(Relay {
            base_url: cfg.base_url.clone(),
            host: host.clone(),
        }))
    }
}

struct Relay {
    base_url: String,
    host: HostContext,
}

#[hofund_plugin_sdk::async_trait]
impl UpstreamProvider for Relay {
    async fn execute(
        &self,
        request: Request,
        inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        let _ = inbound;
        let secret = self
            .host
            .secrets()
            .get("api_key")
            .await?
            .ok_or_else(|| PluginError::auth("api_key is unset"))?;
        let url = format!("{}/v1/chat", self.base_url.trim_end_matches('/'));
        let reply = Call::post(url)
            .credential(CredentialStyle::Bearer, &secret)?
            .bytes(request.model.as_bytes())
            .send(self.host.http())
            .await?;
        Ok(Completion::Raw(BodyStream::from_bytes(reply.into_body())))
    }
}

#[cfg(test)]
mod tests {
    use hofund_kit_upstream::{TestHost, block_on, check_list_and_probe};
    use hofund_plugin_api::{
        Completion, DynUpstreamFactory, PluginErrorKind, PluginId, ProtocolId, RawTable, RawValue,
        Request, UpstreamFactory,
    };
    use hofund_plugin_sdk::Call;

    use super::RelayFactory;

    fn scratch() -> tempfile::TempDir {
        tempfile::tempdir().expect("scratch")
    }

    fn options(url: &str) -> RawTable {
        let mut raw = RawTable::new();
        raw.insert("base_url", RawValue::String(url.to_owned()));
        raw
    }

    #[test]
    fn unknown_keys_and_bad_urls_are_rejected() {
        let mut raw = options("http://upstream.test");
        raw.insert("region", RawValue::String("us".to_owned()));
        let Err(error) = UpstreamFactory::validate(&RelayFactory, &raw) else {
            panic!("unknown key was accepted");
        };
        assert_eq!(error.path(), "region");
        let Err(error) = UpstreamFactory::validate(&RelayFactory, &options("ftp://upstream.test"))
        else {
            panic!("bad scheme was accepted");
        };
        assert_eq!(error.path(), "base_url");
        assert!(
            UpstreamFactory::config_schema(&RelayFactory)
                .field_names()
                .contains(&"base_url")
        );
    }

    #[test]
    fn posts_the_model_with_a_sensitive_bearer_header() {
        block_on(async {
            let plugin = PluginId::from_static("relay");
            let root = scratch();
            let host = TestHost::open(plugin.clone(), root.path());
            host.secrets().insert(&plugin, "api_key", "secret-token");
            host.http().enqueue(b"pong".to_vec());
            let provider = DynUpstreamFactory::validate_and_build(
                &RelayFactory,
                &options("http://upstream.test/"),
                host.context(),
            )
            .expect("build");
            let completion = provider
                .execute(Request::new("gpt-test"), ProtocolId::OPENAI_CHAT)
                .await
                .expect("execute");
            let Completion::Raw(mut body) = completion else {
                panic!("relay returns raw bytes");
            };
            let chunk = body.next_chunk().await.expect("chunk").expect("bytes");
            assert_eq!(&chunk[..], b"pong");
            let calls = host.http().calls();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].method(), "POST");
            assert_eq!(calls[0].uri(), "http://upstream.test/v1/chat");
            assert_eq!(calls[0].body(), b"gpt-test");
            assert_eq!(calls[0].sensitive_headers(), ["authorization"]);
        });
    }

    #[test]
    fn a_missing_api_key_is_auth_and_does_not_send() {
        block_on(async {
            let plugin = PluginId::from_static("relay");
            let root = scratch();
            let host = TestHost::open(plugin, root.path());
            let provider = DynUpstreamFactory::validate_and_build(
                &RelayFactory,
                &options("https://upstream.test"),
                host.context(),
            )
            .expect("build");
            let Err(error) = provider
                .execute(Request::new("gpt-test"), ProtocolId::OPENAI_CHAT)
                .await
            else {
                panic!("missing api key was accepted");
            };
            assert_eq!(error.kind(), PluginErrorKind::Auth);
            assert!(host.http().calls().is_empty());
        });
    }

    #[test]
    fn unsupported_factory_passes_list_conformance() {
        let root = scratch();
        block_on(check_list_and_probe(
            &RelayFactory,
            options,
            ProtocolId::OPENAI_CHAT,
            "",
            root.path(),
        ));
    }

    #[test]
    fn connect_failures_are_retried() {
        block_on(async {
            let plugin = PluginId::from_static("relay");
            let root = scratch();
            let host = TestHost::open(plugin, root.path());
            host.http().enqueue_connect("down");
            host.http().enqueue(b"pong".to_vec());
            let reply = Call::post("http://upstream.test/v1")
                .bytes(b"x")
                .send_with_retry(host.context().http(), 3)
                .await
                .expect("retry");
            assert_eq!(reply.body(), b"pong");
            assert_eq!(host.http().calls().len(), 2);
        });
    }
}
