//! Adapter for upstreams that speak OpenAI Chat Completions or the Responses API.
//!
//! The base URL is the caller's. A Mantle host (`bedrock-mantle.<region>.api.aws`)
//! is the one exception: chat and Responses pick `/v1` or `/openai/v1` from the
//! model id. See [`crate::mantle`].

use async_trait::async_trait;
use hofund_codec::{Codec, OpenAiChat, OpenAiResponses};
use hofund_ir::Request;
use reqwest::header::HeaderMap;

use crate::error::ProviderError;
use crate::http::{self, Endpoint, Upstream};
use crate::mantle::{self, LearnedRoutes};
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
    /// Set only when the base URL is a Mantle `/v1` root.
    mantle: Option<Mantle>,
    models: crate::models::ModelClient,
}

#[derive(Debug)]
struct Mantle {
    base: String,
    routes: LearnedRoutes,
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
        let mantle = mantle::is_mantle_root(&endpoint.base_url).then(|| Mantle {
            base: endpoint.base_url.clone(),
            routes: LearnedRoutes::default(),
        });
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
            mantle,
            models,
        })
    }

    fn path(&self) -> &'static str {
        match self.protocol {
            OpenAiProtocol::Chat => "chat/completions",
            OpenAiProtocol::Responses => "responses",
        }
    }

    fn url_for(&self, model: &str) -> String {
        let Some(mantle) = &self.mantle else {
            return http::request_url(&self.upstream).to_owned();
        };
        let base = mantle::mantle_base(&mantle.base, model, mantle.routes.other(model));
        format!(
            "{}/{}",
            base.trim_end_matches('/'),
            self.path().trim_start_matches('/')
        )
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
        let Some(mantle) = &self.mantle else {
            return http::execute(&self.upstream, codec, request, prepare, inbound_protocol).await;
        };
        let model = request.model.clone();
        let url = self.url_for(&model);
        match http::execute_at(
            &self.upstream,
            &url,
            codec,
            request.clone(),
            prepare,
            inbound_protocol,
        )
        .await
        {
            Err(error) if mantle::wrong_route(error.status, &error.message) => {
                mantle.routes.learn_other(&model);
                let url = self.url_for(&model);
                match http::execute_at(
                    &self.upstream,
                    &url,
                    codec,
                    request,
                    prepare,
                    inbound_protocol,
                )
                .await
                {
                    Ok(completion) => Ok(completion),
                    Err(second)
                        if mantle::wrong_route(second.status, &second.message)
                            || second.status.is_none() =>
                    {
                        mantle.routes.forget(&model);
                        Err(error)
                    }
                    other => other,
                }
            }
            other => other,
        }
    }

    async fn list_models(
        &self,
        query: &hofund_plugin_api::ListQuery,
    ) -> Result<hofund_plugin_api::ModelList, hofund_plugin_api::PluginError> {
        if self.models.is_azure() {
            crate::azure::azure_list(&self.models.endpoint).await
        } else {
            self.models.live_list(query, false).await
        }
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
    use crate::mantle::LearnedRoutes;
    use crate::{Completion, Provider};
    use hofund_ir::{ContentPart, Message, Request, Role};

    fn say(model: &str) -> Request {
        let mut request = Request::new(model);
        let mut message = Message::new(Role::User);
        message.content.push(ContentPart::text("hi"));
        request.messages.push(message);
        request
    }

    fn chat_ok() -> serde_json::Value {
        serde_json::json!({
            "id": "chatcmpl-1",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "hello"},
                "finish_reason": "stop"
            }]
        })
    }

    #[test]
    fn a_mantle_root_picks_the_prefix_from_the_model_id() {
        let endpoint = Endpoint::new("mantle", "https://bedrock-mantle.us-east-1.api.aws/v1");
        let provider = OpenAiCompat::new(OpenAiProtocol::Chat, endpoint).expect("provider");
        assert!(provider.mantle.is_some());
        assert_eq!(
            provider.url_for("openai.gpt-5.5"),
            "https://bedrock-mantle.us-east-1.api.aws/openai/v1/chat/completions"
        );
        assert_eq!(
            provider.url_for("openai.gpt-oss-120b"),
            "https://bedrock-mantle.us-east-1.api.aws/v1/chat/completions"
        );
        assert_eq!(
            provider.url_for("xai.grok-4.3"),
            "https://bedrock-mantle.us-east-1.api.aws/openai/v1/chat/completions"
        );
        let responses = OpenAiCompat::new(
            OpenAiProtocol::Responses,
            Endpoint::new("mantle", "https://bedrock-mantle.eu-west-1.api.aws/v1"),
        )
        .expect("responses");
        assert_eq!(
            responses.url_for("xai.grok-4.3"),
            "https://bedrock-mantle.eu-west-1.api.aws/openai/v1/responses"
        );

        let runtime = Endpoint::new(
            "runtime",
            "https://bedrock-runtime.us-east-1.amazonaws.com/openai/v1",
        );
        let runtime = OpenAiCompat::new(OpenAiProtocol::Chat, runtime).expect("runtime");
        assert!(runtime.mantle.is_none());
        assert_eq!(
            runtime.url_for("openai.gpt-oss-120b"),
            "https://bedrock-runtime.us-east-1.amazonaws.com/openai/v1/chat/completions"
        );
    }

    #[tokio::test]
    async fn a_refused_prefix_is_tried_once_at_the_other() {
        let server = wiremock::MockServer::start().await;
        let base = format!("{}/v1", server.uri());
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "error": {"message": "model `newmaker.model-1` isn't supported on this route"}
                })),
            )
            .expect(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/openai/v1/chat/completions"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(chat_ok()))
            .expect(2)
            .mount(&server)
            .await;

        let mut endpoint = Endpoint::new("mantle", &base);
        endpoint.api_key = "ABSK-test".to_owned();
        let mut provider = OpenAiCompat::new(OpenAiProtocol::Chat, endpoint).expect("provider");
        provider.mantle = Some(super::Mantle {
            base,
            routes: LearnedRoutes::default(),
        });

        let first = provider
            .execute(say("newmaker.model-1"), "openai-chat")
            .await
            .expect("retry");
        assert!(matches!(first, Completion::Raw(_)));
        let second = provider
            .execute(say("newmaker.model-1"), "openai-chat")
            .await
            .expect("remembered");
        assert!(matches!(second, Completion::Raw(_)));
        let requests = server.received_requests().await.expect("requests");
        assert!(requests.iter().all(|request| {
            request
                .headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                == Some("Bearer ABSK-test")
        }));
    }

    #[tokio::test]
    async fn a_model_refused_at_both_prefixes_keeps_the_guess() {
        let server = wiremock::MockServer::start().await;
        let base = format!("{}/v1", server.uri());
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "error": {"message": "isn't supported on this route"}
                })),
            )
            .expect(2)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/openai/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "error": {"message": "is not supported on this route"}
                })),
            )
            .expect(2)
            .mount(&server)
            .await;

        let mut provider = OpenAiCompat::new(OpenAiProtocol::Chat, Endpoint::new("mantle", &base))
            .expect("provider");
        provider.mantle = Some(super::Mantle {
            base,
            routes: LearnedRoutes::default(),
        });

        let Err(error) = provider
            .execute(say("newmaker.model-1"), "openai-chat")
            .await
        else {
            panic!("both prefixes refused");
        };
        assert!(error.message.contains("isn't supported on this route"));
        let Err(again) = provider
            .execute(say("newmaker.model-1"), "openai-chat")
            .await
        else {
            panic!("the guess was not restored");
        };
        assert!(again.message.contains("isn't supported on this route"));
    }
}
