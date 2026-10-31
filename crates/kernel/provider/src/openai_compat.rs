//! Adapter for upstreams that speak OpenAI Chat Completions or the Responses API.
//!
//! The base URL is the caller's. A Bedrock Mantle base, whose prefix depends
//! on the model id, is served by the `bedrock-mantle` plugin instead.

use async_trait::async_trait;
use hofund_codec::{Codec, OpenAiChat, OpenAiResponses};
use hofund_ir::Request;
use reqwest::header::HeaderMap;
use sha2::{Digest, Sha256};

use crate::error::ProviderError;
use crate::http::{self, Endpoint, Upstream};
use crate::{AttemptHook, AttemptRoute, Completion, Provider};

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
    attempt: AttemptRoute,
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
        let attempt = AttemptRoute {
            base_url: direct.clone(),
            route: route_id(&endpoint.id, &endpoint.api_key),
            keeps_responses: protocol == OpenAiProtocol::Responses
                && responses_kept_by_host(&direct),
        };
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
            attempt,
            models,
        })
    }

    /// The base URL a caller may post to directly: the configured base.
    #[must_use]
    pub fn direct_base(&self) -> &str {
        &self.direct
    }
}

/// Whether a Responses base belongs to a host that retains responses by id.
#[must_use]
pub fn responses_kept_by_host(base_url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(base_url) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    if host == "api.openai.com" {
        return true;
    }
    if [
        ".openai.azure.com",
        ".cognitiveservices.azure.com",
        ".services.ai.azure.com",
    ]
    .iter()
    .any(|suffix| host.len() > suffix.len() && host.ends_with(suffix))
    {
        return true;
    }
    host == "ark.cn-beijing.volces.com" && url.path().contains("/api/v3")
}

fn route_id(id: &str, api_key: &str) -> String {
    let sum = Sha256::digest(api_key.as_bytes());
    let fingerprint: String = sum[..8].iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{id}\0{fingerprint}")
}

#[async_trait]
impl Provider for OpenAiCompat {
    fn id(&self) -> &str {
        http::id_of(&self.upstream)
    }

    fn attempt_route(&self) -> AttemptRoute {
        self.attempt.clone()
    }

    async fn execute(
        &self,
        request: Request,
        inbound_protocol: &str,
    ) -> Result<Completion, ProviderError> {
        self.execute_with_attempt(request, inbound_protocol, None)
            .await
    }

    async fn execute_with_attempt(
        &self,
        request: Request,
        inbound_protocol: &str,
        hook: Option<&dyn AttemptHook>,
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
        http::execute_with_attempt(
            &self.upstream,
            codec,
            request,
            prepare,
            inbound_protocol,
            &self.attempt,
            hook,
        )
        .await
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
    use super::{OpenAiCompat, OpenAiProtocol, responses_kept_by_host};
    use crate::Provider;
    use crate::http::Endpoint;

    #[test]
    fn a9_response_keeping_hosts_are_exact_and_ark_requires_api_v3() {
        for base in [
            "https://api.openai.com/v1",
            "https://resource.openai.azure.com/openai/v1",
            "https://resource.cognitiveservices.azure.com/openai/v1",
            "https://resource.services.ai.azure.com/openai/v1",
            "https://ark.cn-beijing.volces.com/api/v3",
            "https://ark.cn-beijing.volces.com/api/v3/responses",
        ] {
            assert!(responses_kept_by_host(base), "{base}");
        }
        for base in [
            "https://api.openai.com.evil.test/v1",
            "https://openai.azure.com/v1",
            "https://example.test/v1",
            "https://ark.cn-beijing.volces.com/v3",
            "https://ark.cn-beijing.volces.com/api/v4",
            "not a URL",
        ] {
            assert!(!responses_kept_by_host(base), "{base}");
        }
    }

    #[test]
    fn attempt_route_contains_only_a_hash_prefix() {
        let mut endpoint = Endpoint::new("primary", "https://api.openai.com/v1");
        endpoint.api_key = "never-print-this-key".to_owned();
        let adapter = OpenAiCompat::new(OpenAiProtocol::Responses, endpoint).expect("adapter");
        let route = adapter.attempt_route();
        assert!(route.route.starts_with("primary\0"));
        assert_eq!(route.route.len(), "primary\0".len() + 16);
        assert!(!route.route.contains("never-print-this-key"));
    }

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
