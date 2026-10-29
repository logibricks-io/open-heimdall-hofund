//! Chat Completions, Responses, Messages, and Generative Language calls on the
//! host's HTTP handle.
//!
//! [`execute`] encodes a [`Request`] for the target protocol, posts it with
//! [`Call`], and returns a [`Completion`]. [`count_input`] asks a Messages or
//! Generative Language upstream to count the input tokens of a request. Both
//! mirror the builtin adapters' transport step for step:
//!
//! - Quirks are applied first. When the inbound protocol differs from the
//!   target, unmapped tools are logged once per name, the billing line is
//!   removed from system text, and extension bags are dropped.
//! - An encode failure is status 400: the client sent something the target
//!   protocol cannot carry.
//! - A Messages call carries the protocol version header, which a configured
//!   header of the same name replaces.
//! - A Generative Language call names the model in the path:
//!   `{base}/models/{model}:generateContent`, or
//!   `{base}/models/{model}:streamGenerateContent?alt=sse` for a stream. A
//!   leading `models/` is dropped and the rest is one escaped path segment. An
//!   empty model is status 400 before anything is sent. [`Target::model_root`]
//!   replaces `models` for an upstream such as Vertex AI, whose models sit at
//!   `{base}/publishers/google/models/{model}`.
//! - The call is a JSON `POST`. Only a connect failure is retried, once, so a
//!   body that may have been written is never replayed.
//! - A status outside 2xx keeps its status and `Retry-After`. Its message is the
//!   JSON `error.message` or `message`, or the trimmed body cut to 512
//!   characters.
//! - Across protocols, a stream decodes into events and a unary body into a
//!   response, both without extensions. A stream whose first event is an
//!   upstream failure becomes status 502.
//! - On the same protocol the body is relayed as raw bytes. A stream is read
//!   until its first event so an in-band failure is still an error.

use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::sync::{Mutex, OnceLock};

use bytes::Bytes;
use hofund_codec::{
    AnthropicMessages, Codec, CodecError, GenerativeLanguage, OpenAiChat, OpenAiResponses,
    StreamDecoder, UNMAPPED_TOOLS,
};
use hofund_plugin_api::{
    BodyStream, Completion, ContentPart, CountOutcome, CredentialStyle, HttpHandle, PluginError,
    ProtocolId, Request, Role, SecretString, StreamEvent,
};
use secrecy::ExposeSecret;

use crate::transport::{Call, HttpResponse, is_quota_error, parse_retry_after_headers, read_body};

/// Attempts per call. Only a connect failure is retried.
const ATTEMPTS: u8 = 2;
/// Longest plain-text error message kept from a status body.
const MESSAGE_LIMIT: usize = 512;
/// Prefix of the billing line some clients put in system text.
const BILLING_PREFIX: &str = "x-anthropic-billing-header:";
/// Messages protocol version header and the value sent by default.
const VERSION_HEADER: (&str, &str) = ("anthropic-version", "2023-06-01");

type Prepare = fn(&[u8]) -> Result<Vec<u8>, CodecError>;

/// Where a protocol's calls go under the base URL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Path {
    /// One path for every call.
    Fixed(&'static str),
    /// `models/{model}:{method}`, the Generative Language form.
    Model,
}

/// The Generative Language method a call names.
#[derive(Clone, Copy, Debug)]
enum Method {
    Generate,
    Stream,
    Count,
}

/// Where and how [`execute`] sends a call. [`Debug`] omits header values.
#[derive(Clone)]
pub struct Target {
    protocol: ProtocolId,
    base_url: String,
    credential: CredentialStyle,
    headers: Vec<(String, String)>,
    quirks: Option<&'static hofund_quirks::Profile>,
    model_root: &'static str,
}

impl Target {
    /// A target speaking `protocol` at `base_url`. The credential style is
    /// [`CredentialStyle::Bearer`], with no extra headers and no quirks.
    #[must_use]
    pub fn new(protocol: ProtocolId, base_url: impl Into<String>) -> Self {
        Self {
            protocol,
            base_url: base_url.into(),
            credential: CredentialStyle::Bearer,
            headers: Vec::new(),
            quirks: None,
            model_root: "models",
        }
    }

    /// Where the secret is written.
    #[must_use]
    pub fn credential(mut self, credential: CredentialStyle) -> Self {
        self.credential = credential;
        self
    }

