//! Amazon Bedrock Converse, signed with AWS Signature Version 4.
//!
//! `POST {root}/model/{modelId}/converse` is the unary call.
//! `POST {root}/model/{modelId}/converse-stream` is the stream. The body is
//! Converse JSON. The model id is only in the path, with SigV4 percent-encoding
//! (`:` becomes `%3A`). Chat, Responses, and Messages on this host stay on the
//! API-key routes and are not this adapter.
//!
//! Converse:
//! <https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_Converse.html>
//! and
//! <https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_ConverseStream.html>.
//! Signing:
//! <https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_sigv-create-signed-request.html>.
//! The runtime host:
//! <https://docs.aws.amazon.com/bedrock/latest/userguide/endpoints.html>.
//! Checked 2026-10-25.
//!
//! Not read: the instance metadata service, a shared credentials file, SSO,
//! and a Bedrock API key on this path. Those calls keep using `protocol =
//! "chat"` or `"messages"`.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use hofund_codec::{Codec, Converse};
use hofund_ir::Request;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

use crate::error::ProviderError;
use crate::http::{self, Endpoint, Upstream};
use crate::{Completion, Provider};
use hofund_plugin_sdk::sigv4::{self, SignInput};

/// One Converse upstream.
pub struct Bedrock {
    upstream: Upstream,
    base: String,
    models: crate::models::ModelClient,
    region: String,
    access_key_id: String,
    secret_access_key: String,
    session_token: String,
    #[cfg(test)]
    signing: Option<(SystemTime, String)>,
}

impl std::fmt::Debug for Bedrock {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Bedrock")
            .field("id", &http::id_of(&self.upstream))
            .field("base", &self.base)
            .field("region", &self.region)
            .field("session_token", &!self.session_token.is_empty())
            .finish()
    }
}

impl Bedrock {
    /// Builds a client that signs every call with `access_key_id` and
    /// `secret_access_key`.
    ///
    /// `region` is the signing region. Empty parses `bedrock-runtime.<region>.amazonaws.com`
    /// from `endpoint.base_url`. `session_token` is empty when the key is
    /// long-lived. `endpoint.api_key` must be empty.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the id, URL, region, or a credential is
    /// unusable.
    pub fn new(
        endpoint: Endpoint,
        region: &str,
        access_key_id: &str,
        secret_access_key: &str,
        session_token: &str,
    ) -> Result<Self, ProviderError> {
        if !endpoint.api_key.is_empty() {
            return Err(ProviderError::decode(
                "Converse uses AWS credentials, not an API key",
            ));
        }
        let access_key_id = access_key_id.trim().to_owned();
        let secret_access_key = secret_access_key.trim().to_owned();
        let session_token = session_token.trim().to_owned();
        header_safe(&endpoint.id, &access_key_id, "access key id")?;
        header_safe(&endpoint.id, &secret_access_key, "secret access key")?;
        if !session_token.is_empty() {
            header_safe(&endpoint.id, &session_token, "session token")?;
        }
        let base = runtime_root(&endpoint.base_url);
        if base.is_empty() {
            return Err(ProviderError::decode("provider base URL is empty"));
        }
        let region = signing_region(region, &base)?;
        let models = crate::models::ModelClient::new(&endpoint)
            .map_err(|error| ProviderError::decode(error.message()))?;
        let upstream = Upstream::open(endpoint, "", HeaderMap::new())?;
        Ok(Self {
            upstream,
            base,
            models,
            region,
            access_key_id,
            secret_access_key,
            session_token,
            #[cfg(test)]
            signing: None,
        })
    }

    #[cfg(test)]
    fn with_signing(mut self, time: SystemTime, host: &str) -> Self {
        self.signing = Some((time, host.to_owned()));
        self
    }
}

#[async_trait]
impl Provider for Bedrock {
    fn id(&self) -> &str {
        http::id_of(&self.upstream)
    }

