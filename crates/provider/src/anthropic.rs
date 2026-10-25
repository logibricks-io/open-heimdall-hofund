//! Adapter for upstreams that speak the Anthropic Messages protocol.

use async_trait::async_trait;
use hofund_codec::AnthropicMessages;
use hofund_ir::Request;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

use crate::error::ProviderError;
use crate::http::{self, Endpoint, Upstream};
use crate::{Completion, CountOutcome, Provider};

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
        http::insert_credential(
            &mut headers,
            &endpoint.api_key,
            endpoint.credential,
            http::Credential::XApiKey,
        )?;
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

    async fn count_input(
        &self,
        mut request: Request,
        inbound_protocol: &str,
    ) -> Result<CountOutcome, ProviderError> {
        request.stream = false;
        let body = omit_max_tokens(http::prepare_body(
            &self.upstream,
            &AnthropicMessages,
            request,
            None,
            inbound_protocol,
        )?);
        let url = format!("{}/count_tokens", http::messages_url(&self.upstream));
        let response = http::post_json(&self.upstream, &url, body).await?;
        let status = response.status().as_u16();
        let bytes = response.bytes().await.map_err(http::map_transport)?;
        classify_count(status, &bytes)
    }

    async fn execute(
        &self,
        request: Request,
        inbound_protocol: &str,
    ) -> Result<Completion, ProviderError> {
        http::execute(
            &self.upstream,
            &AnthropicMessages,
            request,
            None,
            inbound_protocol,
        )
        .await
    }
}

/// The count endpoint rejects `max_tokens`. Create still requires it, so the
/// codec fills a cap and this removes the field afterwards.
///
/// Allowed count fields, checked 2026-10-25:
/// <https://platform.claude.com/docs/en/build-with-claude/token-counting>.
fn omit_max_tokens(body: Vec<u8>) -> Vec<u8> {
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return body;
    };
    let Some(object) = value.as_object_mut() else {
        return body;
    };
    object.remove("max_tokens");
    serde_json::to_vec(&value).unwrap_or(body)
}

fn classify_count(status: u16, bytes: &[u8]) -> Result<CountOutcome, ProviderError> {
    if status == 404
        || status == 405
        || status == 501
        || (status == 400 && endpoint_unsupported(bytes))
    {
        return Ok(CountOutcome::Unsupported);
    }
    if (200..300).contains(&status) {
        let value: serde_json::Value = serde_json::from_slice(bytes)
            .map_err(|error| ProviderError::decode(error.to_string()))?;
        let tokens = value
            .get("input_tokens")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| ProviderError::decode("count response has no input_tokens"))?;
        return Ok(CountOutcome::Counted(tokens));
    }
    Err(ProviderError::status(status, http::message_from(bytes)))
}

/// Whether a 400 says the count endpoint itself is missing.
///
/// A message about an unsupported image or tool is a real rejection of this
/// request, not a missing endpoint. Those stay errors.
fn endpoint_unsupported(bytes: &[u8]) -> bool {
    let message = http::message_from(bytes).to_ascii_lowercase();
    let message = message.trim();
    message == "unsupported"
        || message.contains("unsupported endpoint")
        || message.contains("not supported")
        || message.contains("not implemented")
        || message.contains("does not support")
        || message.contains("do not support")
}