    /// Extra headers. Values are sensitive and replace a header of the same
    /// name, including the credential header.
    #[must_use]
    pub fn headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.headers = headers;
        self
    }

    /// The request rewrites applied before encoding.
    #[must_use]
    pub fn quirks(mut self, quirks: Option<&'static hofund_quirks::Profile>) -> Self {
        self.quirks = quirks;
        self
    }

    /// The path under the base URL that a Generative Language model id
    /// follows: `models` by default, `publishers/google/models` on Vertex AI.
    /// Slashes around it are dropped. Other protocols ignore it.
    #[must_use]
    pub fn model_root(mut self, root: &'static str) -> Self {
        self.model_root = root.trim_matches('/');
        self
    }
}

impl fmt::Debug for Target {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.headers.iter().map(|(name, _)| name.as_str()).collect();
        formatter
            .debug_struct("Target")
            .field("protocol", &self.protocol.as_str())
            .field("base_url", &self.base_url)
            .field("credential", &self.credential)
            .field("header_names", &names)
            .field("quirks", &self.quirks.is_some())
            .field("model_root", &self.model_root)
            .finish()
    }
}

/// Sends `request` to `target` and returns the completion.
///
/// `inbound` is the protocol the client spoke. A missing or empty `secret`
/// sends no credential header.
///
/// # Errors
///
/// Returns [`PluginError`] when the protocol has no SDK transport, the model
/// or request cannot be encoded, the call fails, the upstream answers outside
/// 2xx, or the body cannot be decoded.
pub async fn execute(
    http: &HttpHandle,
    target: &Target,
    secret: Option<&SecretString>,
    request: Request,
    inbound: ProtocolId,
) -> Result<Completion, PluginError> {
    let (codec, prepare, path) = route(target.protocol)?;
    let cross_protocol = codec.id() != inbound.as_str();
    let streaming = request.stream;
    let method = if streaming {
        Method::Stream
    } else {
        Method::Generate
    };
    let url = url_for(
        &target.base_url,
        target.model_root,
        path,
        &request.model,
        method,
    )?;
    let body = prepare_body(target, codec, request, prepare, inbound)?;
    let response = call(target, codec, secret, url)?
        .header("content-type", "application/json")
        .bytes(body)
        .open_raw(http, ATTEMPTS)
        .await?;
    if !response.status().is_success() {
        return Err(status_error(response).await);
    }
    let body = response.into_body();
    if cross_protocol {
        decoded(body, codec, streaming).await
    } else {
        raw(body, codec, streaming).await
    }
}

/// Counts the input tokens of `request` on a Messages or Generative Language
/// target.
///
/// The request is encoded as for [`execute`] without streaming. On Messages,
/// `max_tokens` is removed and the body is posted to
/// `{base}/v1/messages/count_tokens`, which answers `input_tokens`. On
/// Generative Language it is posted to `{base}/models/{model}:countTokens`
/// (under [`Target::model_root`]),
/// which answers `totalTokens`. Status 404, 405, or 501, or a 400 that says the
/// endpoint is not supported, is [`CountOutcome::Unsupported`].
///
/// # Errors
///
/// Returns [`PluginError`] when the target is neither protocol, the model or
/// request cannot be encoded, the call fails, the upstream answers with another
/// status outside 2xx, or a 2xx body has no token count.
pub async fn count_input(
    http: &HttpHandle,
    target: &Target,
    secret: Option<&SecretString>,
    mut request: Request,
    inbound: ProtocolId,
) -> Result<CountOutcome, PluginError> {
    let field = match target.protocol {
        ProtocolId::ANTHROPIC_MESSAGES => "input_tokens",
        ProtocolId::GENERATIVE_LANGUAGE => "totalTokens",
        other => {
            return Err(PluginError::unsupported(format!(
                "protocol `{}` has no SDK count transport",
                other.as_str()
            )));
        }
    };
    let (codec, prepare, path) = route(target.protocol)?;
    request.stream = false;
    let url = match path {
        Path::Fixed(path) => format!(
            "{}/{path}/count_tokens",
            target.base_url.trim_end_matches('/')
        ),
        Path::Model => url_for(
            &target.base_url,
            target.model_root,
            path,
            &request.model,
            Method::Count,
        )?,
    };
    let mut body = prepare_body(target, codec, request, prepare, inbound)?;
    if field == "input_tokens" {
        body = omit_max_tokens(body);
    }
    let mut response = call(target, codec, secret, url)?
        .header("content-type", "application/json")
        .bytes(body)
        .open_raw(http, ATTEMPTS)
        .await?;
    let status = response.status().as_u16();
    let retry_after = parse_retry_after_headers(response.headers(), std::time::SystemTime::now());
    let success = response.status().is_success();
    let mut bytes = Vec::new();
    while let Some(chunk) = response.body_mut().next_chunk().await {
        match chunk {
            Ok(chunk) => bytes.extend_from_slice(&chunk),
            Err(error) if success => return Err(error),
            // The reference reads an error body best effort: a cut body is empty.
            Err(_) => {
                bytes.clear();
                break;
            }
        }
    }
    classify_count(status, retry_after, &bytes, field)
}