    async fn execute(
        &self,
        request: Request,
        inbound_protocol: &str,
    ) -> Result<Completion, ProviderError> {
        let stream = request.stream;
        let codec = Converse;
        let body = http::prepare_body(
            &self.upstream,
            &codec,
            request.clone(),
            None,
            inbound_protocol,
        )?;
        let response = self.converse_response(&request.model, body, stream).await?;
        let status = response.status();
        if !status.is_success() {
            let retry_after = http::retry_after(response.headers());
            let bytes = response.bytes().await.unwrap_or_default();
            let error = ProviderError::status(status.as_u16(), http::message_from(&bytes));
            return Err(match retry_after {
                Some(after) => error.with_retry_after(after),
                None => error,
            });
        }
        if codec.id() == inbound_protocol {
            return http::raw_completion(response, stream, &codec).await;
        }
        if stream {
            let events = crate::stream::SseEvents::new(
                response.bytes_stream(),
                codec.stream_decoder(),
                true,
            );
            return Ok(Completion::Stream(Box::pin(events)));
        }
        let bytes = response.bytes().await.map_err(http::map_body)?;
        let mut decoded = codec
            .decode_response(&bytes)
            .map_err(|error| ProviderError::decode(error.to_string()))?;
        decoded.strip_extensions();
        Ok(Completion::Unary(Box::new(decoded)))
    }

    async fn list_models(
        &self,
        _query: &hofund_plugin_api::ListQuery,
    ) -> Result<hofund_plugin_api::ModelList, hofund_plugin_api::PluginError> {
        crate::models::static_models(
            crate::presets::preset_models("bedrock")
                .map(<[String]>::to_vec)
                .unwrap_or_default(),
        )
    }

    async fn probe(
        &self,
        query: &hofund_plugin_api::ProbeQuery,
    ) -> Result<hofund_plugin_api::ProbeReport, hofund_plugin_api::PluginError> {
        tokio::time::timeout(crate::probe::PROBE_WAIT, self.probe_inner(query))
            .await
            .map_err(|_| {
                hofund_plugin_api::PluginError::timeout("provider probe exceeded 20 seconds")
            })?
    }
}

impl Bedrock {
    async fn probe_inner(
        &self,
        query: &hofund_plugin_api::ProbeQuery,
    ) -> Result<hofund_plugin_api::ProbeReport, hofund_plugin_api::PluginError> {
        use hofund_plugin_api::PluginError;
        let mut body = serde_json::json!({
            "messages": [{"role": "user", "content": [{"text": "hi"}]}],
            "inferenceConfig": {"maxTokens": 16},
        });
        if let Some(extra) = query.extra_body() {
            let extra: serde_json::Value = serde_json::from_slice(extra)
                .map_err(|_| PluginError::decode("probe extra body is not JSON"))?;
            let extra = extra
                .as_object()
                .ok_or_else(|| PluginError::decode("probe extra body is not a JSON object"))?;
            for (key, value) in extra {
                body[key] = value.clone();
            }
        }
        let stream = body
            .get("stream")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let bytes = serde_json::to_vec(&body)
            .map_err(|_| PluginError::decode("could not encode probe body"))?;
        let response = self
            .converse_response(query.model(), bytes, stream)
            .await
            .map_err(plugin_error)?;
        let status = response.status().as_u16();
        let cap = if (200..300).contains(&status) && stream {
            1024 * 1024
        } else {
            4096
        };
        let page = crate::models::read_page(response, cap, false).await?;
        let url = format!(
            "{}{}",
            self.base.trim_end_matches('/'),
            sigv4::model_wire_uri(query.model(), stream)
        );
        self.models.probe_report(&url, page, stream)
    }

