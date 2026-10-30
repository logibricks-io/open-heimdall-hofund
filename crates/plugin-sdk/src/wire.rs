//! Chat Completions, Responses, Messages, Generative Language, and Bedrock
//! Converse calls on the host's HTTP handle.
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
//! - A Bedrock Converse call names the model in the path:
//!   `{base}/model/{model}/converse`, or `/converse-stream` for a stream, with
//!   `SigV4` percent-encoding. It is signed with the target's [`AwsSigner`]
//!   (service `bedrock`), never an API key. The signed headers are the
//!   configured headers, lowercased, then `content-type`, `host`,
//!   `x-amz-content-sha256`, `x-amz-date`, `accept` for a stream, and
//!   `x-amz-security-token` for temporary credentials; a signed header
//!   replaces a configured one of the same name. An empty model is status 400.
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
//! - [`send`] stops before the body: [`Sent::headers`] reads the reply
//!   headers, [`Sent::watch`] shows the stream's `data:` lines within a byte
//!   limit to a callback while passing the bytes through unchanged, and
//!   [`Sent::fold`] assembles a stream into one response for a client that
//!   asked for one when the upstream only streams.

use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, OnceLock};

use bytes::Bytes;
use hofund_codec::{
    AnthropicMessages, Codec, CodecError, Converse, GenerativeLanguage, OpenAiChat,
    OpenAiResponses, StreamDecoder, UNMAPPED_TOOLS,
};
use hofund_plugin_api::{
    BodyStream, Completion, ContentPart, CountOutcome, CredentialStyle, HttpHandle, PluginError,
    ProtocolId, RawAnswer, RawBody, Request, Role, SecretString, StreamEvent,
};
use secrecy::ExposeSecret;

use crate::sigv4::{self, AwsSigner};
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

/// A rewrite of an encoded JSON request body, run by [`send`] after quirks,
/// encoding, and the protocol's body finisher. It edits the top-level object
/// in place and returns headers to add to the call; those are sensitive and
/// replace a header of the same name, as [`Target::headers`] do.
pub type Rewrite = Arc<
    dyn Fn(&mut serde_json::Map<String, serde_json::Value>) -> Vec<(String, String)> + Send + Sync,
>;

/// A rewrite of each JSON value a 2xx response carries, run by
/// [`Sent::complete`] before the body is decoded or passed on: the whole
/// body of a unary call, and the payload of each `data:` line of a stream.
/// A body or line that is not JSON is passed on as it is.
pub type Unwrap = Arc<dyn Fn(serde_json::Value) -> serde_json::Value + Send + Sync>;

/// A look at the payload of each `data:` line of a 2xx stream, set by
/// [`Sent::watch`]. The bytes pass on unchanged. Returning `true` stops the
/// look; the rest of the stream is not shown to it.
pub type Watch = Arc<dyn Fn(&[u8]) -> bool + Send + Sync>;

/// Where a protocol's calls go under the base URL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Path {
    /// One path for every call.
    Fixed(&'static str),
    /// `models/{model}:{method}`, the Generative Language form.
    Model,
    /// `/model/{model}/converse`, signed with `SigV4`.
    Converse,
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
    signer: Option<AwsSigner>,
    rewrite: Option<Rewrite>,
    method_root: Option<&'static str>,
    unwrap: Option<Unwrap>,
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
            signer: None,
            rewrite: None,
            method_root: None,
            unwrap: None,
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

    /// The AWS credentials a Bedrock Converse call is signed with. Other
    /// protocols ignore them.
    #[must_use]
    pub fn signer(mut self, signer: AwsSigner) -> Self {
        self.signer = Some(signer);
        self
    }

    /// The rewrite [`send`] applies to the encoded body of a Chat
    /// Completions, Responses, Messages, or Generative Language call. A body
    /// that is not a JSON object is sent as encoded, with no added headers.
    /// Bedrock Converse calls and [`count_input`] ignore it.
    #[must_use]
    pub fn rewrite(mut self, rewrite: Rewrite) -> Self {
        self.rewrite = Some(rewrite);
        self
    }

    /// Posts Generative Language calls to `{base}/{root}:{method}`, with no
    /// model in the path, as Code Assist's `v1internal` takes them; the model
    /// then rides in the body, set by a [`Target::rewrite`]. Slashes around
    /// `root` are dropped. [`count_input`] on such a target is
    /// [`CountOutcome::Unsupported`]. Other protocols ignore it.
    #[must_use]
    pub fn method_root(mut self, root: &'static str) -> Self {
        self.method_root = Some(root.trim_matches('/'));
        self
    }

    /// The rewrite [`Sent::complete`] applies to each JSON value of a 2xx
    /// response, such as lifting the reply out of an envelope.
    #[must_use]
    pub fn unwrap(mut self, unwrap: Unwrap) -> Self {
        self.unwrap = Some(unwrap);
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
            .field("signer", &self.signer)
            .field("rewrite", &self.rewrite.is_some())
            .field("method_root", &self.method_root)
            .field("unwrap", &self.unwrap.is_some())
            .finish()
    }
}

/// Sends `request` to `target` and returns the completion.
///
/// `inbound` is the protocol the client spoke. A missing or empty `secret`
/// sends no credential header. This is [`send`] then [`Sent::complete`].
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
    send(http, target, secret, request, inbound)
        .await?
        .complete()
        .await
}

/// A 2xx response from [`send`] whose body is not read yet.
pub struct Sent {
    response: HttpResponse,
    codec: &'static dyn Codec,
    cross_protocol: bool,
    streaming: bool,
    unwrap: Option<Unwrap>,
    watch: Option<(usize, Watch)>,
}

impl fmt::Debug for Sent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Sent")
            .field("status", &self.response.status().as_u16())
            .field("codec", &self.codec.id())
            .field("cross_protocol", &self.cross_protocol)
            .field("streaming", &self.streaming)
            .field("unwrap", &self.unwrap.is_some())
            .field("watch", &self.watch.as_ref().map(|(limit, _)| *limit))
            .finish()
    }
}

