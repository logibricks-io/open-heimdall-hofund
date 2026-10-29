//! Adapter for upstreams that speak the Anthropic Messages protocol.
//!
//! A Bedrock Runtime or Mantle base, whose headers and body Bedrock fits, is
//! served by the `bedrock` or `bedrock-mantle` plugin instead.

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
    models: crate::models::ModelClient,
    /// The configured base URL.
    direct: String,
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
        let models = crate::models::ModelClient::new(&endpoint)
            .map_err(|error| ProviderError::decode(error.message()))?;
        let direct = endpoint.base_url.clone();
        Ok(Self {
            upstream: Upstream::open(endpoint, "v1/messages", headers)?,
            models,
            direct,
        })
    }

    /// The base URL a caller may post to directly: the configured base.
    #[must_use]
    pub fn direct_base(&self) -> &str {
        &self.direct
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
        let retry_after = http::retry_after(response.headers());
        let bytes = if (200..300).contains(&status) {
            response.bytes().await.map_err(http::map_body)?
        } else {
            response.bytes().await.unwrap_or_default()
        };
        classify_count(status, retry_after, &bytes)
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

    async fn list_models(
        &self,
        query: &hofund_plugin_api::ListQuery,
    ) -> Result<hofund_plugin_api::ModelList, hofund_plugin_api::PluginError> {
        self.models.live_list(query, true).await
    }

    async fn probe(
        &self,
        query: &hofund_plugin_api::ProbeQuery,
    ) -> Result<hofund_plugin_api::ProbeReport, hofund_plugin_api::PluginError> {
        self.models.anthropic_probe(query).await
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

fn classify_count(
    status: u16,
    retry_after: Option<std::time::Duration>,
    bytes: &[u8],
) -> Result<CountOutcome, ProviderError> {
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
    let error = ProviderError::response(status, bytes);
    Err(match retry_after {
        Some(after) => error.with_retry_after(after),
        None => error,
    })
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

#[cfg(test)]
mod direct_tests {
    use super::Anthropic;
    use crate::http::Endpoint;

    #[test]
    fn the_direct_base_is_the_configured_base_on_any_host() {
        for base in [
            "https://gateway.example/api/",
            "https://bedrock-runtime.us-east-1.amazonaws.com",
            "https://bedrock-mantle.us-east-1.api.aws/anthropic",
        ] {
            let adapter = Anthropic::new(Endpoint::new("plain", base)).expect("adapter");
            assert_eq!(adapter.direct_base(), base);
        }
    }
}
