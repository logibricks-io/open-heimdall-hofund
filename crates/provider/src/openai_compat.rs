//! Adapter for upstreams that speak OpenAI Chat Completions or the Responses API.
//!
//! The base URL is the caller's. This adapter does not assume a particular host.

use async_trait::async_trait;
use hofund_codec::{Codec, OpenAiChat, OpenAiResponses};
use hofund_ir::Request;
use reqwest::header::{HeaderMap, HeaderValue};

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
        let mut headers = HeaderMap::new();
        if !endpoint.api_key.is_empty() {
            let mut value = HeaderValue::try_from(format!("Bearer {}", endpoint.api_key))
                .map_err(|_| ProviderError::decode("API key is not valid header text"))?;
            value.set_sensitive(true);
            headers.insert(reqwest::header::AUTHORIZATION, value);
        }
        Ok(Self {
            protocol,
            upstream: Upstream::open(endpoint, path, headers)?,
        })
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
}
