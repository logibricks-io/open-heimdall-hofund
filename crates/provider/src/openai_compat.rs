//! Adapter for upstreams that speak OpenAI Chat Completions or the Responses API.
//!
//! The base URL is the caller's. A Bedrock Mantle base, whose prefix depends
//! on the model id, is served by the `bedrock-mantle` plugin instead.

use async_trait::async_trait;
use hofund_codec::{Codec, OpenAiChat, OpenAiResponses};
use hofund_ir::Request;
use reqwest::header::HeaderMap;

use crate::error::ProviderError;
use crate::http::{self, Endpoint, Upstream};
use crate::{Completion, Provider};

/// Which OpenAI-compatible protocol the upstream expects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenAiProtocol {
    /// `POST {base}/chat/completions`.
    Chat,
    /// `POST {base}/responses`.
    Responses,
}

/// One OpenAI-compatible upstream.
#[derive(Debug)]
pub struct OpenAiCompat {
    protocol: OpenAiProtocol,
    upstream: Upstream,
    /// The configured base URL.
    direct: String,
    models: crate::models::ModelClient,
}

impl OpenAiCompat {
    /// Builds a client.
    ///
    /// `endpoint.base_url` is the root the path is appended to, including a
    /// `/v1` prefix when the upstream uses one.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the id, base URL, or a header is unusable.
    pub fn new(protocol: OpenAiProtocol, endpoint: Endpoint) -> Result<Self, ProviderError> {
        let path = match protocol {
            OpenAiProtocol::Chat => "chat/completions",
            OpenAiProtocol::Responses => "responses",
        };
        let direct = endpoint.base_url.clone();
        let mut headers = HeaderMap::new();
        http::insert_credential(
            &mut headers,
            &endpoint.api_key,
            endpoint.credential,
            http::Credential::Bearer,
        )?;
        let models = crate::models::ModelClient::new(&endpoint)
            .map_err(|error| ProviderError::decode(error.message()))?;
        Ok(Self {
            protocol,
            upstream: Upstream::open(endpoint, path, headers)?,
            direct,
            models,
        })
    }

    /// The base URL a caller may post to directly: the configured base.
    #[must_use]
    pub fn direct_base(&self) -> &str {
        &self.direct
    }
}

#[async_trait]
impl Provider for OpenAiCompat {
    fn id(&self) -> &str {
        http::id_of(&self.upstream)
    }

    async fn execute(
        &self,
        request: Request,
        inbound_protocol: &str,
    ) -> Result<Completion, ProviderError> {
        let prepare = match self.protocol {
            OpenAiProtocol::Chat => Some(OpenAiChat::prepare_upstream_body as http::Prepare),
            OpenAiProtocol::Responses => {
                Some(OpenAiResponses::prepare_upstream_body as http::Prepare)
            }
        };
        let codec: &dyn Codec = match self.protocol {
            OpenAiProtocol::Chat => &OpenAiChat,
            OpenAiProtocol::Responses => &OpenAiResponses,
        };
        http::execute(&self.upstream, codec, request, prepare, inbound_protocol).await
    }

    async fn list_models(
        &self,
        query: &hofund_plugin_api::ListQuery,
    ) -> Result<hofund_plugin_api::ModelList, hofund_plugin_api::PluginError> {
        self.models.live_list(query, false).await
    }

    async fn probe(
        &self,
        query: &hofund_plugin_api::ProbeQuery,
    ) -> Result<hofund_plugin_api::ProbeReport, hofund_plugin_api::PluginError> {
        self.models.openai_probe(self.protocol, query).await
    }
}

#[cfg(test)]
mod tests {
    use super::{OpenAiCompat, OpenAiProtocol};
    use crate::http::Endpoint;

    #[test]
    fn the_direct_base_is_the_configured_base_on_any_host() {
        for base in [
            "https://gateway.example/v1",
            "https://bedrock-mantle.us-east-1.api.aws/openai/v1",
        ] {
            for protocol in [OpenAiProtocol::Chat, OpenAiProtocol::Responses] {
                let adapter =
                    OpenAiCompat::new(protocol, Endpoint::new("primary", base)).expect("adapter");
                assert_eq!(adapter.direct_base(), base);
            }
        }
    }
}
