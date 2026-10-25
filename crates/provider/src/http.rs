//! Shared HTTP execution for the protocol adapters.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use futures_util::StreamExt;
use hofund_codec::{Codec, CodecError, UNMAPPED_TOOLS};
use hofund_ir::Request;
use hofund_quirks::Profile;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

use crate::Completion;

pub(crate) type Prepare = fn(&[u8]) -> Result<Vec<u8>, CodecError>;
use crate::error::ProviderError;
use crate::stream::SseEvents;
pub(crate) use crate::stream::{map_transport, message_from};

/// Where an API key is written.
///
/// [`Credential::Default`] keeps the adapter's historical header: `Authorization:
/// Bearer` for Chat and Responses, `x-api-key` for Messages.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Credential {
    /// The adapter's own header.
    #[default]
    Default,
    /// `Authorization: Bearer`.
    Bearer,
    /// `api-key`. Azure OpenAI uses this. A bearer token there is an Entra ID token.
    ApiKey,
    /// `x-api-key` alone.
    XApiKey,
    /// `x-api-key` and `Authorization: Bearer` together.
    ///
    /// Third-party Messages gateways disagree about which one they accept, so a
    /// preset that is not Anthropic's own host sends both.
    MessagesBoth,
    /// `x-goog-api-key`.
    ///
    /// The Generative Language API treats `Authorization: Bearer` as an OAuth
    /// access token and rejects an API key sent that way.
    GoogApiKey,
}

/// Connection settings for one upstream endpoint.
#[derive(Clone, Debug)]
pub struct Endpoint {
    /// Id recorded on usage rows and failover traces.
    pub id: String,
    /// API root. The adapter appends its protocol path.
    pub base_url: String,
    /// Credential sent with each call. Empty when the upstream does not use one.
    pub api_key: String,
    /// Which header carries [`Self::api_key`].
    pub credential: Credential,
    /// Extra headers. Names and values must be valid HTTP header text.
    pub headers: Vec<(String, String)>,
    /// Quirk profile applied to the canonical request before encode.
    pub quirks: Option<&'static Profile>,
    /// Idle deadline the kernel enforces between body chunks.
    ///
    /// Reqwest's read timeout is slightly longer, so a stall is reported by
    /// the kernel. A stream that keeps sending is not cut off.
    pub timeout: Duration,
    /// Host pool client. `None` builds a private client for unit tests.
    ///
    /// The registry factory sets this from [`pooled_reqwest`] so connect and
    /// idle-read timeouts are the daemon's `HttpLimits`.
    pub client: Option<reqwest::Client>,
}

impl Endpoint {
    /// An endpoint with no key, no extra headers, no quirks, and a 60 second timeout.
    #[must_use]
    pub fn new(id: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            base_url: base_url.into(),
            api_key: String::new(),
            credential: Credential::Default,
            headers: Vec::new(),
            quirks: None,
            timeout: Duration::from_secs(60),
            client: None,
        }
    }
}

/// The pooled `reqwest` client stored by the host, if `value` is one.
///
/// The host handle returns the client as a type-erased value. Adapters
/// downcast it here so the registry crate does not depend on `reqwest`.
#[must_use]
pub fn pooled_reqwest(value: &dyn std::any::Any) -> Option<reqwest::Client> {
    value.downcast_ref::<reqwest::Client>().cloned()
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
        let client = match endpoint.client.clone() {
            Some(client) => client,
            None => reqwest::Client::builder()
                .connect_timeout(endpoint.timeout)
                .read_timeout(socket_backstop(endpoint.timeout))
                .build()
                .map_err(|error| ProviderError::connect(error.to_string()))?,
        };
        Ok(Self {
            id: endpoint.id,
            client,
            url,
            headers,
            quirks: endpoint.quirks,
        })
    }
}

