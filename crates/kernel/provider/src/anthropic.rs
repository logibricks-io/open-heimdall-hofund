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

    fn close_idle_connections(&self) {
        self.upstream.close_idle_connections();
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
            .map_err(|error| mark_messages_account_off(&self.direct, error))
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
        .map_err(|error| mark_messages_account_off(&self.direct, error))
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

/// Official Messages host. A relay that speaks the same protocol is not this.
const OFFICIAL_MESSAGES_HOST: &str = "api.anthropic.com";

/// Rests an official-host account the vendor turned off.
///
/// The same sentences from any other host stay an ordinary failure: they
/// describe the relay's own account, not this key. The words are matched
/// here, where the configured host is known, and the router only sees
/// [`ProviderError::account_restricted`].
pub(crate) fn mark_messages_account_off(base_url: &str, error: ProviderError) -> ProviderError {
    let Some(status) = error.status else {
        return error;
    };
    if !(400..500).contains(&status) || !official_messages_host(base_url) {
        return error;
    }
    if !account_off_words(&error.message) {
        return error;
    }
    error.account_restricted()
}

fn official_messages_host(base_url: &str) -> bool {
    host_of(base_url).is_some_and(|host| host.eq_ignore_ascii_case(OFFICIAL_MESSAGES_HOST))
}

fn host_of(base_url: &str) -> Option<&str> {
    let rest = base_url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let hostport = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = if let Some(inner) = hostport.strip_prefix('[') {
        inner.split(']').next()?
    } else {
        hostport.split(':').next()?
    };
    if host.is_empty() { None } else { Some(host) }
}

/// Sentences an official account uses when it can serve nothing more.
///
/// Either apostrophe, or "cannot". Case is ignored. Kept in step with the
/// subscription bridge classifier.
fn account_off_words(message: &str) -> bool {
    let folded = fold_account_off(message);
    if folded.contains("this organization has been disabled")
        || folded.contains("organization has disabled claude subscription access")
        || folded.contains("oauth authentication is currently not allowed for this organization")
    {
        return true;
    }
    let cannot = folded.replace("can't", "cannot");
    cannot.contains("account is on hold and cannot use claude")
        || cannot.contains("account cannot receive replies from claude")
        || cannot.contains("account cannot receive responses from claude")
}

fn fold_account_off(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    for ch in message.chars() {
        let ch = if ch == '\u{2019}' { '\'' } else { ch };
        out.extend(ch.to_lowercase());
    }
    out.replace("\\u{2019}", "'").replace("\\u2019", "'")
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

    fn variants(message: &str) -> Vec<String> {
        let mut out = vec![message.to_owned(), message.to_ascii_uppercase()];
        if message.contains("can't") {
            out.push(message.replace("can't", "can\u{2019}t"));
            out.push(message.replace("can't", "cannot"));
            out.push(message.replace("can't", "can\\u2019t"));
        }
        out
    }

    #[test]
    fn d234_official_host_marks_each_account_off_phrase_on_400_and_403() {
        let phrases = [
            "This organization has been disabled.",
            "Your account is on hold and can't use Claude Code.",
            "Your organization has disabled Claude subscription access",
            "OAuth authentication is currently not allowed for this organization.",
            "Your account can't receive replies from Claude.",
            "Your account can't receive responses from Claude.",
        ];
        for phrase in phrases {
            for message in variants(phrase) {
                for status in [400_u16, 403] {
                    let error = super::mark_messages_account_off(
                        "https://api.anthropic.com",
                        crate::error::ProviderError::status(status, message.clone()),
                    );
                    assert!(error.is_account_restricted(), "{status} {message}");
                    assert_eq!(error.status, Some(status), "{message}");
                    assert!(error.cools(), "{message}");
                }
            }
        }
    }

    #[test]
    fn d234_a_relay_host_and_a_5xx_stay_ordinary() {
        for base in [
            "https://relay.example.com",
            "http://127.0.0.1:9",
            "https://api.anthropic.com.evil.example",
            "https://not-api.anthropic.com",
            "not a url",
            "",
        ] {
            let error = super::mark_messages_account_off(
                base,
                crate::error::ProviderError::status(400, "This organization has been disabled."),
            );
            assert!(!error.is_account_restricted(), "{base}");
        }
        let server = super::mark_messages_account_off(
            "https://api.anthropic.com/v1",
            crate::error::ProviderError::status(500, "This organization has been disabled."),
        );
        assert!(!server.is_account_restricted());
    }

    #[test]
    fn d234_extra_usage_and_usage_policy_are_not_account_off() {
        for message in [
            "Third-party apps now draw from your extra usage, not your plan limits.",
            "unable to respond to this request, which appears to violate our Usage Policy",
        ] {
            let error = super::mark_messages_account_off(
                "https://API.ANTHROPIC.COM:443/v1",
                crate::error::ProviderError::status(400, message),
            );
            assert!(!error.is_account_restricted(), "{message}");
        }
    }

    #[test]
    fn d234_official_host_edges() {
        for base in [
            "https://api.anthropic.com",
            "https://api.anthropic.com/v1",
            "https://api.anthropic.com:443",
            "https://user:secret@api.anthropic.com/v1",
            "https://API.ANTHROPIC.COM",
        ] {
            assert!(super::official_messages_host(base), "{base}");
        }
        assert!(!super::official_messages_host(
            "https://api.anthropic.com.evil.example"
        ));
    }
}
