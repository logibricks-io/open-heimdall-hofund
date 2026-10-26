//! Adapts an [`UpstreamProvider`] to the daemon's [`Provider`] trait.
//!
//! Chat, Responses, Messages, and Generative are built by `registry.factory`
//! and then wrapped here. The member stack still speaks [`Provider`]. Pull
//! request E replaces the factory body, not this wrapper.

use std::sync::Arc;

use async_trait::async_trait;
use futures_util::StreamExt;
use hofund_plugin_api::{
    Completion as PluginCompletion, CountOutcome as PluginCount, PluginError, PluginErrorKind,
    ProtocolId, Request, UpstreamProvider,
};
use hofund_provider::{Completion, CountOutcome, Provider, ProviderError};

/// One plugin provider behind the existing member trait.
pub(crate) struct ProviderAdapter {
    id: String,
    inner: Arc<dyn UpstreamProvider>,
}

impl ProviderAdapter {
    /// `id` is the configured provider id, recorded on routes and usage.
    pub(crate) fn new(id: impl Into<String>, inner: Arc<dyn UpstreamProvider>) -> Self {
        Self {
            id: id.into(),
            inner,
        }
    }

    pub(crate) fn upstream(&self) -> Arc<dyn UpstreamProvider> {
        Arc::clone(&self.inner)
    }
}

#[async_trait]
impl Provider for ProviderAdapter {
    fn id(&self) -> &str {
        &self.id
    }

    async fn count_input(
        &self,
        request: Request,
        inbound_protocol: &str,
    ) -> Result<CountOutcome, ProviderError> {
        let inbound = protocol(inbound_protocol)?;
        let outcome = self
            .inner
            .count_input(request, inbound)
            .await
            .map_err(provider_error)?;
        Ok(match outcome {
            PluginCount::Counted(tokens) => CountOutcome::Counted(tokens),
            PluginCount::Unsupported => CountOutcome::Unsupported,
            _ => CountOutcome::Unsupported,
        })
    }

    async fn execute(
        &self,
        request: Request,
        inbound_protocol: &str,
    ) -> Result<Completion, ProviderError> {
        let inbound = protocol(inbound_protocol)?;
        let completion = self
            .inner
            .execute(request, inbound)
            .await
            .map_err(provider_error)?;
        Ok(provider_completion(completion))
    }
}

fn protocol(inbound: &str) -> Result<ProtocolId, ProviderError> {
    ProtocolId::try_from(inbound).map_err(|_| ProviderError::decode("unknown protocol id"))
}

fn provider_error(error: PluginError) -> ProviderError {
    match error.kind() {
        PluginErrorKind::Connect => ProviderError::connect(error.message()),
        PluginErrorKind::Timeout => ProviderError::timeout(error.message()),
        PluginErrorKind::Status => {
            ProviderError::status(error.status().unwrap_or(502), error.message())
        }
        PluginErrorKind::Decode | PluginErrorKind::Transport => {
            // Transport means the request was already sent. Provider has no
            // separate kind, and Connect would let the member stack send it again.
            ProviderError::decode(error.message())
        }
        PluginErrorKind::Auth
        | PluginErrorKind::Config
        | PluginErrorKind::Unsupported
        | PluginErrorKind::Quota
        | PluginErrorKind::Cancelled
        | PluginErrorKind::Io
        | PluginErrorKind::Internal
        | _ => ProviderError::decode(error.message()),
    }
}

fn provider_completion(completion: PluginCompletion) -> Completion {
    match completion {
        PluginCompletion::Unary(response) => Completion::Unary(response),
        PluginCompletion::Stream(stream) => {
            Completion::Stream(Box::pin(stream.map(|item| item.map_err(provider_error))))
        }
        PluginCompletion::Raw(body) => Completion::Raw(Box::pin(futures_util::stream::unfold(
            body,
            |mut body| async move {
                body.next_chunk()
                    .await
                    .map(|result| (result.map_err(provider_error), body))
            },
        ))),
        _ => Completion::Raw(Box::pin(futures_util::stream::empty())),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use hofund_plugin_api::{
        BodyStream, Completion, CountOutcome, PluginError, ProtocolId, Request, UpstreamProvider,
    };
    use hofund_provider::{Provider, ProviderErrorKind};

    use super::ProviderAdapter;

    struct Echo;

    #[async_trait]
    impl UpstreamProvider for Echo {
        async fn execute(
            &self,
            request: Request,
            _inbound: ProtocolId,
        ) -> Result<Completion, PluginError> {
            let _ = request;
            Ok(Completion::Raw(BodyStream::from_bytes(b"ok".as_slice())))
        }

        async fn count_input(
            &self,
            _request: Request,
            _inbound: ProtocolId,
        ) -> Result<CountOutcome, PluginError> {
            Err(PluginError::auth("missing api key"))
        }
    }

    #[tokio::test]
    async fn the_adapter_keeps_the_provider_id_and_maps_auth_to_decode() {
        let adapter = ProviderAdapter::new("openai", Arc::new(Echo));
        assert_eq!(adapter.id(), "openai");
        let Ok(hofund_provider::Completion::Raw(_)) =
            adapter.execute(Request::new("demo"), "openai-chat").await
        else {
            panic!("raw body");
        };
        let Err(error) = adapter
            .count_input(Request::new("demo"), "openai-chat")
            .await
        else {
            panic!("auth");
        };
        assert_eq!(error.kind, ProviderErrorKind::Decode);
        assert_eq!(error.message, "missing api key");
        let Err(error) = adapter
            .execute(Request::new("demo"), "not-a-protocol")
            .await
        else {
            panic!("protocol");
        };
        assert_eq!(error.kind, ProviderErrorKind::Decode);
    }
}