impl Sent {
    /// Reads the body into the completion, as [`execute`] does after the status.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the body cannot be read or decoded, or a
    /// stream's first event is an upstream failure.
    pub async fn complete(self) -> Result<Completion, PluginError> {
        let (codec, cross_protocol, streaming) = (self.codec, self.cross_protocol, self.streaming);
        let body = self.into_body();
        if cross_protocol {
            decoded(body, codec, streaming).await
        } else {
            raw(body, codec, streaming).await
        }
    }

    /// The response headers.
    #[must_use]
    pub fn headers(&self) -> &http::HeaderMap {
        self.response.headers()
    }

    /// Shows `watch` the payload of each `data:` line in the first `limit`
    /// bytes of a stream, before [`Unwrap`] runs. A unary body is not shown.
    #[must_use]
    pub fn watch(mut self, limit: usize, watch: Watch) -> Self {
        self.watch = Some((limit, watch));
        self
    }

    /// Reads a stream to its end and folds its events into one response,
    /// whatever protocol the client spoke: for a client that asked for one
    /// reply from an upstream that only streams. An upstream failure
    /// anywhere in the stream is status 502. A unary body is read as
    /// [`Sent::complete`] reads it.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the body cannot be read or decoded,
    /// carries no event, or reports an upstream failure.
    pub async fn fold(self) -> Result<Completion, PluginError> {
        if !self.streaming {
            return self.complete().await;
        }
        let mut decoder = self.codec.stream_decoder();
        let mut body = self.into_body();
        let mut events = Vec::new();
        while let Some(chunk) = body.next_chunk().await {
            keep_events(decoder.push(&chunk?), &mut events)?;
        }
        keep_events(decoder.finish(), &mut events)?;
        if events.is_empty() {
            return Err(PluginError::decode("the upstream stream carried no events"));
        }
        Ok(Completion::Unary(Box::new(hofund_ir::assemble(&events))))
    }

    /// The body with the watch and the unwrap applied, in that order.
    fn into_body(self) -> BodyStream {
        let mut body = self.response.into_body();
        if self.streaming
            && let Some((limit, watch)) = self.watch
        {
            body = watched(body, limit, watch);
        }
        if let Some(unwrap) = self.unwrap {
            body = unwrapped(body, unwrap, self.streaming);
        }
        body
    }
}

/// Adds decoded `events` to `into` without extensions, skipping unknown
/// events; an upstream failure among them is the error.
fn keep_events(
    events: Result<Vec<StreamEvent>, CodecError>,
    into: &mut Vec<StreamEvent>,
) -> Result<(), PluginError> {
    let events = events.map_err(|error| PluginError::decode(error.to_string()))?;
    for mut event in events {
        if let Some(error) = failure_of(&event) {
            return Err(error);
        }
        if matches!(event, StreamEvent::Unknown(_)) {
            continue;
        }
        event.strip_extensions();
        into.push(event);
    }
    Ok(())
}

/// `body` passed on unchanged while [`Watcher`] looks at its first `limit`
/// bytes.
fn watched(body: BodyStream, limit: usize, watch: Watch) -> BodyStream {
    let watcher = Watcher {
        body,
        watch: Some(watch),
        limit,
        seen: 0,
        pending: Vec::new(),
    };
    BodyStream::from_stream(futures_util::stream::unfold(
        watcher,
        |mut watcher| async move {
            let item = watcher.body.next_chunk().await?;
            if let Ok(chunk) = &item {
                watcher.look(chunk);
            }
            Some((item, watcher))
        },
    ))
}

/// The look behind [`Sent::watch`].
struct Watcher {
    body: BodyStream,
    /// `None` once the look is over.
    watch: Option<Watch>,
    limit: usize,
    /// Bytes read so far.
    seen: usize,
    /// Bytes after the last line end read so far.
    pending: Vec<u8>,
}

impl Watcher {
    fn look(&mut self, chunk: &[u8]) {
        let Some(watch) = self.watch.clone() else {
            return;
        };
        let room = self.limit.saturating_sub(self.seen);
        let take = chunk.len().min(room);
        self.seen += take;
        self.pending.extend_from_slice(&chunk[..take]);
        let end = self
            .pending
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |last| last + 1);
        let whole: Vec<u8> = self.pending.drain(..end).collect();
        for line in whole.split(|byte| *byte == b'\n') {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let Some(payload) = line.strip_prefix(b"data:") else {
                continue;
            };
            if watch(payload.trim_ascii()) {
                self.stop();
                return;
            }
        }
        if self.seen >= self.limit {
            self.stop();
        }
    }

    fn stop(&mut self) {
        self.watch = None;
        self.pending = Vec::new();
    }
}