    async fn converse_response(
        &self,
        model: &str,
        body: Vec<u8>,
        stream: bool,
    ) -> Result<reqwest::Response, ProviderError> {
        if model.is_empty() {
            return Err(ProviderError::status(400, "model is empty"));
        }
        let path = sigv4::model_wire_uri(model, stream);
        let canonical_path = sigv4::canonical_uri(&path, sigv4::UriEncoding::Double);
        let url = format!("{}{path}", self.base.trim_end_matches('/'));
        let parsed = reqwest::Url::parse(&url)
            .map_err(|error| ProviderError::decode(format!("provider URL is invalid: {error}")))?;
        #[cfg(test)]
        let (signing_time, signing_host) = self
            .signing
            .clone()
            .unwrap_or_else(|| (SystemTime::now(), host_header(&parsed)));
        #[cfg(not(test))]
        let (signing_time, signing_host) = (SystemTime::now(), host_header(&parsed));
        let amz_date = sigv4::amz_date(
            signing_time
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_secs())
                .unwrap_or(0),
        );
        let hash = sigv4::payload_hash(&body);
        let mut ours = vec![
            ("content-type".to_owned(), "application/json".to_owned()),
            ("host".to_owned(), signing_host),
            ("x-amz-content-sha256".to_owned(), hash),
            ("x-amz-date".to_owned(), amz_date.clone()),
        ];
        if stream {
            ours.push((
                "accept".to_owned(),
                "application/vnd.amazon.eventstream".to_owned(),
            ));
        }
        if !self.session_token.is_empty() {
            ours.push((
                "x-amz-security-token".to_owned(),
                self.session_token.clone(),
            ));
        }
        let pairs = merged_headers(&self.upstream, ours);
        let borrowed: Vec<(&str, &str)> = pairs
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        let input = SignInput::new()
            .method("POST")
            .canonical_uri(&canonical_path)
            .amz_date(&amz_date)
            .scope(&self.region, "bedrock")
            .credentials(&self.access_key_id, &self.secret_access_key)
            .payload(&body)
            .headers(&borrowed);
        let signed = sigv4::sign(&input)
            .map_err(|error| ProviderError::decode(format!("Converse signing: {error}")))?;
        let headers = request_headers(&pairs, signed.authorization())?;
        http::post_with_headers(&self.upstream, &url, body, headers).await
    }
}

fn plugin_error(error: ProviderError) -> hofund_plugin_api::PluginError {
    use hofund_plugin_api::PluginError;
    let retry_after = error.retry_after;
    let mut converted = match error.kind {
        crate::ProviderErrorKind::Connect => PluginError::connect(error.message),
        crate::ProviderErrorKind::Timeout => PluginError::timeout(error.message),
        crate::ProviderErrorKind::Transport => PluginError::transport(error.message),
        crate::ProviderErrorKind::Interrupted => PluginError::interrupted(error.message),
        crate::ProviderErrorKind::Status => {
            PluginError::status_error(error.status.unwrap_or(502), error.message)
        }
        crate::ProviderErrorKind::Decode => PluginError::decode(error.message),
        crate::ProviderErrorKind::Unsupported => PluginError::unsupported(error.message),
        crate::ProviderErrorKind::Auth => PluginError::auth(error.message),
        crate::ProviderErrorKind::Quota => PluginError::quota(error.message),
        crate::ProviderErrorKind::Config => PluginError::config(error.message),
        crate::ProviderErrorKind::Cancelled => PluginError::cancelled(error.message),
        crate::ProviderErrorKind::Io => PluginError::io(error.message),
        crate::ProviderErrorKind::Internal => PluginError::internal(error.message),
    };
    if let Some(after) = retry_after {
        converted = converted.with_retry_after(after);
    }
    converted
}

/// Strips an OpenAI or Messages suffix so a pasted runtime URL is the root
/// Converse is appended to.
pub(crate) fn runtime_root(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    let trimmed = trimmed
        .strip_suffix("/openai/v1")
        .or_else(|| trimmed.strip_suffix("/anthropic"))
        .unwrap_or(trimmed);
    trimmed.trim_end_matches('/').to_owned()
}

