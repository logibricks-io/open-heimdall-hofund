//! Shared HTTP execution for the protocol adapters.

use futures_util::StreamExt;
use hofund_codec::{Codec, CodecError, UNMAPPED_TOOLS};
use hofund_ir::Request;
use hofund_plugin_sdk::transport::{
    parse_reset_after_headers, parse_retry_after, parse_retry_after_headers,
};
use hofund_quirks::Profile;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::Completion;

pub(crate) type Prepare = fn(&[u8]) -> Result<Vec<u8>, CodecError>;
use crate::error::ProviderError;
use crate::stream::SseEvents;
pub(crate) use crate::stream::{map_body, map_transport, message_from};

fn model_from_body(body: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    value
        .get("model")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

pub(crate) fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    parse_retry_after_headers(headers, std::time::SystemTime::now())
}

fn explicit_retry_after(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| parse_retry_after(value, std::time::SystemTime::now()))
}

pub(crate) fn reset_after(headers: &HeaderMap) -> Option<Duration> {
    parse_reset_after_headers(headers, std::time::SystemTime::now())
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
    /// Catalog preset id, when this endpoint came from one.
    pub preset: Option<String>,
    /// Credential sent with each call. Empty when the upstream does not use one.
    pub api_key: String,
    /// Which header carries [`Self::api_key`].
    pub credential: Credential,
    /// Extra headers. Names and values must be valid HTTP header text.
    pub headers: Vec<(String, String)>,
    /// Client header names to copy from each request.
    pub pass_headers: Vec<String>,
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
    /// Shared handle for a host-pooled client. Every upstream on this pool key
    /// must use the same slot so a regional refresh reaches every holder.
    pub client_slot: Option<PooledClientSlot>,
    /// Rebuilds the pooled client after a regional refusal. Set with the client
    /// when the host pool supplied it.
    pub client_refresh: Option<ClientRefresh>,
}

/// Shared host-pool client for one profile and limits pair.
pub type PooledClientSlot = Arc<std::sync::RwLock<reqwest::Client>>;

/// Replaces an upstream HTTP client without changing its settings.
pub type ClientRefresh = Arc<dyn Fn() -> Result<reqwest::Client, ProviderError> + Send + Sync>;

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
            .field("preset", &self.preset)
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
            preset: None,
            api_key: String::new(),
            credential: Credential::Default,
            headers: Vec::new(),
            pass_headers: Vec::new(),
            quirks: None,
            timeout: Duration::from_secs(60),
            client: None,
            client_slot: None,
            client_refresh: None,
        }
    }
}

/// The pooled `reqwest` client stored by the host, if `value` is one.
///
/// The host handle returns the client as a type-erased value. Adapters
/// downcast it here so the registry crate does not depend on `reqwest`.
#[must_use]
pub fn pooled_reqwest(value: &dyn std::any::Any) -> Option<reqwest::Client> {
    if let Some(slot) = pooled_client_slot(value) {
        return slot.read().ok().map(|guard| guard.clone());
    }
    if let Some(client) = value.downcast_ref::<Arc<reqwest::Client>>() {
        return Some((**client).clone());
    }
    value.downcast_ref::<reqwest::Client>().cloned()
}

/// The shared pooled client behind a host handle, when `value` is one.
#[must_use]
pub fn pooled_client_slot(value: &dyn std::any::Any) -> Option<PooledClientSlot> {
    value.downcast_ref::<PooledClientSlot>().cloned()
}

#[derive(Debug)]
enum ClientStore {
    Private(Mutex<reqwest::Client>),
    Pooled(PooledClientSlot),
}

impl ClientStore {
    fn current(&self) -> Result<reqwest::Client, ProviderError> {
        match self {
            Self::Private(mutex) => mutex
                .lock()
                .map_err(|_| ProviderError::internal("upstream client lock is poisoned"))
                .map(|guard| guard.clone()),
            Self::Pooled(slot) => slot
                .read()
                .map_err(|_| ProviderError::internal("upstream client lock is poisoned"))
                .map(|guard| guard.clone()),
        }
    }

    fn replace(&self, client: reqwest::Client) -> Result<(), ProviderError> {
        match self {
            Self::Private(mutex) => {
                *mutex
                    .lock()
                    .map_err(|_| ProviderError::internal("upstream client lock is poisoned"))? =
                    client;
            }
            Self::Pooled(slot) => {
                *slot
                    .write()
                    .map_err(|_| ProviderError::internal("upstream client lock is poisoned"))? =
                    client;
            }
        }
        Ok(())
    }
}