/// The codec, body finisher, and path for a protocol this module sends.
fn route(protocol: ProtocolId) -> Result<(&'static dyn Codec, Option<Prepare>, Path), PluginError> {
    match protocol {
        ProtocolId::OPENAI_CHAT => Ok((
            &OpenAiChat,
            Some(OpenAiChat::prepare_upstream_body),
            Path::Fixed("chat/completions"),
        )),
        ProtocolId::OPENAI_RESPONSES => Ok((
            &OpenAiResponses,
            Some(OpenAiResponses::prepare_upstream_body),
            Path::Fixed("responses"),
        )),
        ProtocolId::ANTHROPIC_MESSAGES => {
            Ok((&AnthropicMessages, None, Path::Fixed("v1/messages")))
        }
        ProtocolId::GENERATIVE_LANGUAGE => Ok((
            &GenerativeLanguage,
            Some(GenerativeLanguage::prepare_upstream_body),
            Path::Model,
        )),
        other => Err(PluginError::unsupported(format!(
            "protocol `{}` has no SDK transport",
            other.as_str()
        ))),
    }
}

/// The URL a call posts to.
fn url_for(
    base: &str,
    root: &str,
    path: Path,
    model: &str,
    method: Method,
) -> Result<String, PluginError> {
    let base = base.trim_end_matches('/');
    match path {
        Path::Fixed(path) => Ok(format!("{base}/{path}")),
        Path::Model => {
            let model = model_segment(model)?;
            Ok(match method {
                Method::Generate => format!("{base}/{root}/{model}:generateContent"),
                Method::Stream => format!("{base}/{root}/{model}:streamGenerateContent?alt=sse"),
                Method::Count => format!("{base}/{root}/{model}:countTokens"),
            })
        }
    }
}

/// The model as one escaped path segment, without a leading `models/`.
///
/// # Errors
///
/// Returns a 400 status error when the model is empty.
pub fn model_segment(model: &str) -> Result<String, PluginError> {
    let trimmed = model.trim();
    let model = trimmed.strip_prefix("models/").unwrap_or(trimmed);
    if model.is_empty() {
        return Err(PluginError::status_error(400, "model is empty"));
    }
    Ok(path_segment(model))
}

/// Percent-encoding for one URL path segment. Unreserved characters and
/// `$&+:=@` stay literal, so a tag keeps its colon and a slash is escaped.
#[must_use]
pub fn path_segment(text: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'_' | b'.' | b'~' | b'$' | b'&' | b'+' | b':' | b'=' | b'@'
            )
        {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(char::from(HEX[usize::from(byte >> 4)]));
            out.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    out
}

fn call(
    target: &Target,
    codec: &dyn Codec,
    secret: Option<&SecretString>,
    url: String,
) -> Result<Call, PluginError> {
    let mut call = Call::post(url);
    if codec.id() == AnthropicMessages.id() {
        call = call.header(VERSION_HEADER.0, VERSION_HEADER.1);
    }
    if let Some(secret) = secret.filter(|secret| !secret.expose_secret().is_empty()) {
        call = call.credential(target.credential, secret)?;
    }
    for (name, value) in &target.headers {
        call = call.replace_header(name, value.clone(), true);
    }
    Ok(call)
}