/// Encodes and posts `request` as [`execute`] does and stops at the status.
///
/// A caller that retries elsewhere on a refused call can tell a refusal (an
/// error here) from a failure while reading an accepted body (an error from
/// [`Sent::complete`]).
///
/// # Errors
///
/// Returns [`PluginError`] when the protocol has no SDK transport, the model
/// or request cannot be encoded, the call fails, or the upstream answers
/// outside 2xx.
pub async fn send(
    http: &HttpHandle,
    target: &Target,
    secret: Option<&SecretString>,
    request: Request,
    inbound: ProtocolId,
) -> Result<Sent, PluginError> {
    let (codec, prepare, path) = route(target.protocol)?;
    let cross_protocol = codec.id() != inbound.as_str();
    let streaming = request.stream;
    let method = if streaming {
        Method::Stream
    } else {
        Method::Generate
    };
    let call = if path == Path::Converse {
        if secret.is_some_and(|secret| !secret.expose_secret().is_empty()) {
            return Err(PluginError::config(
                "Converse uses AWS credentials, not an API key",
            ));
        }
        if request.model.is_empty() {
            return Err(PluginError::status_error(400, "model is empty"));
        }
        let model = request.model.clone();
        let body = prepare_body(target, codec, request, prepare, inbound)?;
        converse_call(target, &model, streaming, body)?.1
    } else {
        let url = match target.method_root {
            Some(root) if path == Path::Model => method_url(&target.base_url, root, method),
            _ => url_for(
                &target.base_url,
                target.model_root,
                path,
                &request.model,
                method,
            )?,
        };
        let body = prepare_body(target, codec, request, prepare, inbound)?;
        let (body, added) = rewritten(target.rewrite.as_ref(), body);
        let mut call = call(target, codec, secret, url)?;
        for (name, value) in added {
            call = call.replace_header(&name, value, true);
        }
        call.header("content-type", "application/json").bytes(body)
    };
    let response = call.open_raw(http, ATTEMPTS).await?;
    if !response.status().is_success() {
        return Err(status_error(response).await);
    }
    Ok(Sent {
        response,
        codec,
        cross_protocol,
        streaming,
        unwrap: target.unwrap.clone(),
        watch: None,
    })
}

/// The most of a forwarded answer [`forward`] reads: 256 MiB.
pub const FORWARD_CAP: usize = 256 << 20;

/// Posts `body` to `path` under an OpenAI-style target's base URL and returns
/// a `2xx` answer unchanged.
///
/// The call carries the target's credential and headers and the body's own
/// `Content-Type`. Only Chat Completions and Responses targets have such a
/// base; on any other protocol, a signed target, or one that posts to a
/// method root, the call is `unsupported`. `path` is one
/// [`Forward::new`](hofund_plugin_api::Forward::new) accepted.
///
/// # Errors
///
/// Returns `unsupported` as above, [`PluginError`] when the call fails, the
/// same status error as [`execute`] outside `2xx`, or a decode error when the
/// answer is larger than [`FORWARD_CAP`].
pub async fn forward(
    http: &HttpHandle,
    target: &Target,
    secret: Option<&SecretString>,
    path: &str,
    body: RawBody,
) -> Result<RawAnswer, PluginError> {
    if !forwards(target) {
        return Err(PluginError::unsupported(format!(
            "this upstream does not serve {path}"
        )));
    }
    let url = format!("{}{path}", target.base_url.trim_end_matches('/'));
    let call = call(target, &OpenAiChat, secret, url)?
        .replace_header("content-type", body.content_type(), false)
        .bytes(body.bytes());
    let response = call.open_raw(http, ATTEMPTS).await?;
    if !response.status().is_success() {
        return Err(status_error(response).await);
    }
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let answer = read_capped(response.into_body(), FORWARD_CAP).await?;
    Ok(RawAnswer::new(status, content_type, answer))
}

/// Whether [`forward`] serves `target`: an OpenAI Chat or Responses target
/// with no signer and no method root.
fn forwards(target: &Target) -> bool {
    let openai = matches!(
        target.protocol,
        ProtocolId::OPENAI_CHAT | ProtocolId::OPENAI_RESPONSES
    );
    openai && target.signer.is_none() && target.method_root.is_none()
}