impl Upstream {
    pub(crate) fn close_idle_connections(&self) {
        let fresh = if let Some(refresh) = &self.client_refresh {
            refresh()
        } else {
            build_private_client(self.connect_timeout, self.read_timeout)
        };
        if let Ok(client) = fresh {
            let _ = self.client.replace(client);
        }
    }
}

pub(crate) fn build_private_client(
    connect_timeout: Duration,
    read_timeout: Duration,
) -> Result<reqwest::Client, ProviderError> {
    reqwest::Client::builder()
        .http1_only()
        .connect_timeout(connect_timeout)
        .read_timeout(read_timeout)
        .build()
        .map_err(|error| ProviderError::connect(error.to_string()))
}

pub(crate) struct Upstream {
    pub id: String,
    client: ClientStore,
    client_refresh: Option<ClientRefresh>,
    connect_timeout: Duration,
    read_timeout: Duration,
    url: String,
    headers: HeaderMap,
    pass_headers: Vec<String>,
    quirks: Option<&'static Profile>,
    base_url: String,
    preset: Option<String>,
    state: Arc<hofund_quirks::RequestState>,
    allocation_quota_rate: bool,
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
        let client_refresh = endpoint.client_refresh.clone();
        let client = if let Some(slot) = endpoint.client_slot {
            ClientStore::Pooled(slot)
        } else if let Some(client) = endpoint.client {
            ClientStore::Private(Mutex::new(client))
        } else {
            ClientStore::Private(Mutex::new(build_private_client(
                endpoint.timeout,
                socket_backstop(endpoint.timeout),
            )?))
        };
        Ok(Self {
            id: endpoint.id,
            client,
            client_refresh,
            connect_timeout: endpoint.timeout,
            read_timeout: socket_backstop(endpoint.timeout),
            url,
            headers,
            pass_headers: endpoint.pass_headers.clone(),
            quirks: endpoint.quirks,
            base_url: endpoint.base_url,
            preset: endpoint.preset.clone(),
            state: Arc::new(hofund_quirks::RequestState::default()),
            allocation_quota_rate: matches!(endpoint.preset.as_deref(), Some("qwen" | "qwen-cn")),
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
    hofund_quirks::apply_before_encode(
        hofund_quirks::EncodedContext {
            base_url: &upstream.base_url,
            preset: upstream.preset.as_deref(),
            protocol: codec.id(),
        },
        &mut request,
    );
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
    Ok(hofund_quirks::apply_encoded_with_state(
        hofund_quirks::EncodedContext {
            base_url: &upstream.base_url,
            preset: upstream.preset.as_deref(),
            protocol: codec.id(),
        },
        &upstream.state,
        &body,
    ))
}

pub(crate) async fn post_json(
    upstream: &Upstream,
    url: &str,
    body: Vec<u8>,
) -> Result<reqwest::Response, ProviderError> {
    send_json(upstream, url, body, upstream.headers.clone()).await
}

fn outbound_headers(upstream: &Upstream, request: &Request) -> HeaderMap {
    let mut headers = upstream.headers.clone();
    if let Some(client) = crate::pass_headers::client_map(request) {
        crate::pass_headers::apply(&upstream.pass_headers, &client, &mut headers);
    }
    headers
}

async fn send_json(
    upstream: &Upstream,
    url: &str,
    body: Vec<u8>,
    headers: HeaderMap,
) -> Result<reqwest::Response, ProviderError> {
    let builder = upstream
        .client
        .current()
        .map_err(|_| ProviderError::internal("upstream client lock is poisoned"))?
        .post(url)
        .headers(headers)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body);
    send_builder(builder).await
}

/// Sends `builder`, retrying once only when connection establishment failed.
///
/// A connect error proves that no request was sent. Errors after a write are
/// never retried because the upstream may have processed a non-idempotent POST.
/// The reference records when the body was written; a reply after that belongs
/// to the vendor (`reference: internal/gateway/gateway.go:2753-2754`). Body
/// reads happen only after this function returns, so an interrupted body is
/// never replayed here.
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

pub(crate) async fn execute_with_attempt(
    upstream: &Upstream,
    codec: &dyn Codec,
    request: Request,
    prepare: Option<Prepare>,
    inbound_protocol: &str,
    route: &crate::AttemptRoute,
    hook: Option<&dyn crate::AttemptHook>,
) -> Result<Completion, ProviderError> {
    let url = upstream.url.clone();
    finish(
        upstream,
        &url,
        codec,
        request,
        prepare,
        inbound_protocol,
        route,
        hook,
    )
    .await
}

/// Posts `request` to `url` with the upstream's headers.
///
/// The Generative Language adapter builds a URL per model. Every Chat,
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
        &crate::AttemptRoute::default(),
        None,
    )
    .await
}

