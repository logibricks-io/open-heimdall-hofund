//! Adapter for upstreams that speak the Generative Language API.
//!
//! The model is a path segment. A unary call posts
//! `{base}/models/{model}:generateContent`. A stream posts
//! `{base}/models/{model}:streamGenerateContent?alt=sse`. The API key goes in
//! `x-goog-api-key`. `model` and `stream` are removed from the JSON body
//! before the POST.
//!
//! Method URLs, checked 2026-10-25:
//! <https://ai.google.dev/api/generate-content#method:-models.generatecontent>
//! and
//! <https://ai.google.dev/api/generate-content#method:-models.streamgeneratecontent>.
//! The key header is
//! <https://ai.google.dev/gemini-api/docs/api-key>.
//! Token counting is
//! <https://ai.google.dev/api/tokens#method:-models.counttokens>.

use async_trait::async_trait;
use hofund_codec::GenerativeLanguage;
use hofund_ir::Request;
use reqwest::header::HeaderMap;

use crate::error::ProviderError;
use crate::http::{self, Endpoint, Upstream};
use crate::{Completion, CountOutcome, Provider};

/// One Generative Language upstream.
#[derive(Debug)]
pub struct Generative {
    upstream: Upstream,
    base: String,
    models: crate::models::ModelClient,
}

impl Generative {
    /// Builds a client.
    ///
    /// `endpoint.base_url` is the root under which `models/{model}:…` sits.
    /// A host with an empty path receives `/v1beta`. A pasted
    /// `:generateContent`, `:streamGenerateContent`, or `/models` URL is cut
    /// back to that root. `endpoint.api_key` is sent as `x-goog-api-key` and
    /// is not also sent as a bearer token.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the id, base URL, or a header is unusable.
    pub fn new(mut endpoint: Endpoint) -> Result<Self, ProviderError> {
        let base = api_base(&endpoint.base_url)?;
        endpoint.base_url.clone_from(&base);
        let mut headers = HeaderMap::new();
        http::insert_credential(
            &mut headers,
            &endpoint.api_key,
            endpoint.credential,
            http::Credential::GoogApiKey,
        )?;
        // The call URL depends on the model, so the path opened here is only
        // a parse check. `execute` posts to the URL it builds.
        let models = crate::models::ModelClient::new(&endpoint)
            .map_err(|error| ProviderError::decode(error.message()))?;
        let upstream = Upstream::open(endpoint, "", headers)?;
        Ok(Self {
            upstream,
            base,
            models,
        })
    }

    fn url_for(&self, model: &str, stream: bool) -> Result<String, ProviderError> {
        let path = method_path(model, stream)?;
        join_url(&self.base, &path)
    }
}

#[async_trait]
impl Provider for Generative {
    fn id(&self) -> &str {
        http::id_of(&self.upstream)
    }

    async fn count_input(
        &self,
        mut request: Request,
        inbound_protocol: &str,
    ) -> Result<CountOutcome, ProviderError> {
        request.stream = false;
        let url = join_url(&self.base, &count_path(&request.model)?)?;
        let body = http::prepare_body(
            &self.upstream,
            &GenerativeLanguage,
            request,
            Some(GenerativeLanguage::prepare_upstream_body),
            inbound_protocol,
        )?;
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
        let url = self.url_for(&request.model, request.stream)?;
        http::execute_at(
            &self.upstream,
            &url,
            &GenerativeLanguage,
            request,
            Some(GenerativeLanguage::prepare_upstream_body),
            inbound_protocol,
        )
        .await
    }

    async fn list_models(
        &self,
        _query: &hofund_plugin_api::ListQuery,
    ) -> Result<hofund_plugin_api::ModelList, hofund_plugin_api::PluginError> {
        self.models.gemini_list().await
    }

    async fn probe(
        &self,
        query: &hofund_plugin_api::ProbeQuery,
    ) -> Result<hofund_plugin_api::ProbeReport, hofund_plugin_api::PluginError> {
        self.models.generative_probe(query).await
    }
}

/// The base a Generative Language URL is kept as.
///
/// Trimmed. A pasted method or model list is cut back to the root. A bare
/// host is given `/v1beta`, which is the version the API's own SDKs add.
fn api_base(raw: &str) -> Result<String, ProviderError> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(ProviderError::decode("provider base URL is empty"));
    }
    let mut url = if trimmed.contains("://") {
        trimmed.to_owned()
    } else {
        format!("https://{trimmed}")
    };
    url = cut_pasted_method(&url);
    url = url.trim_end_matches('/').to_owned();
    let parsed = reqwest::Url::parse(&url)
        .map_err(|error| ProviderError::decode(format!("provider URL is invalid: {error}")))?;
    if parsed.host_str().is_none() {
        return Err(ProviderError::decode("provider URL has no host"));
    }
    if parsed.path().trim_matches('/').is_empty() {
        url = format!("{url}/v1beta");
    }
    reqwest::Url::parse(&url)
        .map_err(|error| ProviderError::decode(format!("provider URL is invalid: {error}")))?;
    Ok(url)
}