fn prepare_body(
    target: &Target,
    codec: &dyn Codec,
    mut request: Request,
    prepare: Option<Prepare>,
    inbound: ProtocolId,
) -> Result<Vec<u8>, PluginError> {
    if let Some(profile) = target.quirks {
        hofund_quirks::apply(profile, &mut request);
    }
    let cross_protocol = codec.id() != inbound.as_str();
    if cross_protocol {
        warn_unmapped(&request);
        strip_billing_header(&mut request);
        request.strip_extensions();
    }
    let body = codec
        .encode_request(&request)
        .map_err(|error| PluginError::status_error(400, error.to_string()))?;
    match prepare {
        Some(prepare) => prepare(&body).map_err(|error| PluginError::decode(error.to_string())),
        None => Ok(body),
    }
}

/// The count endpoint rejects `max_tokens`, which the encoder fills for
/// create calls.
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
    field: &str,
) -> Result<CountOutcome, PluginError> {
    if status == 404
        || status == 405
        || status == 501
        || (status == 400 && endpoint_unsupported(bytes))
    {
        return Ok(CountOutcome::Unsupported);
    }
    if (200..300).contains(&status) {
        let value: serde_json::Value = serde_json::from_slice(bytes)
            .map_err(|error| PluginError::decode(error.to_string()))?;
        let tokens = value
            .get(field)
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| PluginError::decode(format!("count response has no {field}")))?;
        return Ok(CountOutcome::Counted(tokens));
    }
    Err(status_failure(status, retry_after, bytes))
}

/// Whether a 400 says the count endpoint itself is missing. A rejection of
/// something in the request stays an error.
fn endpoint_unsupported(bytes: &[u8]) -> bool {
    let message = message_from(bytes).to_ascii_lowercase();
    let message = message.trim();
    message == "unsupported"
        || message.contains("unsupported endpoint")
        || message.contains("not supported")
        || message.contains("not implemented")
        || message.contains("does not support")
        || message.contains("do not support")
}

async fn status_error(response: HttpResponse) -> PluginError {
    let (status, retry_after, body) = match read_body(response).await {
        Ok(read) => read,
        Err(error) => return error,
    };
    status_failure(status, retry_after, &body)
}

fn status_failure(
    status: u16,
    retry_after: Option<std::time::Duration>,
    body: &[u8],
) -> PluginError {
    let message = message_from(body);
    let error = if is_quota_error(status, body) {
        PluginError::quota(message)
    } else {
        PluginError::status_error(status, message)
    };
    match retry_after {
        Some(after) => error.with_retry_after(after),
        None => error,
    }
}

async fn collect(mut body: BodyStream) -> Result<Vec<u8>, PluginError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = body.next_chunk().await {
        bytes.extend_from_slice(&chunk?);
    }
    Ok(bytes)
}

async fn decoded(
    body: BodyStream,
    codec: &dyn Codec,
    streaming: bool,
) -> Result<Completion, PluginError> {
    if streaming {
        let events = Events {
            body,
            decoder: codec.stream_decoder(),
            pending: VecDeque::new(),
            finished: false,
            yielded: false,
        };
        return Ok(Completion::Stream(Box::pin(futures_util::stream::unfold(
            events,
            |mut events| async move { events.next().await.map(|item| (item, events)) },
        ))));
    }
    let bytes = collect(body).await?;
    let mut response = codec
        .decode_response(&bytes)
        .map_err(|error| PluginError::decode(error.to_string()))?;
    response.strip_extensions();
    Ok(Completion::Unary(Box::new(response)))
}

async fn raw(
    mut body: BodyStream,
    codec: &dyn Codec,
    streaming: bool,
) -> Result<Completion, PluginError> {
    if !streaming {
        let bytes = collect(body).await?;
        return Ok(Completion::Raw(BodyStream::from_bytes(bytes)));
    }
    let mut decoder = codec.stream_decoder();
    let mut buffered: Vec<Bytes> = Vec::new();
    while let Some(chunk) = body.next_chunk().await {
        let chunk = chunk?;
        let Ok(events) = decoder.push(&chunk) else {
            buffered.push(chunk);
            break;
        };
        buffered.push(chunk);
        let Some(first) = events.first() else {
            continue;
        };
        if let Some(error) = failure_of(first) {
            return Err(error);
        }
        break;
    }
    let mut stream = body;
    for chunk in buffered.into_iter().rev() {
        stream = BodyStream::resume(chunk, stream);
    }
    Ok(Completion::Raw(stream))
}