/// A successful upstream response waiting for its body to be completed.
pub(crate) struct Sent<'a> {
    response: reqwest::Response,
    streaming: bool,
    cross_protocol: bool,
    codec: &'a dyn Codec,
    allocation_quota_rate: bool,
}

impl Sent<'_> {
    /// Reads or streams the successful response into a completion.
    pub(crate) async fn complete(self) -> Result<Completion, ProviderError> {
        if !self.cross_protocol {
            return raw_completion(
                self.response,
                self.streaming,
                self.codec,
                self.allocation_quota_rate,
            )
            .await;
        }
        // Cross-protocol only. Extensions are stripped on the way back.
        if self.streaming {
            let events = SseEvents::new(
                self.response.bytes_stream(),
                self.codec.stream_decoder(),
                self.cross_protocol,
            );
            Ok(Completion::Stream(Box::pin(events)))
        } else {
            let bytes = self.response.bytes().await.map_err(map_body)?;
            if let Some(error) =
                hofund_plugin_sdk::transport::carried_error(&bytes, self.allocation_quota_rate)
            {
                return Err(ProviderError::from_plugin(error));
            }
            let mut decoded = self
                .codec
                .decode_response(&bytes)
                .map_err(|error| ProviderError::decode(error.to_string()))?;
            decoded.strip_extensions();
            Ok(Completion::Unary(Box::new(decoded)))
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn finish(
    upstream: &Upstream,
    url: &str,
    codec: &dyn Codec,
    request: Request,
    prepare: Option<Prepare>,
    inbound_protocol: &str,
    route: &crate::AttemptRoute,
    hook: Option<&dyn crate::AttemptHook>,
) -> Result<Completion, ProviderError> {
    send(
        upstream,
        url,
        codec,
        request,
        prepare,
        inbound_protocol,
        route,
        hook,
    )
    .await?
    .complete()
    .await
}

#[allow(clippy::too_many_arguments)]
async fn send<'a>(
    upstream: &Upstream,
    url: &str,
    codec: &'a dyn Codec,
    request: Request,
    prepare: Option<Prepare>,
    inbound_protocol: &str,
    route: &crate::AttemptRoute,
    hook: Option<&dyn crate::AttemptHook>,
) -> Result<Sent<'a>, ProviderError> {
    let cross_protocol = codec.id() != inbound_protocol;
    let streaming = request.stream;
    let model = request.model.clone();
    let outbound = outbound_headers(upstream, &request);
    let whole = prepare_body(upstream, codec, request, prepare, inbound_protocol)?;
    let shortened = hook.and_then(|hook| hook.prepare(route, &whole));
    let delta = shortened.is_some();
    let mut body = shortened.unwrap_or_else(|| whole.clone());
    let mut response = send_json(upstream, url, body.clone(), outbound.clone()).await?;
    let mut root_retried = false;
    let mut token_retried = false;
    let mut replacement_retried = false;
    let mut beta_retried = false;
    let mut thinking_back_retried = false;
    while !response.status().is_success() {
        let status = response.status();
        let retry_after = explicit_retry_after(response.headers());
        let reset_after = reset_after(response.headers());
        let bytes = response.bytes().await.unwrap_or_default();
        if status.as_u16() == 400
            && !cross_protocol
            && codec.id() == "openai-responses"
            && let Some(field) = hofund_quirks::refused_passthrough(&bytes)
        {
            let (stripped, changed) = hofund_quirks::without_passthrough(&body, &field);
            if changed && upstream.state.learn_passthrough(&field) {
                body = stripped;
                response = send_json(upstream, url, body.clone(), outbound.clone()).await?;
                continue;
            }
        }
        if status.as_u16() == 400 && !root_retried && hofund_quirks::root_union_refusal(&bytes) {
            let (folded, changed) = hofund_quirks::fold_encoded(codec.id(), &body);
            if changed {
                root_retried = true;
                upstream.state.learn_root_union(&model);
                body = folded;
                response = send_json(upstream, url, body.clone(), outbound.clone()).await?;
                continue;
            }
        }
        if status.as_u16() == 400
            && codec.id() == "openai-chat"
            && !token_retried
            && let Some(wanted) = hofund_quirks::token_field_wanted(&bytes)
        {
            let (renamed, changed) = hofund_quirks::with_token_field(&body, wanted);
            if changed {
                token_retried = true;
                upstream.state.learn_token_field(wanted);
                body = renamed;
                response = send_json(upstream, url, body.clone(), outbound.clone()).await?;
                continue;
            }
        }
        if !beta_retried
            && matches!(status.as_u16(), 400 | 422)
            && codec.id() == "anthropic-messages"
            && let Some(stripped) =
                hofund_quirks::retry_without_refused_betas(&upstream.state, &whole, &bytes)
        {
            beta_retried = true;
            body = stripped;
            response = send_json(upstream, url, body.clone(), outbound.clone()).await?;
            continue;
        }
        if !thinking_back_retried
            && status.as_u16() == 400
            && hofund_quirks::thinking_back_refusal(&String::from_utf8_lossy(&bytes))
            && let Some(model) = model_from_body(&whole)
            && upstream.state.learn_thinking_back(&model)
        {
            let filled = hofund_quirks::fill_thinking_back(codec.id(), &whole);
            if filled != whole {
                thinking_back_retried = true;
                body = filled;
                response = send_json(upstream, url, body.clone(), outbound.clone()).await?;
                continue;
            }
        }
        if !replacement_retried
            && matches!(status.as_u16(), 400 | 404 | 422)
            && let Some(replacement) = hook.and_then(|hook| {
                hook.retry(route, status.as_u16(), &bytes[..bytes.len().min(1 << 20)])
            })
        {
            replacement_retried = true;
            body = replacement;
            response = send_json(upstream, url, body.clone(), outbound.clone()).await?;
            continue;
        }
        let refused = delta
            && matches!(status.as_u16(), 400 | 404 | 422)
            && hook.is_some_and(|hook| {
                hook.refused(route, status.as_u16(), &bytes[..bytes.len().min(1 << 20)])
            });
        if refused {
            response = send_json(upstream, url, whole.clone(), outbound.clone()).await?;
            if !response.status().is_success() {
                return Err(response_error(response, upstream.allocation_quota_rate).await);
            }
        } else {
            return Err(refusal(
                status.as_u16(),
                retry_after,
                reset_after,
                &bytes,
                upstream.allocation_quota_rate,
            ));
        }
    }
    Ok(Sent {
        response,
        streaming,
        cross_protocol,
        codec,
        allocation_quota_rate: upstream.allocation_quota_rate,
    })
}