pub(crate) fn insert_credential(
    headers: &mut HeaderMap,
    api_key: &str,
    credential: Credential,
    default: Credential,
) -> Result<(), ProviderError> {
    if api_key.is_empty() {
        return Ok(());
    }
    let style = match credential {
        Credential::Default => default,
        other => other,
    };
    match style {
        Credential::Default | Credential::Bearer => {
            insert_header(
                headers,
                reqwest::header::AUTHORIZATION,
                format!("Bearer {api_key}"),
            )?;
        }
        Credential::ApiKey => {
            insert_header(
                headers,
                HeaderName::from_static("api-key"),
                api_key.to_owned(),
            )?;
        }
        Credential::XApiKey => {
            insert_header(
                headers,
                HeaderName::from_static("x-api-key"),
                api_key.to_owned(),
            )?;
        }
        Credential::MessagesBoth => {
            insert_header(
                headers,
                HeaderName::from_static("x-api-key"),
                api_key.to_owned(),
            )?;
            insert_header(
                headers,
                reqwest::header::AUTHORIZATION,
                format!("Bearer {api_key}"),
            )?;
        }
        Credential::GoogApiKey => {
            insert_header(
                headers,
                HeaderName::from_static("x-goog-api-key"),
                api_key.to_owned(),
            )?;
        }
    }
    Ok(())
}

fn insert_header(
    headers: &mut HeaderMap,
    name: HeaderName,
    value: String,
) -> Result<(), ProviderError> {
    let mut value = HeaderValue::try_from(value)
        .map_err(|_| ProviderError::decode("API key is not valid header text"))?;
    value.set_sensitive(true);
    headers.insert(name, value);
    Ok(())
}

pub(crate) fn messages_url(upstream: &Upstream) -> &str {
    &upstream.url
}

pub(crate) fn request_url(upstream: &Upstream) -> &str {
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
    send_json(upstream, url, body, None).await
}

pub(crate) async fn post_with_headers(
    upstream: &Upstream,
    url: &str,
    body: Vec<u8>,
    extra: HeaderMap,
) -> Result<reqwest::Response, ProviderError> {
    upstream
        .client
        .post(url)
        .headers(upstream.headers.clone())
        .headers(extra)
        .body(body)
        .send()
        .await
        .map_err(map_transport)
}

/// Header pairs captured at [`Upstream::open`], excluding `Authorization`.
///
/// A signer includes these so a caller-supplied header is part of the
/// signature instead of an unsigned extra.
pub(crate) fn extra_header_pairs(upstream: &Upstream) -> Vec<(String, String)> {
    upstream
        .headers
        .iter()
        .filter(|(name, _)| name.as_str() != "authorization")
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_owned(), value.to_owned()))
        })
        .collect()
}

pub(crate) async fn post_bearer(
    upstream: &Upstream,
    url: &str,
    body: Vec<u8>,
    bearer: &str,
) -> Result<reqwest::Response, ProviderError> {
    send_json(upstream, url, body, Some(bearer)).await
}

async fn send_json(
    upstream: &Upstream,
    url: &str,
    body: Vec<u8>,
    bearer: Option<&str>,
) -> Result<reqwest::Response, ProviderError> {
    let mut builder = upstream
        .client
        .post(url)
        .headers(upstream.headers.clone())
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body);
    if let Some(token) = bearer {
        builder = builder.header(reqwest::header::AUTHORIZATION, bearer_header(token)?);
    }
    builder.send().await.map_err(map_transport)
}

fn bearer_header(token: &str) -> Result<HeaderValue, ProviderError> {
    let mut value = HeaderValue::try_from(format!("Bearer {token}"))
        .map_err(|_| ProviderError::decode("access token is not valid header text"))?;
    value.set_sensitive(true);
    Ok(value)
}

pub(crate) async fn execute(
    upstream: &Upstream,
    codec: &dyn Codec,
    request: Request,
    prepare: Option<Prepare>,
    inbound_protocol: &str,
) -> Result<Completion, ProviderError> {
    let url = upstream.url.clone();
    execute_at(upstream, &url, codec, request, prepare, inbound_protocol).await
}

