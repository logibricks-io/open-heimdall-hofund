//! Shared HTTP execution for the protocol adapters.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use hofund_codec::{Codec, CodecError, UNMAPPED_TOOLS};
use hofund_ir::Request;
use hofund_quirks::Profile;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

use crate::Completion;

pub(crate) type Prepare = fn(&[u8]) -> Result<Vec<u8>, CodecError>;
use crate::error::ProviderError;
use crate::stream::SseEvents;
pub(crate) use crate::stream::{map_transport, message_from};

/// Connection settings for one upstream endpoint.
#[derive(Clone, Debug)]
pub struct Endpoint {
    /// Id recorded on usage rows and failover traces.
    pub id: String,
    /// API root. The adapter appends its protocol path.
    pub base_url: String,
    /// Credential sent with each call. Empty when the upstream does not use one.
    pub api_key: String,
    /// Extra headers. Names and values must be valid HTTP header text.
    pub headers: Vec<(String, String)>,
    /// Quirk profile applied to the canonical request before encode.
    pub quirks: Option<&'static Profile>,
    /// Whole-call timeout, including a streaming body that stalls.
    pub timeout: Duration,
}

impl Endpoint {
    /// An endpoint with no key, no extra headers, no quirks, and a 60 second timeout.
    #[must_use]
    pub fn new(id: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            base_url: base_url.into(),
            api_key: String::new(),
            headers: Vec::new(),
            quirks: None,
            timeout: Duration::from_secs(60),
        }
    }
}

#[derive(Debug)]
pub(crate) struct Upstream {
    pub id: String,
    client: reqwest::Client,
    url: String,
    headers: HeaderMap,
    quirks: Option<&'static Profile>,
}

impl Upstream {
    pub(crate) fn open(
        endpoint: Endpoint,
        path: &str,
        mut headers: HeaderMap,
    ) -> Result<Self, ProviderError> {
        if endpoint.id.is_empty() {
            return Err(ProviderError::decode("provider id is empty"));
        }
        if endpoint.base_url.is_empty() {
            return Err(ProviderError::decode("provider base URL is empty"));
        }
        let url = format!(
            "{}/{}",
            endpoint.base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        );
        reqwest::Url::parse(&url)
            .map_err(|error| ProviderError::decode(format!("provider URL is invalid: {error}")))?;
        for (name, value) in endpoint.headers {
            let header_name = match HeaderName::try_from(name.as_str()) {
                Ok(header_name) => header_name,
                Err(_) => {
                    return Err(ProviderError::decode(format!(
                        "invalid header name `{name}`"
                    )));
                }
            };
            let header_value = match HeaderValue::try_from(value.as_str()) {
                Ok(header_value) => header_value,
                Err(_) => {
                    return Err(ProviderError::decode(format!(
                        "invalid header value for `{name}`"
                    )));
                }
            };
            headers.insert(header_name, header_value);
        }
        let client = reqwest::Client::builder()
            .timeout(endpoint.timeout)
            .build()
            .map_err(|error| ProviderError::connect(error.to_string()))?;
        Ok(Self {
            id: endpoint.id,
            client,
            url,
            headers,
            quirks: endpoint.quirks,
        })
    }
}

pub(crate) fn messages_url(upstream: &Upstream) -> &str {
    &upstream.url
}

pub(crate) fn prepare_body(
    upstream: &Upstream,
    codec: &dyn Codec,
    mut request: Request,
    prepare: Option<Prepare>,
    inbound_protocol: &str,
) -> Result<Vec<u8>, ProviderError> {
    if let Some(profile) = upstream.quirks {
        hofund_quirks::apply(profile, &mut request);
    }
    let cross_protocol = codec.id() != inbound_protocol;
    if cross_protocol {
        warn_unmapped(&request);
        crate::billing::strip_billing_header(&mut request);
        request.strip_extensions();
    }
    let mut body = codec.encode_request(&request).map_err(|error| {
        // The client omitted or sent something this protocol cannot encode.
        // That is a bad request, not an upstream failure.
        ProviderError::status(400, error.to_string())
    })?;
    if let Some(prepare) = prepare {
        body = prepare(&body).map_err(|error| ProviderError::decode(error.to_string()))?;
    }
    Ok(body)
}

pub(crate) async fn post_json(
    upstream: &Upstream,
    url: &str,
    body: Vec<u8>,
) -> Result<reqwest::Response, ProviderError> {
    upstream
        .client
        .post(url)
        .headers(upstream.headers.clone())
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
        .map_err(map_transport)
}

pub(crate) async fn execute(
    upstream: &Upstream,
    codec: &dyn Codec,
    request: Request,
    prepare: Option<Prepare>,
    inbound_protocol: &str,
) -> Result<Completion, ProviderError> {
    let cross_protocol = codec.id() != inbound_protocol;
    let streaming = request.stream;
    let body = prepare_body(upstream, codec, request, prepare, inbound_protocol)?;
    let response = post_json(upstream, &upstream.url, body).await?;
    let status = response.status();
    if !status.is_success() {
        let bytes = response.bytes().await.unwrap_or_default();
        return Err(ProviderError::status(status.as_u16(), message_from(&bytes)));
    }
    if streaming {
        let events = SseEvents::new(
            response.bytes_stream(),
            codec.stream_decoder(),
            cross_protocol,
        );
        Ok(Completion::Stream(Box::pin(events)))
    } else {
        let bytes = response.bytes().await.map_err(map_transport)?;
        let mut decoded = codec
            .decode_response(&bytes)
            .map_err(|error| ProviderError::decode(error.to_string()))?;
        if cross_protocol {
            decoded.strip_extensions();
        }
        Ok(Completion::Unary(Box::new(decoded)))
    }
}

/// Adapter identity used by both concrete providers.
pub(crate) fn id_of(upstream: &Upstream) -> &str {
    &upstream.id
}

fn warn_unmapped(request: &Request) {
    let Some(serde_json::Value::Array(items)) = request.extensions.get(UNMAPPED_TOOLS) else {
        return;
    };
    for item in items {
        let Some(name) = item.as_str() else {
            continue;
        };
        if note_dropped(name) {
            eprintln!(
                "warning: dropped tool `{name}`; it has no mapping onto the upstream protocol"
            );
        }
    }
}

/// `true` the first time `name` is dropped in this process.
fn note_dropped(name: &str) -> bool {
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
    let mut seen = seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    seen.insert(name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::note_dropped;

    #[test]
    fn a_dropped_tool_is_noted_once() {
        let name = format!("once-{}", std::process::id());
        assert!(note_dropped(&name));
        assert!(!note_dropped(&name));
        assert!(note_dropped(&format!("{name}-other")));
    }
}