async fn response_error(response: reqwest::Response, allocation_quota_rate: bool) -> ProviderError {
    let status = response.status();
    let retry_after = explicit_retry_after(response.headers());
    let reset_after = reset_after(response.headers());
    let bytes = response.bytes().await.unwrap_or_default();
    refusal(
        status.as_u16(),
        retry_after,
        reset_after,
        &bytes,
        allocation_quota_rate,
    )
}

/// The error for an upstream refusal: classified from its words, with the
/// wait its headers or, failing those, its words name.
fn refusal(
    status: u16,
    retry_after: Option<Duration>,
    reset_after: Option<Duration>,
    bytes: &[u8],
    allocation_quota_rate: bool,
) -> ProviderError {
    let error = ProviderError::response_for(status, bytes, allocation_quota_rate);
    let credit = error.is_credit();
    let named_reset =
        hofund_plugin_sdk::transport::reset_after_from_body(bytes, std::time::SystemTime::now())
            .or(reset_after);
    let error = match named_reset.filter(|_| credit) {
        Some(after) => error.with_reset_after(after),
        None => error,
    };
    let error = if credit && hofund_plugin_sdk::transport::credit_soon(bytes) {
        error.with_credit_soon()
    } else {
        error
    };
    let retry = retry_after
        .or_else(|| (!credit).then_some(named_reset).flatten())
        .or_else(|| hofund_plugin_sdk::transport::retry_after_from_body(bytes));
    match retry {
        Some(after) => error.with_retry_after(after),
        None => error,
    }
}