/// Posts `request` to `url` with `bearer` in `Authorization`.
///
/// The header is marked sensitive. It is not taken from the map captured at
/// [`Upstream::open`], because a minted token expires.
pub(crate) async fn execute_bearer(
    upstream: &Upstream,
    url: &str,
    codec: &dyn Codec,
    request: Request,
    prepare: Option<Prepare>,
    inbound_protocol: &str,
    bearer: &str,
) -> Result<Completion, ProviderError> {
    finish(
        upstream,
        url,
        codec,
        request,
        prepare,
        inbound_protocol,
        Some(bearer),
    )
    .await
}

/// Posts `request` to `url` with the upstream's headers.
///
/// The Generative Language adapter builds a URL per model. A Mantle chat or
/// Responses call may also replace the `/v1` prefix. Every other Chat,
/// Responses, and Messages call keeps the URL fixed at [`Upstream::open`].
pub(crate) async fn execute_at(
    upstream: &Upstream,
    url: &str,
    codec: &dyn Codec,
    request: Request,
    prepare: Option<Prepare>,
    inbound_protocol: &str,
) -> Result<Completion, ProviderError> {
    finish(
        upstream,
        url,
        codec,
        request,
        prepare,
        inbound_protocol,
        None,
    )
    .await
}

async fn finish(
    upstream: &Upstream,
    url: &str,
    codec: &dyn Codec,
    request: Request,
    prepare: Option<Prepare>,
    inbound_protocol: &str,
    bearer: Option<&str>,
) -> Result<Completion, ProviderError> {
    let cross_protocol = codec.id() != inbound_protocol;
    let streaming = request.stream;
    let body = prepare_body(upstream, codec, request, prepare, inbound_protocol)?;
    let response = send_json(upstream, url, body, bearer).await?;
    let status = response.status();
    if !status.is_success() {
        let bytes = response.bytes().await.unwrap_or_default();
        return Err(ProviderError::status(status.as_u16(), message_from(&bytes)));
    }
    if !cross_protocol {
        return raw_completion(response, streaming, codec).await;
    }
    // Cross-protocol only. Extensions are stripped on the way back.
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
        decoded.strip_extensions();
        Ok(Completion::Unary(Box::new(decoded)))
    }
}

pub(crate) async fn raw_completion(
    response: reqwest::Response,
    streaming: bool,
    codec: &dyn Codec,
) -> Result<Completion, ProviderError> {
    if streaming {
        return raw_stream(response, codec).await;
    }
    let bytes = response.bytes().await.map_err(map_transport)?;
    Ok(Completion::Raw(Box::pin(futures_util::stream::once(
        async move { Ok(bytes) },
    ))))
}

async fn raw_stream(
    response: reqwest::Response,
    codec: &dyn Codec,
) -> Result<Completion, ProviderError> {
    let mut incoming = response.bytes_stream();
    let mut decoder = codec.stream_decoder();
    let mut buffered = Vec::new();
    loop {
        match incoming.next().await {
            None => break,
            Some(Err(error)) => return Err(map_transport(error)),
            Some(Ok(chunk)) => {
                let events = match decoder.push(&chunk) {
                    Ok(events) => events,
                    Err(_) => {
                        buffered.push(chunk);
                        break;
                    }
                };
                buffered.push(chunk);
                if events.is_empty() {
                    continue;
                }
                if let Some(error) = crate::stream::failure_of(events.first().expect("event")) {
                    return Err(error);
                }
                break;
            }
        }
    }
    let head = futures_util::stream::iter(buffered.into_iter().map(Ok));
    let rest = incoming.map(map_chunk);
    Ok(Completion::Raw(Box::pin(head.chain(rest))))
}

/// Reqwest's read timeout sits 50ms past the kernel's idle deadline, so the
/// kernel is the one that reports a stall. There is no whole-call cap.
pub(crate) fn socket_backstop(idle: Duration) -> Duration {
    idle.saturating_add(Duration::from_millis(50))
}

fn map_chunk(chunk: Result<bytes::Bytes, reqwest::Error>) -> Result<bytes::Bytes, ProviderError> {
    chunk.map_err(map_transport)
}

/// Adapter identity used by the concrete providers.
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