/// Cuts a trailing `/models`, `/models/{segment}`, and an optional query or
/// fragment. A longer path is left alone.
fn cut_pasted_method(url: &str) -> String {
    let mut end = url.len();
    while let Some(index) = url[..end].rfind("/models") {
        if method_suffix(&url[index + "/models".len()..]) {
            return url[..index].to_owned();
        }
        if index == 0 {
            break;
        }
        end = index;
    }
    url.to_owned()
}

/// Whether `rest` is the tail of `/models` or `/models/{one segment}`.
fn method_suffix(rest: &str) -> bool {
    if rest.is_empty() || rest.starts_with('?') || rest.starts_with('#') {
        return true;
    }
    let Some(segment) = rest.strip_prefix('/') else {
        return false;
    };
    match segment.find(['/', '?', '#']) {
        None => true,
        Some(end) => {
            let after = &segment[end..];
            after.starts_with('?') || after.starts_with('#')
        }
    }
}

fn method_path(model: &str, stream: bool) -> Result<String, ProviderError> {
    let model = escape_model(model)?;
    if stream {
        Ok(format!("/models/{model}:streamGenerateContent?alt=sse"))
    } else {
        Ok(format!("/models/{model}:generateContent"))
    }
}

fn count_path(model: &str) -> Result<String, ProviderError> {
    Ok(format!("/models/{}:countTokens", escape_model(model)?))
}

pub(crate) fn escape_model(model: &str) -> Result<String, ProviderError> {
    let model = model.trim().strip_prefix("models/").unwrap_or(model.trim());
    if model.is_empty() {
        return Err(ProviderError::status(400, "model is empty"));
    }
    Ok(path_segment(model))
}

/// Percent-encoding for one URL path segment.
///
/// Unreserved characters and `$&+:=@` stay literal, matching the encoding
/// used for a path segment that may contain a colon (a tag) but must not
/// contain a slash.
pub(crate) fn path_segment(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if path_segment_unescaped(byte) {
            out.push(char::from(byte));
        } else {
            const HEX: &[u8; 16] = b"0123456789ABCDEF";
            out.push('%');
            out.push(char::from(HEX[(byte >> 4) as usize]));
            out.push(char::from(HEX[(byte & 0x0f) as usize]));
        }
    }
    out
}

fn path_segment_unescaped(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'-' | b'_' | b'.' | b'~' | b'$' | b'&' | b'+' | b':' | b'=' | b'@'
        )
}

fn join_url(base: &str, path: &str) -> Result<String, ProviderError> {
    let url = format!("{base}{path}");
    reqwest::Url::parse(&url)
        .map_err(|error| ProviderError::decode(format!("provider URL is invalid: {error}")))?;
    Ok(url)
}

pub(crate) fn classify_count(
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
            .get("totalTokens")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| ProviderError::decode("count response has no totalTokens"))?;
        return Ok(CountOutcome::Counted(tokens));
    }
    let error = ProviderError::status(status, http::message_from(bytes));
    Err(match retry_after {
        Some(after) => error.with_retry_after(after),
        None => error,
    })
}

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
mod tests {
    use super::{api_base, count_path, method_path};

    #[test]
    fn a_base_gains_the_version_and_a_pasted_method_is_cut_back() {
        let cases = [
            (
                "generativelanguage.googleapis.com",
                "https://generativelanguage.googleapis.com/v1beta",
            ),
            (
                "https://generativelanguage.googleapis.com/",
                "https://generativelanguage.googleapis.com/v1beta",
            ),
            (
                "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse",
                "https://generativelanguage.googleapis.com/v1beta",
            ),
            (
                "https://relay.example/proxy/v1beta/models",
                "https://relay.example/proxy/v1beta",
            ),
            (
                "https://relay.example/proxy/v1",
                "https://relay.example/proxy/v1",
            ),
            (
                "https://relay.example/v1beta/models/gemini-2.5-pro:generateContent",
                "https://relay.example/v1beta",
            ),
        ];
        for (input, want) in cases {
            assert_eq!(api_base(input).expect("base"), want, "{input}");
        }
        assert!(api_base("  ").is_err());
    }

    #[test]
    fn the_model_is_a_path_segment() {
        assert_eq!(
            method_path("gemini-2.5-flash", false).expect("path"),
            "/models/gemini-2.5-flash:generateContent"
        );
        assert_eq!(
            method_path("models/gemini-2.5-pro", true).expect("path"),
            "/models/gemini-2.5-pro:streamGenerateContent?alt=sse"
        );
        assert_eq!(
            method_path("llama:7b", false).expect("path"),
            "/models/llama:7b:generateContent"
        );
        assert_eq!(
            method_path("vendor/model", false).expect("path"),
            "/models/vendor%2Fmodel:generateContent"
        );
        assert_eq!(
            count_path("models/gemini-2.5-flash").expect("count"),
            "/models/gemini-2.5-flash:countTokens"
        );
        assert!(method_path("  ", false).is_err());
        assert!(method_path("models/", true).is_err());
    }
}