pub(crate) async fn raw_completion(
    response: reqwest::Response,
    streaming: bool,
    codec: &dyn Codec,
    allocation_quota_rate: bool,
) -> Result<Completion, ProviderError> {
    if streaming {
        return raw_stream(response, codec).await;
    }
    let bytes = response.bytes().await.map_err(map_body)?;
    if let Some(error) = hofund_plugin_sdk::transport::carried_error(&bytes, allocation_quota_rate)
    {
        return Err(ProviderError::from_plugin(error));
    }
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
            Some(Err(error)) => return Err(map_body(error)),
            Some(Ok(chunk)) => {
                let events = match decoder.push(&chunk) {
                    Ok(events) => events,
                    Err(_) => {
                        buffered.push(chunk);
                        break;
                    }
                };
                buffered.push(chunk);
                if let Some(error) = events.iter().find_map(crate::stream::failure_of) {
                    return Err(error);
                }
                if events.iter().any(stream_commits) {
                    break;
                }
            }
        }
    }
    let head = futures_util::stream::iter(buffered.into_iter().map(Ok));
    let rest = incoming.map(map_chunk);
    Ok(Completion::Raw(Box::pin(head.chain(rest))))
}

fn stream_commits(event: &hofund_ir::StreamEvent) -> bool {
    match event {
        hofund_ir::StreamEvent::MessageStart(_)
        | hofund_ir::StreamEvent::ContentBlockStart(_)
        | hofund_ir::StreamEvent::ContentBlockStop(_)
        | hofund_ir::StreamEvent::Ping(_) => false,
        hofund_ir::StreamEvent::MessageDelta(delta) => delta.stop.is_some(),
        _ => true,
    }
}

/// Reqwest's read timeout sits 50ms past the kernel's idle deadline, so the
/// kernel is the one that reports a stall. There is no whole-call cap.
pub(crate) fn socket_backstop(idle: Duration) -> Duration {
    idle.saturating_add(Duration::from_millis(50))
}