/// All of `body`, or a decode error once it holds more than `cap` bytes.
async fn read_capped(mut body: BodyStream, cap: usize) -> Result<Vec<u8>, PluginError> {
    let mut out = Vec::new();
    while let Some(chunk) = body.next_chunk().await {
        let chunk = chunk?;
        if out.len() + chunk.len() > cap {
            return Err(PluginError::decode(format!(
                "the answer is larger than {cap} bytes"
            )));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
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
        ProtocolId::GENERATIVE_LANGUAGE if target.method_root.is_some() => {
            return Ok(CountOutcome::Unsupported);
        }
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
        Path::Model | Path::Converse => url_for(
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
        ProtocolId::BEDROCK_CONVERSE => Ok((&Converse, None, Path::Converse)),
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
        Path::Converse => Ok(format!(
            "{base}{}",
            sigv4::model_wire_uri(model, matches!(method, Method::Stream))
        )),
    }
}

/// The URL of a Generative Language call on a [`Target::method_root`].
fn method_url(base: &str, root: &str, method: Method) -> String {
    let base = base.trim_end_matches('/');
    match method {
        Method::Generate => format!("{base}/{root}:generateContent"),
        Method::Stream => format!("{base}/{root}:streamGenerateContent?alt=sse"),
        Method::Count => format!("{base}/{root}:countTokens"),
    }
}

/// A signed Bedrock Converse `POST` of `body` for `model`, and its URL.
///
/// The URL is `{base}/model/{model}/converse`, or `/converse-stream` when
/// `stream` is set. The call carries the signed headers described in the
/// [module documentation](self) and the body. Send it with [`Call::open_raw`]
/// or [`Call::open`]; a retry resends the same signature.
///
/// # Errors
///
/// Returns a 400 status error for an empty model, a config error when the
/// target has no [`AwsSigner`] or its URL does not parse, and a decode error
/// when the request cannot be signed.
pub fn converse_call(
    target: &Target,
    model: &str,
    stream: bool,
    body: Vec<u8>,
) -> Result<(String, Call), PluginError> {
    if model.is_empty() {
        return Err(PluginError::status_error(400, "model is empty"));
    }
    let signer = target
        .signer
        .as_ref()
        .ok_or_else(|| PluginError::config("Converse needs AWS credentials"))?;
    let path = sigv4::model_wire_uri(model, stream);
    let canonical_path = sigv4::canonical_uri(&path, sigv4::UriEncoding::Double);
    let url = format!("{}{path}", target.base_url.trim_end_matches('/'));
    let parsed = url::Url::parse(&url)
        .map_err(|error| PluginError::config(format!("provider URL is invalid: {error}")))?;
    let (secs, host) = match signer.fixed_clock() {
        Some((secs, host)) => (*secs, host.clone()),
        None => (unix_now(), host_header(&parsed)),
    };
    let amz_date = sigv4::amz_date(secs);
    let mut ours = vec![
        ("content-type", "application/json".to_owned()),
        ("host", host),
        ("x-amz-content-sha256", sigv4::payload_hash(&body)),
        ("x-amz-date", amz_date.clone()),
    ];
    if stream {
        ours.push(("accept", "application/vnd.amazon.eventstream".to_owned()));
    }
    if let Some(token) = signer.session_token_text() {
        ours.push(("x-amz-security-token", token.to_owned()));
    }
    // Name to (value, sensitive). Configured values are sensitive; ours
    // replace a configured header of the same name.
    let mut pairs = std::collections::BTreeMap::new();
    for (name, value) in &target.headers {
        pairs.insert(name.to_ascii_lowercase(), (value.clone(), true));
    }
    for (name, value) in ours {
        pairs.insert(name.to_owned(), (value, name == "x-amz-security-token"));
    }
    let borrowed: Vec<(&str, &str)> = pairs
        .iter()
        .map(|(name, (value, _))| (name.as_str(), value.as_str()))
        .collect();
    let input = sigv4::SignInput::new()
        .method("POST")
        .canonical_uri(&canonical_path)
        .amz_date(&amz_date)
        .scope(signer.region(), "bedrock")
        .credentials(signer.access_key_id(), signer.secret_access_key())
        .payload(&body)
        .headers(&borrowed);
    let sealed = sigv4::sign(&input)
        .map_err(|error| PluginError::decode(format!("Converse signing: {error}")))?;
    let mut call = Call::post(url.as_str());
    for (name, (value, sensitive)) in pairs {
        if name == "host" {
            continue;
        }
        call = if sensitive {
            call.sensitive_header(name, value)
        } else {
            call.header(name, value)
        };
    }
    let call = call
        .sensitive_header("authorization", sealed.authorization())
        .bytes(body);
    Ok((url, call))
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

/// The `host` value a client sends: the port only when it is not the
/// scheme's default.
fn host_header(url: &url::Url) -> String {
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

/// `body` after `rewrite`, and the headers the rewrite adds. A body that is
/// not a JSON object is left as it is.
fn rewritten(rewrite: Option<&Rewrite>, body: Vec<u8>) -> (Vec<u8>, Vec<(String, String)>) {
    let Some(rewrite) = rewrite else {
        return (body, Vec::new());
    };
    let Ok(serde_json::Value::Object(mut object)) = serde_json::from_slice(&body) else {
        return (body, Vec::new());
    };
    let added = rewrite(&mut object);
    match serde_json::to_vec(&object) {
        Ok(out) => (out, added),
        Err(_) => (body, Vec::new()),
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
    match retry_after.or_else(|| retry_delay(body)) {
        Some(after) => error.with_retry_after(after),
        None => error,
    }
}

/// The wait a Google error body names in its `google.rpc.RetryInfo`
/// detail, such as `"retryDelay": "3.5s"`. The error may come alone or as
/// the first entry of an array.
fn retry_delay(body: &[u8]) -> Option<std::time::Duration> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let error = match &value {
        serde_json::Value::Array(items) => items.first()?.get("error")?,
        other => other.get("error")?,
    };
    let delay = error
        .get("details")?
        .as_array()?
        .iter()
        .filter(|detail| {
            detail.get("@type").and_then(serde_json::Value::as_str)
                == Some("type.googleapis.com/google.rpc.RetryInfo")
        })
        .find_map(|detail| detail.get("retryDelay")?.as_str())?;
    let seconds: f64 = delay.strip_suffix('s')?.parse().ok()?;
    (seconds.is_finite() && seconds >= 0.0).then(|| std::time::Duration::from_secs_f64(seconds))
}

/// `body` with `unwrap` applied to its JSON: the whole body when unary,
/// each `data:` line's payload when streaming.
fn unwrapped(body: BodyStream, unwrap: Unwrap, streaming: bool) -> BodyStream {
    if !streaming {
        return BodyStream::from_stream(futures_util::stream::once(async move {
            let bytes = collect(body).await?;
            Ok(Bytes::from(unwrap_json(&unwrap, bytes)))
        }));
    }
    let lines = Lines {
        body,
        unwrap,
        pending: Vec::new(),
        finished: false,
    };
    BodyStream::from_stream(futures_util::stream::unfold(
        lines,
        |mut lines| async move { lines.next().await.map(|item| (item, lines)) },
    ))
}

/// `bytes` after `unwrap` when they are JSON, else as they are.
fn unwrap_json(unwrap: &Unwrap, bytes: Vec<u8>) -> Vec<u8> {
    let Ok(value) = serde_json::from_slice(&bytes) else {
        return bytes;
    };
    serde_json::to_vec(&unwrap(value)).unwrap_or(bytes)
}

/// The event-stream rewrite behind [`unwrapped`]: whole lines out, the
/// payload of each `data:` line unwrapped.
struct Lines {
    body: BodyStream,
    unwrap: Unwrap,
    /// Bytes after the last line end read so far.
    pending: Vec<u8>,
    finished: bool,
}

impl Lines {
    async fn next(&mut self) -> Option<Result<Bytes, PluginError>> {
        loop {
            if self.finished {
                return None;
            }
            match self.body.next_chunk().await {
                None => {
                    self.finished = true;
                    let rest = std::mem::take(&mut self.pending);
                    if rest.is_empty() {
                        return None;
                    }
                    return Some(Ok(Bytes::from(self.line(&rest))));
                }
                Some(Err(error)) => {
                    self.finished = true;
                    return Some(Err(error));
                }
                Some(Ok(chunk)) => {
                    self.pending.extend_from_slice(&chunk);
                    let Some(end) = self.pending.iter().rposition(|byte| *byte == b'\n') else {
                        continue;
                    };
                    let rest = self.pending.split_off(end + 1);
                    let whole = std::mem::replace(&mut self.pending, rest);
                    let mut out = Vec::with_capacity(whole.len());
                    for line in whole.split_inclusive(|byte| *byte == b'\n') {
                        out.extend(self.line(line));
                    }
                    return Some(Ok(Bytes::from(out)));
                }
            }
        }
    }

    /// One line, with its line end, unwrapped when it is a JSON `data:` line.
    fn line(&self, line: &[u8]) -> Vec<u8> {
        let body_end = line
            .iter()
            .rposition(|byte| !matches!(byte, b'\r' | b'\n'))
            .map_or(0, |last| last + 1);
        let (text, end) = line.split_at(body_end);
        let Some(payload) = text.strip_prefix(b"data:") else {
            return line.to_vec();
        };
        let payload = payload.strip_prefix(b" ").unwrap_or(payload);
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(payload) else {
            return line.to_vec();
        };
        let Ok(json) = serde_json::to_vec(&(self.unwrap)(value)) else {
            return line.to_vec();
        };
        let mut out = Vec::with_capacity(json.len() + 8);
        out.extend_from_slice(b"data: ");
        out.extend(json);
        out.extend_from_slice(end);
        out
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
        AwsSigner, Method, Path, Target, classify_count, consume_fields, converse_call,
        endpoint_unsupported, failure_of, host_header, message_from, omit_max_tokens, path_segment,
        rewritten, route, sigv4, strip_billing_header, url_for,
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
    fn a_rewrite_edits_object_bodies_and_adds_headers() {
        let rewrite: super::Rewrite = std::sync::Arc::new(|body| {
            body.insert("store".to_owned(), false.into());
            body.remove("user");
            vec![("session-id".to_owned(), "s-1".to_owned())]
        });
        let target = Target::new(ProtocolId::OPENAI_RESPONSES, "http://127.0.0.1:1")
            .rewrite(rewrite.clone());
        assert!(format!("{target:?}").contains("rewrite: true"));
        let (body, added) = rewritten(Some(&rewrite), br#"{"model":"m","user":"u"}"#.to_vec());
        assert_eq!(body, br#"{"model":"m","store":false}"#);
        assert_eq!(added, vec![("session-id".to_owned(), "s-1".to_owned())]);
        let (body, added) = rewritten(Some(&rewrite), b"[1]".to_vec());
        assert_eq!(body, b"[1]");
        assert_eq!(added, Vec::<(String, String)>::new());
        let (body, added) = rewritten(None, br#"{"user":"u"}"#.to_vec());
        assert_eq!(body, br#"{"user":"u"}"#);
        assert_eq!(added, Vec::<(String, String)>::new());
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
        let (codec, prepare, path) = route(ProtocolId::BEDROCK_CONVERSE).expect("converse");
        assert_eq!(codec.id(), hofund_codec::Converse.id());
        assert!(prepare.is_none());
        assert_eq!(path, Path::Converse);
    }

    const VECTOR_MODEL: &str = "anthropic.claude-3-5-sonnet-20240620-v1:0";
    const VECTOR_BODY: &[u8] = br#"{"messages":[{"role":"user","content":[{"text":"hi"}]}]}"#;

    fn vector_target() -> Target {
        let signer = AwsSigner::new(
            "us-east-1",
            "AKIDEXAMPLE",
            secrecy::SecretString::from("wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_owned()),
        )
        .session_token(secrecy::SecretString::from(
            "SESSIONTOKENEXAMPLE".to_owned(),
        ))
        .fixed(1_440_938_160, "bedrock-runtime.us-east-1.amazonaws.com");
        Target::new(
            ProtocolId::BEDROCK_CONVERSE,
            "https://bedrock-runtime.us-east-1.amazonaws.com/",
        )
        .signer(signer)
    }

    fn header<'a>(triples: &[(&'a str, &'a str, bool)], name: &str) -> Option<(&'a str, bool)> {
        triples
            .iter()
            .find(|(found, _, _)| *found == name)
            .map(|(_, value, sensitive)| (*value, *sensitive))
    }

    #[test]
    fn converse_signs_with_the_botocore_vector() {
        // botocore 1.43.118 SigV4Auth for service bedrock, the same vector the
        // kernel adapter was pinned to.
        let (url, call) =
            converse_call(&vector_target(), VECTOR_MODEL, false, VECTOR_BODY.to_vec())
                .expect("signed");
        assert_eq!(
            url,
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/anthropic.claude-3-5-sonnet-20240620-v1%3A0/converse"
        );
        assert_eq!(call.uri_and_body(), (url.as_str(), VECTOR_BODY));
        let triples = call.header_triples();
        assert_eq!(
            header(&triples, "authorization"),
            Some((
                "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/bedrock/aws4_request, SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date;x-amz-security-token, Signature=9acaa85e4b0b79f684cb81d4050d66206f71f74d9b7145bfbaa74b72a2c04655",
                true
            ))
        );
        assert_eq!(
            header(&triples, "x-amz-date"),
            Some(("20150830T123600Z", false))
        );
        assert_eq!(
            header(&triples, "x-amz-security-token"),
            Some(("SESSIONTOKENEXAMPLE", true))
        );
        assert_eq!(
            header(&triples, "content-type"),
            Some(("application/json", false))
        );
        let hash = sigv4::payload_hash(VECTOR_BODY);
        assert_eq!(
            header(&triples, "x-amz-content-sha256"),
            Some((hash.as_str(), false))
        );
        assert_eq!(header(&triples, "host"), None);
        assert_eq!(header(&triples, "accept"), None);
        let names: Vec<&str> = triples.iter().map(|(name, _, _)| *name).collect();
        assert_eq!(
            names,
            [
                "content-type",
                "x-amz-content-sha256",
                "x-amz-date",
                "x-amz-security-token",
                "authorization"
            ]
        );
    }

    #[test]
    fn a_converse_stream_accepts_the_event_stream_and_signs_configured_headers() {
        let target = vector_target().headers(vec![
            ("X-Tenant".to_owned(), "blue".to_owned()),
            ("Content-Type".to_owned(), "text/plain".to_owned()),
        ]);
        let (url, call) =
            converse_call(&target, VECTOR_MODEL, true, VECTOR_BODY.to_vec()).expect("signed");
        assert!(
            url.ends_with("/model/anthropic.claude-3-5-sonnet-20240620-v1%3A0/converse-stream")
        );
        let triples = call.header_triples();
        assert_eq!(
            header(&triples, "accept"),
            Some(("application/vnd.amazon.eventstream", false))
        );
        assert_eq!(header(&triples, "x-tenant"), Some(("blue", true)));
        assert_eq!(
            header(&triples, "content-type"),
            Some(("application/json", false))
        );
        let authorization = header(&triples, "authorization").expect("authorization").0;
        assert!(authorization.contains(
            "SignedHeaders=accept;content-type;host;x-amz-content-sha256;x-amz-date;x-amz-security-token;x-tenant,"
        ));
    }

    #[test]
    fn converse_without_a_token_or_a_fixed_clock_signs_now() {
        let signer = AwsSigner::new(
            "eu-west-1",
            "AKIDEXAMPLE",
            secrecy::SecretString::from("secret".to_owned()),
        )
        .session_token(secrecy::SecretString::from(String::new()));
        let target =
            Target::new(ProtocolId::BEDROCK_CONVERSE, "http://127.0.0.1:9/").signer(signer);
        let (url, call) = converse_call(&target, "demo", false, b"{}".to_vec()).expect("signed");
        assert_eq!(url, "http://127.0.0.1:9/model/demo/converse");
        let triples = call.header_triples();
        assert_eq!(header(&triples, "x-amz-security-token"), None);
        let date = header(&triples, "x-amz-date").expect("date").0;
        assert!(date.get(..4).is_some_and(|year| year >= "2026"), "{date}");
        let authorization = header(&triples, "authorization").expect("authorization").0;
        assert!(authorization.contains("/eu-west-1/bedrock/aws4_request"));
    }

    #[test]
    fn converse_needs_a_model_a_signer_and_a_url() {
        let error = converse_call(&vector_target(), "", false, Vec::new()).expect_err("model");
        assert_eq!(error.status(), Some(400));
        assert!(error.to_string().contains("model is empty"));
        let bare = Target::new(ProtocolId::BEDROCK_CONVERSE, "https://up.test");
        let error = converse_call(&bare, "demo", false, Vec::new()).expect_err("signer");
        assert_eq!(error.kind(), PluginErrorKind::Config);
        assert!(error.to_string().contains("Converse needs AWS credentials"));
        let target = Target::new(ProtocolId::BEDROCK_CONVERSE, "not a url").signer(AwsSigner::new(
            "us-east-1",
            "AKID",
            secrecy::SecretString::from("secret".to_owned()),
        ));
        let error = converse_call(&target, "demo", false, Vec::new()).expect_err("url");
        assert_eq!(error.kind(), PluginErrorKind::Config);
        assert!(error.to_string().contains("provider URL is invalid"));
        let target = Target::new(ProtocolId::BEDROCK_CONVERSE, "https://up.test").signer(
            AwsSigner::new("", "AKID", secrecy::SecretString::from("secret".to_owned())),
        );
        let error = converse_call(&target, "demo", false, Vec::new()).expect_err("sign");
        assert_eq!(error.kind(), PluginErrorKind::Decode);
        assert!(error.to_string().contains("Converse signing:"));
    }

    #[test]
    fn the_host_header_names_a_port_only_off_the_default() {
        let parse = |url: &str| url::Url::parse(url).expect("url");
        assert_eq!(host_header(&parse("https://a.test/x")), "a.test");
        assert_eq!(host_header(&parse("https://a.test:443/x")), "a.test");
        assert_eq!(host_header(&parse("https://a.test:8443/x")), "a.test:8443");
        assert_eq!(host_header(&parse("http://a.test:80/x")), "a.test");
        assert_eq!(host_header(&parse("http://a.test:443/x")), "a.test:443");
        assert_eq!(host_header(&parse("http://a.test:8080/x")), "a.test:8080");
    }

    #[test]
    fn a_signer_debug_shows_no_credential() {
        let shown = format!("{:?}", vector_target());
        assert!(shown.contains("us-east-1"), "{shown}");
        assert!(shown.contains("session_token: true"), "{shown}");
        assert!(!shown.contains("AKIDEXAMPLE"), "{shown}");
        assert!(!shown.contains("EXAMPLEKEY"), "{shown}");
        assert!(!shown.contains("SESSIONTOKENEXAMPLE"), "{shown}");
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

    fn lift() -> super::Unwrap {
        std::sync::Arc::new(|value: serde_json::Value| match value {
            serde_json::Value::Object(mut map) if map.contains_key("response") => {
                map.remove("response").unwrap_or_default()
            }
            other => other,
        })
    }

    fn chunked(chunks: &[&str]) -> hofund_plugin_api::BodyStream {
        let items: Vec<Result<bytes::Bytes, hofund_plugin_api::PluginError>> = chunks
            .iter()
            .map(|chunk| Ok(bytes::Bytes::from(chunk.as_bytes().to_vec())))
            .collect();
        hofund_plugin_api::BodyStream::from_stream(futures_util::stream::iter(items))
    }

    fn drained(body: hofund_plugin_api::BodyStream) -> (Vec<String>, String) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        runtime.block_on(async move {
            let mut body = body;
            let mut pieces = Vec::new();
            while let Some(chunk) = body.next_chunk().await {
                pieces.push(String::from_utf8(chunk.expect("chunk").to_vec()).expect("utf8"));
            }
            let all = pieces.concat();
            (pieces, all)
        })
    }

    #[test]
    fn method_root_urls_carry_no_model() {
        let base = "https://ca.example/";
        assert_eq!(
            super::method_url(base, "v1internal", Method::Generate),
            "https://ca.example/v1internal:generateContent"
        );
        assert_eq!(
            super::method_url(base, "v1internal", Method::Stream),
            "https://ca.example/v1internal:streamGenerateContent?alt=sse"
        );
        assert_eq!(
            super::method_url(base, "v1internal", Method::Count),
            "https://ca.example/v1internal:countTokens"
        );
        let target = Target::new(ProtocolId::GENERATIVE_LANGUAGE, base).method_root("/v1internal/");
        assert_eq!(target.method_root, Some("v1internal"));
        let shown = format!("{:?}", target.unwrap(lift()));
        assert!(
            shown.contains("method_root: Some(\"v1internal\")"),
            "{shown}"
        );
        assert!(shown.contains("unwrap: true"), "{shown}");
        let plain = format!("{:?}", Target::new(ProtocolId::GENERATIVE_LANGUAGE, base));
        assert!(plain.contains("unwrap: false"), "{plain}");
    }

    #[test]
    fn a_stream_is_unwrapped_line_by_line_across_chunks() {
        let body = chunked(&[
            "event: x\r\ndata: {\"respo",
            "nse\":{\"a\":1}}\r\n\r\ndata: [DONE]\n",
            ": note\ndata:{\"b\":2}\n",
            "data: {\"response\":{\"c\":3}}",
        ]);
        let (pieces, all) = drained(super::unwrapped(body, lift(), true));
        assert_eq!(
            all,
            "event: x\r\ndata: {\"a\":1}\r\n\r\ndata: [DONE]\n: note\ndata: {\"b\":2}\ndata: {\"c\":3}"
        );
        assert_eq!(pieces[0], "event: x\r\n", "a partial line waits");
        assert_eq!(pieces.len(), 4, "{pieces:?}");
    }

    #[test]
    fn a_stream_without_a_trailing_remainder_ends_cleanly() {
        let (pieces, all) = drained(super::unwrapped(
            chunked(&["data: {\"response\":1}\n", ""]),
            lift(),
            true,
        ));
        assert_eq!(all, "data: 1\n");
        assert_eq!(pieces, vec!["data: 1\n".to_owned()]);
    }

    #[test]
    fn a_unary_body_is_unwrapped_whole_and_non_json_passes() {
        let (_, all) = drained(super::unwrapped(
            chunked(&["{\"response\":", "{\"x\":true}}"]),
            lift(),
            false,
        ));
        assert_eq!(all, "{\"x\":true}");
        let (_, all) = drained(super::unwrapped(chunked(&["not json"]), lift(), false));
        assert_eq!(all, "not json");
    }

    #[test]
    fn google_retry_info_sets_the_retry_after() {
        let detail = |delay: &str| {
            serde_json::json!({"error": {"code": 429, "message": "slow down", "details": [
                {"@type": "type.googleapis.com/google.rpc.ErrorInfo", "retryDelay": "99s"},
                {"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": delay},
            ]}})
        };
        let one = detail("3.5s").to_string();
        assert_eq!(
            super::retry_delay(one.as_bytes()),
            Some(std::time::Duration::from_millis(3500))
        );
        let listed = serde_json::json!([detail("2s")]).to_string();
        assert_eq!(
            super::retry_delay(listed.as_bytes()),
            Some(std::time::Duration::from_secs(2))
        );
        for bad in ["-1s", "2", "xs", "NaNs", "infs"] {
            let body = detail(bad).to_string();
            assert_eq!(super::retry_delay(body.as_bytes()), None, "{bad}");
        }
        assert_eq!(super::retry_delay(b"[]"), None);
        assert_eq!(super::retry_delay(b"{}"), None);
        let error = super::status_failure(429, None, one.as_bytes());
        assert_eq!(
            error.retry_after(),
            Some(std::time::Duration::from_millis(3500))
        );
        let header =
            super::status_failure(429, Some(std::time::Duration::from_secs(7)), one.as_bytes());
        assert_eq!(
            header.retry_after(),
            Some(std::time::Duration::from_secs(7))
        );
    }

    mod sent {
        use std::sync::{Arc, Mutex};

        use bytes::Bytes;
        use hofund_codec::OpenAiResponses;
        use hofund_plugin_api::{BodyStream, Completion, ContentPart, PluginErrorKind};

        use super::super::Sent;

        const STREAM: &str = concat!(
            "event: codex.rate_limits\n",
            "data: {\"type\":\"codex.rate_limits\",\"rate_limits\":{}}\n\n",
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"m\",\"status\":\"in_progress\",\"output\":[]}}\n\n",
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[]}}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"content_index\":0,\"delta\":\"Hel\"}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"content_index\":0,\"delta\":\"lo\"}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"model\":\"m\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":3,\"output_tokens\":2}}}\n\n",
        );

        fn sent(chunks: &[&str], cross_protocol: bool) -> Sent {
            let mut body = BodyStream::from_bytes(Bytes::new());
            for chunk in chunks.iter().rev() {
                body = BodyStream::resume(Bytes::from(chunk.to_string()), body);
            }
            let response = http::Response::builder()
                .header("x-test", "yes")
                .body(body)
                .expect("response");
            Sent {
                response,
                codec: &OpenAiResponses,
                cross_protocol,
                streaming: true,
                unwrap: None,
                watch: None,
            }
        }

        fn split(text: &str, size: usize) -> Vec<&str> {
            let mut out = Vec::new();
            let mut rest = text;
            while !rest.is_empty() {
                let at = rest.len().min(size);
                let (head, tail) = rest.split_at(at);
                out.push(head);
                rest = tail;
            }
            out
        }

        async fn read(completion: Completion) -> Vec<u8> {
            let Completion::Raw(mut body) = completion else {
                panic!("raw expected");
            };
            let mut out = Vec::new();
            while let Some(chunk) = body.next_chunk().await {
                out.extend_from_slice(&chunk.expect("chunk"));
            }
            out
        }

        #[tokio::test]
        async fn fold_assembles_a_same_protocol_stream_into_one_reply() {
            let sent = sent(&split(STREAM, 7), false);
            assert_eq!(
                sent.headers()
                    .get("x-test")
                    .map(http::HeaderValue::as_bytes),
                Some(&b"yes"[..])
            );
            let Completion::Unary(response) = sent.fold().await.expect("fold") else {
                panic!("unary expected");
            };
            let text: String = response
                .message
                .content
                .iter()
                .filter_map(|part| match part {
                    ContentPart::Text(text) => Some(text.text.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(text, "Hello");
            assert_eq!(response.id.as_deref(), Some("resp_1"));
            let usage = response.usage.expect("usage");
            assert_eq!(usage.input_tokens, Some(3));
            assert_eq!(usage.output_tokens, Some(2));
        }

        #[tokio::test]
        async fn fold_fails_on_an_upstream_failure_and_on_an_empty_stream() {
            let late = format!(
                "{}event: error\ndata: {{\"type\":\"error\",\"message\":\"boom\"}}\n\n",
                &STREAM[..STREAM.find("event: response.completed").expect("completed")]
            );
            let error = sent(&[&late], true)
                .fold()
                .await
                .err()
                .expect("late failure");
            assert_eq!(error.status(), Some(502));
            assert!(error.message().contains("boom"), "{}", error.message());

            let failed = concat!(
                "event: response.failed\n",
                "data: {\"type\":\"response.failed\",\"response\":{\"output\":[],\"error\":{\"message\":\"over\"}}}\n\n",
            );
            let error = sent(&[failed], false).fold().await.err().expect("failure");
            assert_eq!(error.status(), Some(502));
            assert!(error.message().contains("over"), "{}", error.message());

            let error = sent(&[": keep-alive\n\n"], true)
                .fold()
                .await
                .err()
                .expect("empty");
            assert_eq!(error.kind(), PluginErrorKind::Decode);
        }

        #[tokio::test]
        async fn watch_sees_data_lines_until_it_stops_and_passes_bytes_unchanged() {
            let seen = Arc::new(Mutex::new(Vec::<String>::new()));
            let into = Arc::clone(&seen);
            let completion = sent(&split(STREAM, 5), false)
                .watch(
                    1 << 16,
                    Arc::new(move |payload: &[u8]| {
                        let text = String::from_utf8_lossy(payload).into_owned();
                        let stop = text.contains("response.created");
                        into.lock().expect("lock").push(text);
                        stop
                    }),
                )
                .complete()
                .await
                .expect("complete");
            assert_eq!(read(completion).await, STREAM.as_bytes());
            let seen = seen.lock().expect("lock").clone();
            assert_eq!(seen.len(), 2, "{seen:?}");
            assert!(
                seen[0].starts_with("{\"type\":\"codex.rate_limits\""),
                "{seen:?}"
            );
        }

        #[tokio::test]
        async fn watch_stops_at_its_limit() {
            let limit = STREAM
                .find("event: response.output_item.added")
                .expect("item");
            // Small pieces, and one piece that runs past the limit.
            for size in [3, STREAM.len()] {
                let seen = Arc::new(Mutex::new(0_usize));
                let into = Arc::clone(&seen);
                let completion = sent(&split(STREAM, size), false)
                    .watch(
                        limit,
                        Arc::new(move |_: &[u8]| {
                            *into.lock().expect("lock") += 1;
                            false
                        }),
                    )
                    .complete()
                    .await
                    .expect("complete");
                assert_eq!(read(completion).await, STREAM.as_bytes());
                assert_eq!(*seen.lock().expect("lock"), 2, "{size}");
            }
        }
    }

    #[test]
    fn only_plain_openai_targets_forward() {
        let base = "https://api.example/v1";
        for protocol in [ProtocolId::OPENAI_CHAT, ProtocolId::OPENAI_RESPONSES] {
            assert!(
                super::forwards(&Target::new(protocol, base)),
                "{protocol:?}"
            );
            assert!(
                !super::forwards(&Target::new(protocol, base).method_root("v1internal")),
                "{protocol:?} with a method root"
            );
            let signer = AwsSigner::new(
                "us-east-1",
                "AKIDEXAMPLE",
                secrecy::SecretString::from("secret".to_owned()),
            );
            assert!(
                !super::forwards(&Target::new(protocol, base).signer(signer)),
                "{protocol:?} with a signer"
            );
        }
        for protocol in [
            ProtocolId::ANTHROPIC_MESSAGES,
            ProtocolId::GENERATIVE_LANGUAGE,
            ProtocolId::BEDROCK_CONVERSE,
        ] {
            assert!(
                !super::forwards(&Target::new(protocol, base)),
                "{protocol:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_forwarded_answer_is_read_whole_up_to_its_cap() {
        let pieces = || {
            hofund_plugin_api::BodyStream::from_stream(futures_util::stream::iter(
                ["ab", "cd", "e"].map(|piece| Ok(bytes::Bytes::from_static(piece.as_bytes()))),
            ))
        };
        assert_eq!(
            super::read_capped(pieces(), 5).await.expect("whole"),
            b"abcde"
        );
        let error = super::read_capped(pieces(), 4).await.expect_err("capped");
        assert_eq!(error.kind(), PluginErrorKind::Decode);
        assert_eq!(error.message(), "the answer is larger than 4 bytes");
        let failing = hofund_plugin_api::BodyStream::from_stream(futures_util::stream::iter([
            Ok(bytes::Bytes::from_static(b"a")),
            Err(hofund_plugin_api::PluginError::interrupted("reset")),
        ]));
        let error = super::read_capped(failing, 8).await.expect_err("reset");
        assert_eq!(error.message(), "reset");
    }
}
