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
    /// The normalized base URL, set only when the base is not a Mantle root.
    direct: Option<String>,
    models: crate::models::ModelClient,
}

#[derive(Debug)]
struct Mantle {
    base: String,
    /// One adapter speaks one protocol, so this per-model memory is per protocol too.
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
        let (path, wire) = match protocol {
            OpenAiProtocol::Chat => ("chat/completions", crate::Wire::Chat),
            OpenAiProtocol::Responses => ("responses", crate::Wire::Responses),
        };
        let mut endpoint = endpoint;
        mantle::normalize(&mut endpoint, wire);
        let mantle = mantle::is_mantle_root(&endpoint).then(|| Mantle {
            base: endpoint.base_url.clone(),
            routes: LearnedRoutes::default(),
        });
        let direct = mantle.is_none().then(|| endpoint.base_url.clone());
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
            direct,
            models,
        })
    }

    /// The base URL a caller may post to directly.
    ///
    /// This is the configured base after Mantle normalization. It is `None` for
    /// a Mantle `/v1` root, whose path prefix depends on the model id and on
    /// routes learned at run time, so only this adapter can send to it.
    #[must_use]
    pub fn direct_base(&self) -> Option<&str> {
        self.direct.as_deref()
    }

    fn path(&self) -> &'static str {
        match self.protocol {
            OpenAiProtocol::Chat => "chat/completions",
            OpenAiProtocol::Responses => "responses",
        }
    }

    fn url_for(&self, model: &str, other: bool) -> String {
        let Some(mantle) = &self.mantle else {
            return http::request_url(&self.upstream).to_owned();
        };
        let base = mantle::mantle_base(&mantle.base, model, other);
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
        let learned = mantle.routes.other(&model);
        let url = self.url_for(&model, learned);
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
            Err(error)
                if !model.is_empty()
                    && !learned
                    && mantle::wrong_route(error.status, &error.message) =>
            {
                mantle.routes.learn_other(&model);
                let url = self.url_for(&model, true);
                // reference: internal/gateway/mantle.go:54-72
                match http::send_at(
                    &self.upstream,
                    &url,
                    codec,
                    request,
                    prepare,
                    inbound_protocol,
                )
                .await
                {
                    Ok(sent) => sent.complete().await,
                    Err(second)
                        if mantle::wrong_route_message(&second.message)
                            || matches!(
                                second.kind,
                                crate::ProviderErrorKind::Connect
                                    | crate::ProviderErrorKind::Timeout
                                    | crate::ProviderErrorKind::Transport
                            ) =>
                    {
                        mantle.routes.forget(&model);
                        Err(error)
                    }
                    Err(second) => Err(second),
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
            provider.url_for("openai.gpt-5.5", false),
            "https://bedrock-mantle.us-east-1.api.aws/openai/v1/chat/completions"
        );
        assert_eq!(
            provider.url_for("openai.gpt-oss-120b", false),
            "https://bedrock-mantle.us-east-1.api.aws/v1/chat/completions"
        );
        assert_eq!(
            provider.url_for("xai.grok-4.3", false),
            "https://bedrock-mantle.us-east-1.api.aws/openai/v1/chat/completions"
        );
        let responses = OpenAiCompat::new(
            OpenAiProtocol::Responses,
            Endpoint::new("mantle", "https://bedrock-mantle.eu-west-1.api.aws/v1"),
        )
        .expect("responses");
        assert_eq!(
            responses.url_for("xai.grok-4.3", false),
            "https://bedrock-mantle.eu-west-1.api.aws/openai/v1/responses"
        );

        let runtime = Endpoint::new(
            "runtime",
            "https://bedrock-runtime.us-east-1.amazonaws.com/openai/v1",
        );
        let runtime = OpenAiCompat::new(OpenAiProtocol::Chat, runtime).expect("runtime");
        assert!(runtime.mantle.is_none());
        assert_eq!(
            runtime.url_for("openai.gpt-oss-120b", false),
            "https://bedrock-runtime.us-east-1.amazonaws.com/openai/v1/chat/completions"
        );
    }

    #[test]
    fn only_a_base_that_is_not_a_mantle_root_is_direct() {
        let mantle = OpenAiCompat::new(
            OpenAiProtocol::Chat,
            Endpoint::new("mantle", "https://bedrock-mantle.us-east-1.api.aws/v1"),
        )
        .expect("mantle");
        assert_eq!(mantle.direct_base(), None);
        let normalized = OpenAiCompat::new(
            OpenAiProtocol::Chat,
            Endpoint::new(
                "mantle",
                "https://bedrock-mantle.us-east-1.api.aws/openai/v1",
            ),
        )
        .expect("normalized");
        assert_eq!(
            normalized.direct_base().is_some(),
            normalized.mantle.is_none()
        );
        let plain = OpenAiCompat::new(
            OpenAiProtocol::Responses,
            Endpoint::new("plain", "http://127.0.0.1:9/v1/"),
        )
        .expect("plain");
        assert_eq!(plain.direct_base(), Some("http://127.0.0.1:9/v1/"));
    }

    #[test]
    fn mantle_route_learning_is_per_provider_instance() {
        let endpoint = || Endpoint::new("mantle", "https://bedrock-mantle.us-east-1.api.aws/v1");
        let first = OpenAiCompat::new(OpenAiProtocol::Chat, endpoint()).expect("first");
        let second = OpenAiCompat::new(OpenAiProtocol::Chat, endpoint()).expect("second");
        first
            .mantle
            .as_ref()
            .expect("first Mantle")
            .routes
            .learn_other("newmaker.model-1");
        assert!(
            !second
                .mantle
                .as_ref()
                .expect("second Mantle")
                .routes
                .other("newmaker.model-1")
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
        endpoint.preset = Some("bedrock-mantle".to_owned());
        endpoint.api_key = "ABSK-test".to_owned();
        let provider = OpenAiCompat::new(OpenAiProtocol::Chat, endpoint).expect("provider");

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

        let mut endpoint = Endpoint::new("mantle", &base);
        endpoint.preset = Some("bedrock-mantle".to_owned());
        let provider = OpenAiCompat::new(OpenAiProtocol::Chat, endpoint).expect("provider");

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

    #[tokio::test]
    async fn a_refusal_on_the_learned_prefix_is_not_retried_or_forgotten() {
        let server = wiremock::MockServer::start().await;
        let base = format!("{}/v1", server.uri());
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(chat_ok()))
            .expect(0)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/openai/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "error": {"message": "learned route isn't supported on this route"}
                })),
            )
            .expect(2)
            .mount(&server)
            .await;

        let mut endpoint = Endpoint::new("mantle", &base);
        endpoint.preset = Some("bedrock-mantle".to_owned());
        let provider = OpenAiCompat::new(OpenAiProtocol::Chat, endpoint).expect("provider");
        provider
            .mantle
            .as_ref()
            .expect("mantle")
            .routes
            .learn_other("newmaker.model-1");
        for _ in 0..2 {
            let Err(error) = provider
                .execute(say("newmaker.model-1"), "openai-chat")
                .await
            else {
                panic!("learned prefix was not refused");
            };
            assert!(error.message.contains("learned route"));
        }
        assert!(
            provider
                .mantle
                .as_ref()
                .expect("mantle")
                .routes
                .other("newmaker.model-1")
        );
    }

    #[tokio::test]
    async fn concurrent_first_calls_each_try_the_other_prefix_once() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(400)
                    .set_delay(std::time::Duration::from_millis(250))
                    .set_body_json(serde_json::json!({
                        "error": {"message": "model isn't supported on this route"}
                    })),
            )
            .expect(4)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/openai/v1/chat/completions"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(chat_ok()))
            .expect(4)
            .mount(&server)
            .await;
        let mut endpoint = Endpoint::new("mantle", format!("{}/v1", server.uri()));
        endpoint.preset = Some("bedrock-mantle".to_owned());
        let provider = std::sync::Arc::new(
            OpenAiCompat::new(OpenAiProtocol::Chat, endpoint).expect("provider"),
        );

        let results = futures_util::future::join_all((0..4).map(|_| {
            let provider = std::sync::Arc::clone(&provider);
            async move {
                provider
                    .execute(say("newmaker.model-1"), "openai-chat")
                    .await
            }
        }))
        .await;
        for result in results {
            assert!(matches!(
                result.expect("concurrent call"),
                Completion::Raw(_)
            ));
        }
        let requests = server.received_requests().await.expect("requests");
        assert_eq!(requests.len(), 8);
        for (path, expected) in [
            ("/v1/chat/completions", 4),
            ("/openai/v1/chat/completions", 4),
        ] {
            assert_eq!(
                requests
                    .iter()
                    .filter(|request| request.url.path() == path)
                    .count(),
                expected
            );
        }
    }

    #[tokio::test]
    async fn a_second_decode_error_is_returned_and_the_route_stays_learned() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "error": {"message": "first prefix isn't supported on this route"}
                })),
            )
            .expect(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/openai/v1/chat/completions"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("not JSON"))
            .expect(2)
            .mount(&server)
            .await;
        let mut endpoint = Endpoint::new("mantle", format!("{}/v1", server.uri()));
        endpoint.preset = Some("bedrock-mantle".to_owned());
        let provider = OpenAiCompat::new(OpenAiProtocol::Chat, endpoint).expect("provider");

        for _ in 0..2 {
            let Err(error) = provider
                .execute(say("newmaker.model-1"), "anthropic-messages")
                .await
            else {
                panic!("an invalid success response must not decode");
            };
            assert_eq!(error.kind, crate::ProviderErrorKind::Decode);
            assert!(!error.message.contains("first prefix"));
        }
        assert!(
            provider
                .mantle
                .as_ref()
                .expect("mantle")
                .routes
                .other("newmaker.model-1")
        );
    }

    #[tokio::test]
    async fn a_second_route_refusal_at_any_status_forgets_the_route() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "error": {"message": "first prefix isn't supported on this route"}
                })),
            )
            .expect(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/openai/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(500).set_body_json(serde_json::json!({
                    "error": {"message": "second prefix is not supported on this route"}
                })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let mut endpoint = Endpoint::new("mantle", format!("{}/v1", server.uri()));
        endpoint.preset = Some("bedrock-mantle".to_owned());
        let provider = OpenAiCompat::new(OpenAiProtocol::Chat, endpoint).expect("provider");

        let Err(error) = provider
            .execute(say("newmaker.model-1"), "openai-chat")
            .await
        else {
            panic!("both prefixes refused");
        };
        assert_eq!(error.status, Some(400));
        assert!(error.message.contains("first prefix"));
        assert!(
            !provider
                .mantle
                .as_ref()
                .expect("mantle")
                .routes
                .other("newmaker.model-1")
        );
    }

    fn complete_request(request: &[u8]) -> bool {
        let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
            return false;
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let length = headers
            .split("\r\n")
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        request.len() >= header_end + 4 + length
    }

    #[tokio::test]
    async fn a_second_transport_error_forgets_the_route() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            for attempt in 0..2 {
                let (mut socket, _) = listener.accept().await.expect("connection");
                let mut request = Vec::new();
                let mut chunk = [0_u8; 1024];
                loop {
                    let read = socket.read(&mut chunk).await.expect("read request");
                    assert_ne!(read, 0, "request ended before its body");
                    request.extend_from_slice(&chunk[..read]);
                    if complete_request(&request) {
                        break;
                    }
                }
                if attempt == 0 {
                    assert!(request.starts_with(b"POST /v1/chat/completions HTTP/1.1\r\n"));
                    let body =
                        br#"{"error":{"message":"first prefix isn't supported on this route"}}"#;
                    socket
                        .write_all(
                            format!(
                                "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                                body.len()
                            )
                            .as_bytes(),
                        )
                        .await
                        .expect("response headers");
                    socket.write_all(body).await.expect("response body");
                } else {
                    assert!(request.starts_with(b"POST /openai/v1/chat/completions HTTP/1.1\r\n"));
                }
            }
        });
        let mut endpoint = Endpoint::new("mantle", format!("http://{address}/v1"));
        endpoint.preset = Some("bedrock-mantle".to_owned());
        let provider = OpenAiCompat::new(OpenAiProtocol::Chat, endpoint).expect("provider");

        let Err(error) = provider
            .execute(say("newmaker.model-1"), "openai-chat")
            .await
        else {
            panic!("a dropped connection must fail");
        };
        server.await.expect("server");
        assert_eq!(error.status, Some(400));
        assert!(error.message.contains("first prefix"));
        assert!(
            !provider
                .mantle
                .as_ref()
                .expect("mantle")
                .routes
                .other("newmaker.model-1")
        );
    }

    async fn assert_second_ok_with_broken_body_keeps_route(streaming: bool) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            for attempt in 0..2 {
                let (mut socket, _) = listener.accept().await.expect("connection");
                let mut request = Vec::new();
                let mut chunk = [0_u8; 1024];
                loop {
                    let read = socket.read(&mut chunk).await.expect("read request");
                    assert_ne!(read, 0, "request ended before its body");
                    request.extend_from_slice(&chunk[..read]);
                    if complete_request(&request) {
                        break;
                    }
                }
                if attempt == 0 {
                    assert!(request.starts_with(b"POST /v1/chat/completions HTTP/1.1\r\n"));
                    let body =
                        br#"{"error":{"message":"first prefix isn't supported on this route"}}"#;
                    socket
                        .write_all(
                            format!(
                                "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                                body.len()
                            )
                            .as_bytes(),
                        )
                        .await
                        .expect("response headers");
                    socket.write_all(body).await.expect("response body");
                } else {
                    assert!(request.starts_with(b"POST /openai/v1/chat/completions HTTP/1.1\r\n"));
                    let content_type = if streaming {
                        "text/event-stream"
                    } else {
                        "application/json"
                    };
                    socket
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: 500\r\nconnection: close\r\n\r\n"
                            )
                            .as_bytes(),
                        )
                        .await
                        .expect("response headers");
                    socket.write_all(b"1234567890").await.expect("partial body");
                }
            }
        });
        let mut endpoint = Endpoint::new("mantle", format!("http://{address}/v1"));
        endpoint.preset = Some("bedrock-mantle".to_owned());
        let provider = OpenAiCompat::new(OpenAiProtocol::Chat, endpoint).expect("provider");
        let mut request = say("newmaker.model-1");
        request.stream = streaming;

        let Err(error) = provider.execute(request, "openai-chat").await else {
            panic!("a truncated success body must fail");
        };
        server.await.expect("server");
        assert_ne!(error.status, Some(400));
        assert!(!error.message.contains("first prefix"));
        assert!(
            provider
                .mantle
                .as_ref()
                .expect("mantle")
                .routes
                .other("newmaker.model-1")
        );
    }

    #[tokio::test]
    async fn a_second_ok_with_a_truncated_body_keeps_the_route() {
        assert_second_ok_with_broken_body_keeps_route(false).await;
    }

    #[tokio::test]
    async fn a_second_ok_stream_that_breaks_keeps_the_route() {
        assert_second_ok_with_broken_body_keeps_route(true).await;
    }

    #[tokio::test]
    async fn an_unrelated_second_error_is_returned_and_the_route_stays_learned() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "error": {"message": "first prefix isn't supported on this route"}
                })),
            )
            .expect(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/openai/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "error": {"message": "invalid model id"}
                })),
            )
            .expect(2)
            .mount(&server)
            .await;
        let mut endpoint = Endpoint::new("mantle", format!("{}/v1", server.uri()));
        endpoint.preset = Some("bedrock-mantle".to_owned());
        let provider = OpenAiCompat::new(OpenAiProtocol::Chat, endpoint).expect("provider");

        for _ in 0..2 {
            let Err(error) = provider
                .execute(say("newmaker.model-1"), "openai-chat")
                .await
            else {
                panic!("the second refusal must be returned");
            };
            assert_eq!(error.status, Some(400));
            assert_eq!(error.message, "invalid model id");
        }
        assert!(
            provider
                .mantle
                .as_ref()
                .expect("mantle")
                .routes
                .other("newmaker.model-1")
        );
    }

    #[tokio::test]
    async fn an_empty_model_is_never_rerouted() {
        let server = wiremock::MockServer::start().await;
        let base = format!("{}/v1", server.uri());
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "error": {"message": "model isn't supported on this route"}
                })),
            )
            .expect(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/openai/v1/chat/completions"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(chat_ok()))
            .expect(0)
            .mount(&server)
            .await;

        let mut endpoint = Endpoint::new("mantle", &base);
        endpoint.preset = Some("bedrock-mantle".to_owned());
        let provider = OpenAiCompat::new(OpenAiProtocol::Chat, endpoint).expect("provider");
        let Err(error) = provider.execute(say(""), "openai-chat").await else {
            panic!("empty model was rerouted");
        };
        assert!(error.message.contains("isn't supported on this route"));
    }
}