fn map_chunk(chunk: Result<bytes::Bytes, reqwest::Error>) -> Result<bytes::Bytes, ProviderError> {
    chunk.map_err(map_body)
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

    use super::{build_private_client, note_dropped, send_builder};
    use hofund_plugin_sdk::transport::parse_retry_after;

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
    fn a8_refusal_takes_its_wait_from_headers_then_from_its_words() {
        let body = br#"{"error":{"message":"Rate limit reached. Please try again in 20s."}}"#;
        assert_eq!(
            super::refusal(429, None, None, body, false).retry_after(),
            Some(Duration::from_secs(20))
        );
        assert_eq!(
            super::refusal(429, Some(Duration::from_secs(5)), None, body, false).retry_after(),
            Some(Duration::from_secs(5))
        );
        assert_eq!(
            super::refusal(429, None, None, br#"{"error":"busy"}"#, false).retry_after(),
            None
        );
    }

    #[test]
    fn a4_refusal_reads_an_allocation_quota_as_a_rate_only_when_asked() {
        let body = br#"{"error":{"code":"insufficient_quota","message":"Allocated quota exceeded, please increase your quota limit."}}"#;
        let rate = super::refusal(429, None, None, body, true);
        assert_eq!(rate.kind, crate::ProviderErrorKind::Status);
        assert!(!rate.is_credit());
        let plain = super::refusal(429, None, None, body, false);
        assert!(
            plain.is_credit() || plain.kind != crate::ProviderErrorKind::Status,
            "{plain:?}"
        );
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

    #[tokio::test]
    async fn d71_injected_client_keeps_marker_after_idle_close() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
        use tokio::io::AsyncWriteExt;

        fn build_client() -> reqwest::Client {
            let mut headers = HeaderMap::new();
            headers.insert(
                HeaderName::from_static("x-hofund-marker"),
                HeaderValue::from_static("injected"),
            );
            reqwest::Client::builder()
                .http1_only()
                .default_headers(headers)
                .pool_max_idle_per_host(4)
                .build()
                .expect("client")
        }
        let generation = Arc::new(AtomicUsize::new(0));
        let make_refresh = {
            let generation = Arc::clone(&generation);
            move || {
                generation.fetch_add(1, Ordering::SeqCst);
                Ok(build_client())
            }
        };
        let marked = Arc::new(AtomicUsize::new(0));
        let accepts = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        let marked_server = Arc::clone(&marked);
        let accepts_server = Arc::clone(&accepts);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                accepts_server.fetch_add(1, Ordering::SeqCst);
                let mut request = Vec::new();
                let mut chunk = [0_u8; 2048];
                while let Ok(read) = socket.read(&mut chunk).await {
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&chunk[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let text = String::from_utf8_lossy(&request);
                if text.contains("x-hofund-marker: injected") {
                    marked_server.fetch_add(1, Ordering::SeqCst);
                }
                let body = b"ok";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.write_all(body).await;
            }
        });
        let mut endpoint = super::Endpoint::new("demo", format!("http://{address}"));
        endpoint.client = Some(build_client());
        endpoint.client_refresh = Some(Arc::new(make_refresh));
        let upstream = super::Upstream::open(endpoint, "v1/chat/completions", HeaderMap::new())
            .expect("upstream");
        let url = format!("http://{address}/v1/chat/completions");
        for _ in 0..2 {
            super::post_json(&upstream, &url, br#"{"model":"demo"}"#.to_vec())
                .await
                .expect("request");
        }
        upstream.close_idle_connections();
        super::post_json(&upstream, &url, br#"{"model":"demo"}"#.to_vec())
            .await
            .expect("after close");
        assert_eq!(marked.load(Ordering::SeqCst), 3);
        assert!(generation.load(Ordering::SeqCst) >= 1);
    }

    #[tokio::test]
    async fn d71_keepalive_works_after_idle_close_rebuild() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        use reqwest::header::HeaderMap;

        let accepts = Arc::new(AtomicUsize::new(0));
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        let accepts_server = Arc::clone(&accepts);
        std::thread::spawn(move || {
            for _ in 0..2 {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                accepts_server.fetch_add(1, Ordering::SeqCst);
                for _ in 0..2 {
                    let mut request = Vec::new();
                    let mut chunk = [0_u8; 2048];
                    loop {
                        let Ok(read) = stream.read(&mut chunk) else {
                            return;
                        };
                        if read == 0 {
                            return;
                        }
                        request.extend_from_slice(&chunk[..read]);
                        if request.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let body = b"ok";
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.write_all(body);
                }
            }
        });
        fn build_client() -> reqwest::Client {
            reqwest::Client::builder()
                .http1_only()
                .pool_max_idle_per_host(4)
                .build()
                .expect("client")
        }
        let mut endpoint = super::Endpoint::new("demo", format!("http://{address}"));
        endpoint.client = Some(build_client());
        endpoint.client_refresh = Some(Arc::new(|| Ok(build_client())));
        let upstream = super::Upstream::open(endpoint, "v1/chat/completions", HeaderMap::new())
            .expect("upstream");
        let url = format!("http://{address}/v1/chat/completions");
        for _ in 0..2 {
            let response = super::post_json(&upstream, &url, br#"{"model":"demo"}"#.to_vec())
                .await
                .expect("request");
            let _ = response.bytes().await;
        }
        assert_eq!(accepts.load(Ordering::SeqCst), 1);
        upstream.close_idle_connections();
        for _ in 0..2 {
            let response = super::post_json(&upstream, &url, br#"{"model":"demo"}"#.to_vec())
                .await
                .expect("request");
            let _ = response.bytes().await;
        }
        assert_eq!(accepts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn d71_shared_pool_slot_refresh_reaches_second_upstream() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        use reqwest::header::HeaderMap;
        use std::sync::RwLock;

        let accepts = Arc::new(AtomicUsize::new(0));
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        let accepts_server = Arc::clone(&accepts);
        std::thread::spawn(move || {
            for _ in 0..2 {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                accepts_server.fetch_add(1, Ordering::SeqCst);
                let mut request = Vec::new();
                let mut chunk = [0_u8; 2048];
                loop {
                    let Ok(read) = stream.read(&mut chunk) else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    request.extend_from_slice(&chunk[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let body = b"ok";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.write_all(body);
            }
        });
        let slot = Arc::new(RwLock::new(
            reqwest::Client::builder()
                .http1_only()
                .pool_max_idle_per_host(4)
                .build()
                .expect("client"),
        ));
        let open_on = |id: &str| {
            let mut endpoint = super::Endpoint::new(id, format!("http://{address}"));
            endpoint.client_slot = Some(Arc::clone(&slot));
            endpoint.client = Some(slot.read().expect("read").clone());
            endpoint.client_refresh = Some(Arc::new(|| {
                let fresh = reqwest::Client::builder()
                    .http1_only()
                    .pool_max_idle_per_host(4)
                    .build()
                    .expect("client");
                Ok(fresh)
            }));
            super::Upstream::open(endpoint, "v1/chat/completions", HeaderMap::new())
                .expect("upstream")
        };
        let first = open_on("first");
        let second = open_on("second");
        let url = format!("http://{address}/v1/chat/completions");
        let response = super::post_json(&first, &url, br#"{"model":"demo"}"#.to_vec())
            .await
            .expect("first");
        let _ = response.bytes().await;
        assert_eq!(accepts.load(Ordering::SeqCst), 1);
        first.close_idle_connections();
        let response = super::post_json(&second, &url, br#"{"model":"demo"}"#.to_vec())
            .await
            .expect("second after refresh");
        let _ = response.bytes().await;
        assert_eq!(
            accepts.load(Ordering::SeqCst),
            2,
            "second upstream must dial after shared refresh"
        );
    }

    #[tokio::test]
    async fn private_client_sends_an_http11_request_line() {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let (request_tx, request_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut request = Vec::new();
            let mut chunk = [0_u8; 4096];
            while let Ok(read) = socket.read(&mut chunk).await {
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let _ = request_tx.send(request);
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                .await;
        });
        let client =
            build_private_client(Duration::from_secs(5), Duration::from_secs(5)).expect("client");
        let _ = client
            .get(format!("http://127.0.0.1:{port}/probe"))
            .send()
            .await
            .expect("send");
        let request = request_rx.await.expect("request bytes");
        let first_line = request
            .split(|byte| *byte == b'\n')
            .next()
            .expect("request line");
        assert!(
            first_line.ends_with(b" HTTP/1.1\r"),
            "request line was {:?}",
            String::from_utf8_lossy(first_line)
        );
    }

    #[tokio::test]
    async fn private_client_sends_an_http11_request_line_over_https_when_alpn_offers_h2_first() {
        use std::sync::{Arc, Mutex};

        use rustls::ServerConfig;
        use rustls::pki_types::{CertificateDer, PrivateKeyDer};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio_rustls::TlsAcceptor;

        const CERT_DER: &[u8] = include_bytes!("../testdata/localhost.der");
        const KEY_DER: &[u8] = include_bytes!("../testdata/localhost.key.der");

        let cert = CertificateDer::from(CERT_DER.to_vec());
        let key = PrivateKeyDer::try_from(KEY_DER.to_vec()).expect("private key der");
        let mut server_config = ServerConfig::builder_with_protocol_versions(&[
            &rustls::version::TLS12,
            &rustls::version::TLS13,
        ])
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .expect("server config");
        server_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        let acceptor = TlsAcceptor::from(Arc::new(server_config));
        let captured = Arc::new(Mutex::new(Vec::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let captured_server = Arc::clone(&captured);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let acceptor = acceptor.clone();
                let captured_server = Arc::clone(&captured_server);
                tokio::spawn(async move {
                    let Ok(mut tls) = acceptor.accept(stream).await else {
                        return;
                    };
                    let mut buf = vec![0_u8; 512];
                    let Ok(read) = tls.read(&mut buf).await else {
                        return;
                    };
                    captured_server
                        .lock()
                        .expect("capture")
                        .extend_from_slice(&buf[..read]);
                    let _ = tls
                        .write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                        )
                        .await;
                });
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let client = reqwest::Client::builder()
            .http1_only()
            .danger_accept_invalid_certs(true)
            .connect_timeout(Duration::from_secs(5))
            .read_timeout(Duration::from_secs(5))
            .build()
            .expect("client");
        let _ = client
            .get(format!("https://127.0.0.1:{port}/probe"))
            .send()
            .await
            .expect("send");
        let bytes = captured.lock().expect("capture").clone();
        assert!(
            !bytes.starts_with(b"PRI * HTTP/2.0"),
            "client must not send the HTTP/2 connection preface: {:?}",
            String::from_utf8_lossy(&bytes)
        );
        let first_line = bytes
            .split(|byte| *byte == b'\n')
            .next()
            .expect("request line");
        assert!(
            first_line.ends_with(b" HTTP/1.1\r"),
            "request line was {:?}",
            String::from_utf8_lossy(first_line)
        );
    }
}
