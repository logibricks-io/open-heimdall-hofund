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

pub(crate) fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    parse_retry_after(value, std::time::SystemTime::now())
}

fn parse_retry_after(value: &str, now: std::time::SystemTime) -> Option<Duration> {
    value
        .parse::<u64>()
        .map(Duration::from_secs)
        .ok()
        .or_else(|| {
            let at = httpdate::parse_http_date(value).ok()?;
            at.duration_since(now).ok()
        })
        .filter(|after| !after.is_zero())
}

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
#[derive(Clone)]
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
    /// A pooled client's reqwest read timeout is instead the daemon-wide
    /// `HttpLimits` read budget. When this deadline exceeds that budget,
    /// reqwest fires first. A private test client uses a read timeout slightly
    /// longer than this deadline. A stream that keeps sending is not cut off.
    pub timeout: Duration,
    /// Host pool client. `None` builds a private client for unit tests.
    ///
    /// The registry factory sets this from [`pooled_reqwest`] so connect and
    /// idle-read timeouts are the daemon's `HttpLimits`.
    pub client: Option<reqwest::Client>,
}

impl std::fmt::Debug for Endpoint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Endpoint")
            .field("id", &self.id)
            .field(
                "base_url",
                &self
                    .base_url
                    .split_once('?')
                    .map_or(self.base_url.clone(), |(base, _)| {
                        format!("{base}?<redacted>")
                    }),
            )
            .field("api_key", &"[redacted]")
            .field("credential", &self.credential)
            .field(
                "headers",
                &self
                    .headers
                    .iter()
                    .map(|(name, _)| (name, "[redacted]"))
                    .collect::<Vec<_>>(),
            )
            .field("quirks", &self.quirks)
            .field("timeout", &self.timeout)
            .field("client", &self.client)
            .finish()
    }
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

pub(crate) struct Upstream {
    pub id: String,
    client: reqwest::Client,
    url: String,
    headers: HeaderMap,
    quirks: Option<&'static Profile>,
}

impl std::fmt::Debug for Upstream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let url = self
            .url
            .split_once('?')
            .map_or(self.url.clone(), |(base, _)| format!("{base}?<redacted>"));
        let headers = self
            .headers
            .keys()
            .map(|name| (name, "[redacted]"))
            .collect::<Vec<_>>();
        formatter
            .debug_struct("Upstream")
            .field("id", &self.id)
            .field("client", &self.client)
            .field("url", &url)
            .field("headers", &headers)
            .field("quirks", &self.quirks)
            .finish()
    }
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
            let mut header_value = match HeaderValue::try_from(value.as_str()) {
                Ok(header_value) => header_value,
                Err(_) => {
                    return Err(ProviderError::decode(format!(
                        "invalid header value for `{name}`"
                    )));
                }
            };
            header_value.set_sensitive(true);
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
    send_builder(
        upstream
            .client
            .post(url)
            .headers(upstream.headers.clone())
            .headers(extra)
            .body(body),
    )
    .await
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
    send_builder(builder).await
}

/// Sends `builder`, retrying once only when connection establishment failed.
///
/// A connect error proves that no request was sent. Errors after a write are
/// never retried because the upstream may have processed a non-idempotent POST.
/// Hyper separately retries requests it cancelled on an idle socket before
/// writing them.
pub(crate) async fn send_builder(
    builder: reqwest::RequestBuilder,
) -> Result<reqwest::Response, ProviderError> {
    let retry = builder.try_clone();
    match builder.send().await {
        Ok(response) => Ok(response),
        Err(error) if error.is_connect() => match retry {
            Some(again) => again.send().await.map_err(map_transport),
            None => Err(map_transport(error)),
        },
        Err(error) => Err(map_transport(error)),
    }
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
        let retry_after = retry_after(response.headers());
        let bytes = response.bytes().await.unwrap_or_default();
        let error = ProviderError::status(status.as_u16(), message_from(&bytes));
        return Err(match retry_after {
            Some(after) => error.with_retry_after(after),
            None => error,
        });
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
    use std::time::Duration;

    use tokio::io::AsyncReadExt;

    use super::{note_dropped, parse_retry_after, send_builder};

    #[test]
    fn retry_after_parses_seconds_and_http_dates_at_a_fixed_time() {
        let now = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let date = httpdate::fmt_http_date(now + Duration::from_secs(120));
        assert_eq!(
            parse_retry_after("120", now),
            Some(Duration::from_secs(120))
        );
        assert_eq!(
            parse_retry_after(&date, now),
            Some(Duration::from_secs(120))
        );
        assert_eq!(
            parse_retry_after("7200", now),
            Some(Duration::from_secs(7200))
        );
        assert_eq!(
            parse_retry_after(&u64::MAX.to_string(), now),
            Some(Duration::from_secs(u64::MAX))
        );
        assert_eq!(parse_retry_after("invalid", now), None);
        assert_eq!(parse_retry_after("0", now), None);
        assert_eq!(parse_retry_after(&httpdate::fmt_http_date(now), now), None);
    }

    #[test]
    fn a_dropped_tool_is_noted_once() {
        let name = format!("once-{}", std::process::id());
        assert!(note_dropped(&name));
        assert!(!note_dropped(&name));
        assert!(note_dropped(&format!("{name}-other")));
    }

    #[tokio::test]
    async fn a_post_closed_after_the_full_request_is_not_replayed() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("request connection");
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
            assert!(request.starts_with(b"POST /v1/chat/completions HTTP/1.1\r\n"));
            assert!(request.ends_with(b"{\"model\":\"demo\"}"));
            listener
        });
        let client = reqwest::Client::new();
        let result = send_builder(
            client
                .post(format!("http://{address}/v1/chat/completions"))
                .body(r#"{"model":"demo"}"#),
        )
        .await;
        assert!(result.is_err(), "a closed response connection must fail");
        let listener = server.await.expect("server");
        assert!(
            tokio::time::timeout(Duration::from_millis(150), listener.accept())
                .await
                .is_err(),
            "the POST was replayed on a second connection"
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
}