/// Pull-based decode of a cross-protocol event stream. The next chunk is read
/// only after the decoded events are consumed.
struct Events {
    body: BodyStream,
    decoder: Box<dyn StreamDecoder>,
    pending: VecDeque<Result<StreamEvent, PluginError>>,
    finished: bool,
    yielded: bool,
}

impl Events {
    async fn next(&mut self) -> Option<Result<StreamEvent, PluginError>> {
        loop {
            if let Some(item) = self.take_pending() {
                return Some(item);
            }
            if self.finished {
                return None;
            }
            match self.body.next_chunk().await {
                None => {
                    self.finished = true;
                    match self.decoder.finish() {
                        Ok(events) => self.pending.extend(events.into_iter().map(Ok)),
                        Err(error) => self
                            .pending
                            .push_back(Err(PluginError::decode(error.to_string()))),
                    }
                }
                Some(Err(error)) => {
                    self.finished = true;
                    return Some(Err(error));
                }
                Some(Ok(chunk)) => match self.decoder.push(&chunk) {
                    Ok(events) => self.pending.extend(events.into_iter().map(Ok)),
                    Err(error) => {
                        self.finished = true;
                        return Some(Err(PluginError::decode(error.to_string())));
                    }
                },
            }
        }
    }

    fn take_pending(&mut self) -> Option<Result<StreamEvent, PluginError>> {
        loop {
            let mut item = self.pending.pop_front()?;
            if !self.yielded
                && let Ok(event) = &item
                && let Some(error) = failure_of(event)
            {
                self.pending.clear();
                self.finished = true;
                return Some(Err(error));
            }
            if let Ok(event) = &mut item {
                if matches!(event, StreamEvent::Unknown(_)) {
                    continue;
                }
                event.strip_extensions();
            }
            if item.is_ok() {
                self.yielded = true;
            }
            return Some(item);
        }
    }
}

/// An upstream failure reported as the first event is status 502.
fn failure_of(event: &StreamEvent) -> Option<PluginError> {
    let StreamEvent::Unknown(unknown) = event else {
        return None;
    };
    let embedded = unknown.name == "data" && unknown.payload.get("error").is_some();
    if unknown.name != "error" && unknown.name != "response.failed" && !embedded {
        return None;
    }
    let message = ["error.message", "message", "response.error.message"]
        .into_iter()
        .find_map(|path| pointer(&unknown.payload, path))
        .unwrap_or_else(|| "upstream stream failed".to_owned());
    Some(PluginError::status_error(502, message))
}

fn pointer(value: &serde_json::Value, path: &str) -> Option<String> {
    let mut current = value;
    for key in path.split('.') {
        current = current.get(key)?;
    }
    let text = current.as_str()?.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_owned())
    }
}

fn message_from(bytes: &[u8]) -> String {
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) {
        for path in ["error.message", "message"] {
            if let Some(text) = pointer(&value, path) {
                return text;
            }
        }
    }
    let text = String::from_utf8_lossy(bytes);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        "upstream request failed".to_owned()
    } else {
        trimmed.chars().take(MESSAGE_LIMIT).collect()
    }
}

fn warn_unmapped(request: &Request) {
    let Some(serde_json::Value::Array(items)) = request.extensions.get(UNMAPPED_TOOLS) else {
        return;
    };
    for item in items {
        let Some(name) = item.as_str() else {
            continue;
        };
        if first_drop(name) {
            tracing::warn!(
                tool = name,
                "dropped a tool with no mapping onto the upstream protocol"
            );
        }
    }
}

/// `true` the first time `name` is dropped in this process.
fn first_drop(name: &str) -> bool {
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
    let mut seen = seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    seen.insert(name.to_owned())
}

/// Removes the billing line when crossing protocols.
///
/// A system text part that is only the line is dropped, and the same prefix is
/// cut off the first remaining system text. User text is left alone.
fn strip_billing_header(request: &mut Request) {
    let mut saw_text = false;
    for message in &mut request.messages {
        if !matches!(message.role, Role::System | Role::Developer) {
            continue;
        }
        message.content.retain(|part| match part {
            ContentPart::Text(text) => !billing_line_only(&text.text),
            _ => true,
        });
        if saw_text {
            continue;
        }
        for part in &mut message.content {
            if let ContentPart::Text(text) = part {
                let cut = strip_billing_prefix(&text.text);
                if cut != text.text {
                    text.text = cut;
                }
                saw_text = true;
                break;
            }
        }
    }
    request.messages.retain(|message| {
        !(matches!(message.role, Role::System | Role::Developer)
            && message.content.is_empty()
            && message.extensions.is_empty())
    });
}

