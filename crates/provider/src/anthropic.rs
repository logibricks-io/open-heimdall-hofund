//! Adapter for upstreams that speak the Anthropic Messages protocol.

use async_trait::async_trait;
use hofund_codec::AnthropicMessages;
use hofund_ir::Request;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

use crate::error::ProviderError;
use crate::http::{self, Endpoint, Upstream};
use crate::{Completion, Provider};

const VERSION: &str = "2023-06-01";

/// One Messages upstream.
#[derive(Debug)]
pub struct Anthropic {
    upstream: Upstream,
}

impl Anthropic {
    /// Builds a client.
    ///
    /// The adapter posts to `{base_url}/v1/messages` and sends the key in
    /// `x-api-key` with `anthropic-version`.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the id, base URL, or a header is unusable.
    pub fn new(endpoint: Endpoint) -> Result<Self, ProviderError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("anthropic-version"),
            HeaderValue::from_static(VERSION),
        );
        if !endpoint.api_key.is_empty() {
            let mut value = HeaderValue::try_from(endpoint.api_key.as_str())
                .map_err(|_| ProviderError::decode("API key is not valid header text"))?;
            value.set_sensitive(true);
            headers.insert(HeaderName::from_static("x-api-key"), value);
        }
        Ok(Self {
            upstream: Upstream::open(endpoint, "v1/messages", headers)?,
        })
    }
}

#[async_trait]
impl Provider for Anthropic {
    fn id(&self) -> &str {
        http::id_of(&self.upstream)
    }

    async fn execute(&self, request: Request) -> Result<Completion, ProviderError> {
        http::execute(&self.upstream, &AnthropicMessages, request, None).await
    }
}