fn signing_region(explicit: &str, base_url: &str) -> Result<String, ProviderError> {
    let explicit = explicit.trim().to_ascii_lowercase();
    if !explicit.is_empty() {
        check_region(&explicit)?;
        return Ok(explicit);
    }
    let host = reqwest::Url::parse(base_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .unwrap_or_default();
    let Some(region) = host
        .strip_prefix("bedrock-runtime.")
        .and_then(|rest| rest.strip_suffix(".amazonaws.com"))
        .filter(|region| !region.is_empty())
    else {
        return Err(ProviderError::decode(
            "Converse needs a region, or a bedrock-runtime.<region>.amazonaws.com host",
        ));
    };
    check_region(region)?;
    Ok(region.to_owned())
}

fn check_region(region: &str) -> Result<(), ProviderError> {
    let ok = !region.is_empty()
        && region.len() <= 32
        && region
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if ok {
        Ok(())
    } else {
        Err(ProviderError::decode(format!(
            "Converse region `{region}` is not a region id"
        )))
    }
}

fn header_safe(provider: &str, value: &str, what: &str) -> Result<(), ProviderError> {
    if value.is_empty() {
        return Err(ProviderError::decode(format!(
            "provider `{provider}`: Converse {what} is empty"
        )));
    }
    if value
        .bytes()
        .any(|byte| byte.is_ascii_control() || byte == b' ')
    {
        return Err(ProviderError::decode(format!(
            "provider `{provider}`: Converse {what} is not valid header text"
        )));
    }
    Ok(())
}

fn host_header(url: &reqwest::Url) -> String {
    let host = url.host_str().unwrap_or_default();
    let default = match url.scheme() {
        "https" => 443,
        _ => 80,
    };
    match url.port_or_known_default() {
        Some(port) if port != default => format!("{host}:{port}"),
        _ => host.to_owned(),
    }
}

fn merged_headers(upstream: &Upstream, ours: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut map = BTreeMap::new();
    for (name, value) in http::extra_header_pairs(upstream) {
        map.insert(name.to_ascii_lowercase(), value);
    }
    for (name, value) in ours {
        map.insert(name.to_ascii_lowercase(), value);
    }
    map.into_iter().collect()
}

fn request_headers(
    pairs: &[(String, String)],
    authorization: &str,
) -> Result<HeaderMap, ProviderError> {
    let mut headers = HeaderMap::new();
    for (name, value) in pairs {
        if name == "host" {
            continue;
        }
        let header_name = HeaderName::try_from(name.as_str())
            .map_err(|_| ProviderError::decode(format!("header `{name}` is not a valid name")))?;
        let mut header_value = HeaderValue::try_from(value.as_str()).map_err(|_| {
            ProviderError::decode(format!("header `{name}` is not valid header text"))
        })?;
        if name == "x-amz-security-token" {
            header_value.set_sensitive(true);
        }
        headers.insert(header_name, header_value);
    }
    let mut authorization = HeaderValue::try_from(authorization)
        .map_err(|_| ProviderError::decode("AWS authorization is not valid header text"))?;
    authorization.set_sensitive(true);
    headers.insert(reqwest::header::AUTHORIZATION, authorization);
    Ok(headers)
}

#[cfg(test)]
mod tests {
    use super::{runtime_root, signing_region};
    use crate::http::Endpoint;
    use crate::{Bedrock, Provider};
    use hofund_ir::{ContentPart, Request, Role};

    #[test]
    fn a_pasted_chat_url_becomes_the_runtime_root() {
        assert_eq!(
            runtime_root("https://bedrock-runtime.eu-west-1.amazonaws.com/openai/v1/"),
            "https://bedrock-runtime.eu-west-1.amazonaws.com"
        );
        assert_eq!(
            signing_region("", "https://bedrock-runtime.eu-west-1.amazonaws.com").expect("region"),
            "eu-west-1"
        );
    }

    #[tokio::test]
    async fn a_signed_converse_call_posts_the_model_path() {
        let server = wiremock::MockServer::start().await;
        let provider = Bedrock::new(
            Endpoint::new("bedrock", server.uri()),
            "us-east-1",
            "AKIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "",
        )
        .expect("client");
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path_regex(
                "/model/anthropic\\.claude-3-sonnet-20240229-v1%3A0/converse",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "output": {"message": {"role": "assistant", "content": [{"text": "ok"}]}},
                    "stopReason": "end_turn",
                    "usage": {"inputTokens": 1, "outputTokens": 1, "totalTokens": 2}
                })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let mut request = Request::new("anthropic.claude-3-sonnet-20240229-v1:0");
        let mut user = hofund_ir::Message::new(Role::User);
        user.content.push(ContentPart::text("hi"));
        request.messages = vec![user];
        let completion = provider
            .execute(request, "openai-chat")
            .await
            .expect("execute");
        let crate::Completion::Unary(response) = completion else {
            panic!("unary");
        };
        assert_eq!(response.message.content, vec![ContentPart::text("ok")]);
        let requests = server.received_requests().await.expect("requests");
        let authorization = requests[0]
            .headers
            .get("authorization")
            .expect("authorization")
            .to_str()
            .expect("text");
        assert!(authorization.starts_with("AWS4-HMAC-SHA256 "));
        assert!(authorization.contains("/us-east-1/bedrock/aws4_request"));
        assert!(requests[0].headers.get("x-amz-date").is_some());
        assert!(requests[0].headers.get("x-amz-content-sha256").is_some());
        let body: serde_json::Value = requests[0].body_json().expect("body");
        assert_eq!(body["messages"][0]["content"][0]["text"], "hi");
        assert!(body.get("model").is_none());
    }

    #[tokio::test]
    async fn the_adapter_signs_a_colon_path_with_the_botocore_vector() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/model/anthropic.claude-3-5-sonnet-20240620-v1%3A0/converse",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "output": {"message": {"role": "assistant", "content": [{"text": "ok"}]}},
                    "stopReason": "end_turn",
                    "usage": {"inputTokens": 1, "outputTokens": 1, "totalTokens": 2}
                })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let provider = Bedrock::new(
            Endpoint::new("bedrock", server.uri()),
            "us-east-1",
            "AKIDEXAMPLE",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "SESSIONTOKENEXAMPLE",
        )
        .expect("client")
        .with_signing(
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_440_938_160),
            "bedrock-runtime.us-east-1.amazonaws.com",
        );
        let mut request = Request::new("anthropic.claude-3-5-sonnet-20240620-v1:0");
        let mut user = hofund_ir::Message::new(Role::User);
        user.content.push(ContentPart::text("hi"));
        request.messages = vec![user];
        provider
            .execute(request, "openai-chat")
            .await
            .expect("execute");

        let requests = server.received_requests().await.expect("requests");
        assert_eq!(
            requests[0].body,
            br#"{"messages":[{"role":"user","content":[{"text":"hi"}]}]}"#
        );
        // botocore 1.43.118 SigV4Auth for service bedrock, with the port-free
        // signing host pinned above so the expected value is independent of the
        // ephemeral test-server port.
        assert_eq!(
            requests[0]
                .headers
                .get("authorization")
                .expect("authorization")
                .to_str()
                .expect("text"),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/bedrock/aws4_request, SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date;x-amz-security-token, Signature=9acaa85e4b0b79f684cb81d4050d66206f71f74d9b7145bfbaa74b72a2c04655"
        );
        assert_eq!(
            requests[0]
                .headers
                .get("x-amz-date")
                .expect("date")
                .to_str()
                .expect("text"),
            "20150830T123600Z"
        );
    }

    #[test]
    fn an_api_key_is_rejected() {
        let mut endpoint =
            Endpoint::new("bedrock", "https://bedrock-runtime.us-east-1.amazonaws.com");
        endpoint.api_key = "ABSK".to_owned();
        let error = Bedrock::new(endpoint, "us-east-1", "AKIA", "secret", "").expect_err("key");
        assert!(error.to_string().contains("not an API key"));
    }

    #[test]
    fn an_empty_credential_names_the_provider() {
        let endpoint = Endpoint::new(
            "aws-production",
            "https://bedrock-runtime.us-east-1.amazonaws.com",
        );
        let error = Bedrock::new(endpoint, "us-east-1", "", "secret", "").expect_err("empty");
        assert!(
            error
                .to_string()
                .contains("provider `aws-production`: Converse access key id is empty"),
            "{error}"
        );
    }
}