fn billing_line_only(text: &str) -> bool {
    text.starts_with(BILLING_PREFIX) && strip_billing_prefix(text).is_empty()
}

fn strip_billing_prefix(text: &str) -> String {
    let Some(rest) = text.strip_prefix(BILLING_PREFIX) else {
        return text.to_owned();
    };
    let rest = consume_fields(rest);
    rest.trim_start_matches(char::is_whitespace).to_owned()
}

/// Consumes `(\s*[A-Za-z_]+=[^;\s]*;)*` and leaves the unmatched tail.
fn consume_fields(mut rest: &str) -> &str {
    loop {
        let trimmed = rest.trim_start();
        let bytes = trimmed.as_bytes();
        if bytes.first().is_none_or(|byte| !is_key_byte(*byte)) {
            return rest;
        }
        let mut index = 1;
        while index < bytes.len() && is_key_byte(bytes[index]) {
            index += 1;
        }
        if index >= bytes.len() || bytes[index] != b'=' {
            return rest;
        }
        index += 1;
        while index < bytes.len() && bytes[index] != b';' && !bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= bytes.len() || bytes[index] != b';' {
            return rest;
        }
        rest = &trimmed[index + 1..];
    }
}

fn is_key_byte(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

#[cfg(test)]
mod tests {
    use hofund_codec::{AnthropicMessages, Codec, GenerativeLanguage};
    use hofund_plugin_api::{CredentialStyle, PluginErrorKind, ProtocolId, Request, StreamEvent};

    use super::{
        Method, Path, Target, classify_count, consume_fields, endpoint_unsupported, failure_of,
        message_from, omit_max_tokens, path_segment, route, strip_billing_header, url_for,
    };
    use hofund_plugin_api::CountOutcome;

    const LINE: &str =
        "x-anthropic-billing-header: cc_version=2.1.197; cc_entrypoint=cli; cch=abc;";

    fn messages_request(system: &str, user: &str) -> Request {
        let body = serde_json::json!({
            "model": "demo",
            "max_tokens": 16,
            "system": system,
            "messages": [{"role": "user", "content": user}],
        });
        AnthropicMessages
            .decode_request(body.to_string().as_bytes())
            .expect("messages request")
    }

    fn texts(request: &Request) -> Vec<String> {
        let encoded = serde_json::to_value(request).expect("request json");
        encoded["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    #[test]
    fn the_billing_prefix_is_cut_from_system_text_and_user_text_is_kept() {
        let mut request = messages_request(
            &format!("{LINE}\nYou are helpful."),
            "see x-anthropic-billing-header: cc_version=1; in the middle",
        );
        strip_billing_header(&mut request);
        let texts = texts(&request);
        assert_eq!(texts.len(), 2, "{texts:?}");
        assert!(texts[0].contains("You are helpful."), "{}", texts[0]);
        assert!(
            !texts[0].contains("x-anthropic-billing-header"),
            "{}",
            texts[0]
        );
        assert!(
            texts[1].contains("x-anthropic-billing-header"),
            "{}",
            texts[1]
        );
    }

    #[test]
    fn a_system_message_that_is_only_the_billing_line_is_dropped() {
        let mut request = messages_request(LINE, "hi");
        strip_billing_header(&mut request);
        let texts = texts(&request);
        assert_eq!(texts.len(), 1, "{texts:?}");
        assert!(texts[0].contains("hi"), "{}", texts[0]);
        assert!(!texts[0].contains("x-anthropic-billing-header"));
    }

    #[test]
    fn system_text_without_the_line_is_unchanged() {
        let mut request = messages_request("You are helpful.", "hi");
        let before = texts(&request);
        strip_billing_header(&mut request);
        assert_eq!(texts(&request), before);
    }

    #[test]
    fn billing_fields_stop_at_the_first_malformed_field() {
        assert_eq!(consume_fields(" a=1; b=2; rest"), " rest");
        assert_eq!(consume_fields(" a=1 b=2;"), " a=1 b=2;");
        assert_eq!(consume_fields(" a1=x;"), " a1=x;");
        assert_eq!(consume_fields(" a=x"), " a=x");
        assert_eq!(consume_fields(""), "");
    }

    #[test]
    fn a_status_message_prefers_json_fields_then_trimmed_text() {
        assert_eq!(
            message_from(br#"{"error":{"message":" slow down "}}"#),
            "slow down"
        );
        assert_eq!(message_from(br#"{"message":"bad key"}"#), "bad key");
        assert_eq!(message_from(b"  plain failure \n"), "plain failure");
        assert_eq!(message_from(b"   "), "upstream request failed");
        let long = "x".repeat(600);
        assert_eq!(message_from(long.as_bytes()).len(), 512);
    }

    #[test]
    fn an_in_band_error_event_is_status_502() {
        let event: StreamEvent = serde_json::from_value(serde_json::json!({
            "type": "unknown",
            "name": "error",
            "payload": {"error": {"message": "overloaded"}}
        }))
        .expect("event");
        let error = failure_of(&event).expect("failure");
        assert_eq!(error.kind(), PluginErrorKind::Status);
        assert_eq!(error.status(), Some(502));
        assert_eq!(error.message(), "overloaded");
    }

    #[test]
    fn target_debug_omits_header_values() {
        let target = Target::new(ProtocolId::OPENAI_CHAT, "http://127.0.0.1:1/v1")
            .credential(CredentialStyle::ApiKey)
            .headers(vec![("x-tenant".to_owned(), "tenant-secret".to_owned())]);
        let shown = format!("{target:?}");
        assert!(shown.contains("x-tenant"), "{shown}");
        assert!(!shown.contains("tenant-secret"), "{shown}");
    }

    #[test]
    fn messages_route_to_v1_messages_without_a_body_finisher() {
        let (codec, prepare, path) = route(ProtocolId::ANTHROPIC_MESSAGES).expect("messages");
        assert_eq!(codec.id(), AnthropicMessages.id());
        assert!(prepare.is_none());
        assert_eq!(path, Path::Fixed("v1/messages"));
        let (_, prepare, path) = route(ProtocolId::OPENAI_CHAT).expect("chat");
        assert!(prepare.is_some());
        assert_eq!(path, Path::Fixed("chat/completions"));
        let (_, prepare, path) = route(ProtocolId::OPENAI_RESPONSES).expect("responses");
        assert!(prepare.is_some());
        assert_eq!(path, Path::Fixed("responses"));
        let (codec, prepare, path) = route(ProtocolId::GENERATIVE_LANGUAGE).expect("generative");
        assert_eq!(codec.id(), GenerativeLanguage.id());
        assert!(prepare.is_some());
        assert_eq!(path, Path::Model);
        let Err(error) = route(ProtocolId::BEDROCK_CONVERSE) else {
            panic!("converse has no SDK transport yet");
        };
        assert_eq!(error.kind(), PluginErrorKind::Unsupported);
    }

    #[test]
    fn generative_urls_name_the_model_and_method() {
        let base = "https://gl.example/v1beta/";
        let cases = [
            (
                Method::Generate,
                "https://gl.example/v1beta/models/gem-1:generateContent",
            ),
            (
                Method::Stream,
                "https://gl.example/v1beta/models/gem-1:streamGenerateContent?alt=sse",
            ),
            (
                Method::Count,
                "https://gl.example/v1beta/models/gem-1:countTokens",
            ),
        ];
        for (method, want) in cases {
            let url = url_for(base, "models", Path::Model, " models/gem-1 ", method).expect("url");
            assert_eq!(url, want, "{method:?}");
        }
        let fixed = url_for(
            base,
            "models",
            Path::Fixed("responses"),
            "",
            Method::Generate,
        )
        .expect("fixed");
        assert_eq!(fixed, "https://gl.example/v1beta/responses");
        let escaped = url_for(
            base,
            "models",
            Path::Model,
            "a/b c:tag$&+=@~é",
            Method::Generate,
        )
        .expect("escaped");
        assert_eq!(
            escaped,
            "https://gl.example/v1beta/models/a%2Fb%20c:tag$&+=@~%C3%A9:generateContent"
        );
        assert_eq!(path_segment("Az09-_.~"), "Az09-_.~");
        assert_eq!(path_segment("?#%"), "%3F%23%25");
        for model in ["", "  ", "models/", " models/ "] {
            let Err(error) = url_for(base, "models", Path::Model, model, Method::Count) else {
                panic!("empty model {model:?}");
            };
            assert_eq!(error.status(), Some(400), "{model:?}");
            assert_eq!(error.message(), "model is empty");
        }
    }

    #[test]
    fn a_model_root_replaces_models_in_generative_urls() {
        let target = Target::new(ProtocolId::GENERATIVE_LANGUAGE, "https://v.example/v1")
            .model_root("/publishers/google/models/");
        assert_eq!(target.model_root, "publishers/google/models");
        let url = url_for(
            &target.base_url,
            target.model_root,
            Path::Model,
            "gem-1",
            Method::Stream,
        )
        .expect("url");
        assert_eq!(
            url,
            "https://v.example/v1/publishers/google/models/gem-1:streamGenerateContent?alt=sse"
        );
        assert_eq!(
            Target::new(ProtocolId::GENERATIVE_LANGUAGE, "x").model_root,
            "models"
        );
    }

    #[test]
    fn max_tokens_is_removed_and_other_bodies_pass_through() {
        let body = br#"{"model":"demo","max_tokens":16,"messages":[]}"#.to_vec();
        let kept: serde_json::Value = serde_json::from_slice(&omit_max_tokens(body)).expect("json");
        assert_eq!(kept, serde_json::json!({"model": "demo", "messages": []}));
        assert_eq!(omit_max_tokens(b"[1,2]".to_vec()), b"[1,2]");
        assert_eq!(omit_max_tokens(b"not json".to_vec()), b"not json");
    }

    #[test]
    fn count_statuses_are_classified() {
        let counted =
            classify_count(200, None, br#"{"input_tokens":12}"#, "input_tokens").expect("counted");
        assert!(matches!(counted, CountOutcome::Counted(12)), "{counted:?}");
        for status in [404, 405, 501] {
            let outcome = classify_count(status, None, b"", "input_tokens").expect("unsupported");
            assert!(matches!(outcome, CountOutcome::Unsupported), "{status}");
        }
        let missing = classify_count(
            400,
            None,
            br#"{"error":{"message":"Not supported"}}"#,
            "input_tokens",
        )
        .expect("unsupported");
        assert!(matches!(missing, CountOutcome::Unsupported));
        let Err(rejected) = classify_count(
            400,
            None,
            br#"{"error":{"message":"bad tool"}}"#,
            "input_tokens",
        ) else {
            panic!("a request rejection stays an error");
        };
        assert_eq!(rejected.status(), Some(400));
        let after = std::time::Duration::from_secs(4);
        let Err(limited) = classify_count(429, Some(after), b"slow", "input_tokens") else {
            panic!("429 is an error");
        };
        assert_eq!(limited.retry_after(), Some(after));
        let Err(quota) = classify_count(429, None, b"monthly quota exhausted", "input_tokens")
        else {
            panic!("quota is an error");
        };
        assert_eq!(quota.kind(), PluginErrorKind::Quota);
        for body in [&br#"{"tokens":1}"#[..], b"ok"] {
            let Err(error) = classify_count(201, None, body, "input_tokens") else {
                panic!("no input_tokens");
            };
            assert_eq!(error.kind(), PluginErrorKind::Decode);
        }
        let total =
            classify_count(200, None, br#"{"totalTokens":7}"#, "totalTokens").expect("total");
        assert!(matches!(total, CountOutcome::Counted(7)), "{total:?}");
        let Err(wrong) = classify_count(200, None, br#"{"input_tokens":7}"#, "totalTokens") else {
            panic!("the field is per protocol");
        };
        assert_eq!(wrong.message(), "count response has no totalTokens");
    }

    #[test]
    fn only_messages_about_the_endpoint_mean_unsupported() {
        for message in [
            "unsupported",
            " Unsupported ",
            "unsupported endpoint: count_tokens",
            "count is not supported here",
            "not implemented",
            "this server does not support counting",
            "we do not support it",
        ] {
            let body = serde_json::json!({"error": {"message": message}}).to_string();
            assert!(endpoint_unsupported(body.as_bytes()), "{message}");
        }
        for message in ["unsupported image type png", "messages: field required", ""] {
            let body = serde_json::json!({"error": {"message": message}}).to_string();
            assert!(!endpoint_unsupported(body.as_bytes()), "{message}");
        }
    }
}
