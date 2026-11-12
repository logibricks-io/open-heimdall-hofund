//! Codex's ChatGPT backend on the gateway's own address.
//!
//! A Codex signed in to ChatGPT can point its built-in provider at
//! `<gateway>/backend-api/codex` and keep that sign-in. Its own models go on
//! to the ChatGPT backend, their input put right for it first; hofund's
//! models, the ids with a `/`, are served here, in process, as
//! `/v1/responses`, and compacted here too. So is a turn for one of Codex's
//! own models when the gateway lists it as `codex/<model>`: that turn runs on
//! hofund's Codex accounts. The gateway token never
//! reaches this code: the gateway takes it out first, so what is relayed is
//! the caller's own sign-in.

use std::collections::{BTreeMap, BTreeSet};
use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hofund_plugin_api::{Body, BodyStream, PluginError, StoppedWaiting, too_long, words_of};
use hofund_plugin_sdk::json::{self, Value, json};
use hofund_plugin_sdk::{
    HeaderMap, HttpRequest, HttpResponse, refused_passthrough, text_response, without_passthrough,
};

use crate::account_refusal;
use crate::input::{self, sealed};
use crate::{
    Kernel, capped_models, client_version, entry, error_response, ids, json_response,
    listed_models, official,
};

/// The route prefix Codex's base URL ends with.
pub(crate) const PREFIX: &str = "/backend-api/codex";

/// How long the backend's model list is waited for before hofund's models
/// are answered alone. Codex gives up on the list at five seconds.
pub(crate) const LIST_WAIT: Duration = Duration::from_secs(3);

/// How much of the backend's model list is read.
const OWN_LIST_CAP: usize = 16 * 1024 * 1024;

/// How much of a compaction's summary reply is read.
const SUMMARY_CAP: usize = 16 * 1024 * 1024;

/// The message a hofund model's compaction gets.
pub(crate) const COMPACT_MESSAGE: &str =
    "/responses/compact is not supported for hofund models; use a compaction_trigger on /responses";

/// The header that names one of hofund's Codex accounts.
pub(crate) const ACCOUNT_HEADER: &str = "x-hofund-account";

/// Where hofund's Codex accounts sit in the gateway's model list: a
/// route named `codex/<model>` serves Codex's own `<model>`.
pub(crate) const ACCOUNTS: &str = "codex/";

/// What a relayed turn that names an account gets.
pub(crate) const ACCOUNT_MESSAGE: &str = "names one of hofund's Codex accounts, and hofund has no codex/<model> route for this model, so the turn would go to the ChatGPT sign-in Codex holds";

/// Where relayed calls go.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Bases {
    /// The ChatGPT backend, for a ChatGPT sign-in.
    pub(crate) chatgpt: &'static str,
    /// The API, for a caller whose bearer is an API key (`sk-`).
    pub(crate) api: &'static str,
}

/// The real upstreams.
pub(crate) const BASES: Bases = Bases {
    chatgpt: "https://chatgpt.com/backend-api/codex",
    api: "https://api.openai.com/v1",
};

/// Request headers that are about one hop, or that the client sets itself.
const DROPPED: [&str; 10] = [
    "connection",
    "keep-alive",
    "proxy-connection",
    "transfer-encoding",
    "upgrade",
    "te",
    "trailer",
    "content-length",
    "host",
    "accept-encoding",
];

/// One whole compaction's summary budget. Codex hears nothing while the
/// summary is made and abandons a silent stream at 300 seconds, so stop a
/// minute earlier and answer with the local compaction while it still waits.
pub(crate) const COMPACT_WITHIN: Duration = Duration::from_secs(240);

const SUMMARY_TIMEOUT: &str = "no summary came in 4m0s, and Codex stops waiting at 5 minutes";

type CompactDeadline = Pin<Box<dyn Future<Output = ()> + Send>>;
type CompactClock = Arc<dyn Fn(Duration) -> CompactDeadline + Send + Sync>;

/// Refused passthrough members and the compaction clock.
#[derive(Clone)]
pub(crate) struct State {
    refused: Arc<Mutex<BTreeMap<String, BTreeSet<String>>>>,
    compact_clock: CompactClock,
}

impl Default for State {
    fn default() -> Self {
        Self {
            refused: Arc::new(Mutex::new(BTreeMap::new())),
            compact_clock: Arc::new(|within| Box::pin(hofund_plugin_sdk::sleep(within))),
        }
    }
}

impl std::fmt::Debug for State {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("State").finish_non_exhaustive()
    }
}

impl State {
    #[cfg(test)]
    pub(crate) fn with_compact_clock(
        clock: impl Fn(Duration) -> CompactDeadline + Send + Sync + 'static,
    ) -> Self {
        Self {
            compact_clock: Arc::new(clock),
            ..Self::default()
        }
    }

    fn strip(&self, base: &str, body: &[u8]) -> Option<Vec<u8>> {
        let refused = self.refused.lock().ok()?;
        let fields = refused.get(base)?;
        let mut out = body.to_vec();
        let mut changed = false;
        for field in fields {
            let (next, removed) = without_passthrough(&out, field);
            out = next;
            changed |= removed;
        }
        changed.then_some(out)
    }

    fn learn(&self, base: &str, field: &str) -> bool {
        self.refused
            .lock()
            .map(|mut refused| {
                refused
                    .entry(base.to_owned())
                    .or_default()
                    .insert(field.to_owned())
            })
            .unwrap_or(false)
    }
}

/// Answers `request` under [`PREFIX`] without retaining refusals.
#[cfg(test)]
pub(crate) async fn serve(kernel: &dyn Kernel, bases: Bases, request: HttpRequest) -> HttpResponse {
    let state = State::with_compact_clock(|_| Box::pin(std::future::pending()));
    serve_with_state(kernel, bases, request, &state).await
}

/// Answers `request` under [`PREFIX`], retaining refused passthrough members.
pub(crate) async fn serve_with_state(
    kernel: &dyn Kernel,
    bases: Bases,
    mut request: HttpRequest,
    state: &State,
) -> HttpResponse {
    if wants_websocket(request.headers()) {
        return text_response(426, "hofund serves Codex over HTTP; use HTTP");
    }
    let rest = request
        .uri()
        .path()
        .strip_prefix(PREFIX)
        .unwrap_or_default()
        .to_owned();
    let get = request.method().as_str() == "GET";
    if get && rest == "/models" {
        return models(kernel, bases, &request).await;
    }
    let compact = rest == "/responses/compact";
    if !get && (compact || rest == "/responses") {
        if !compact
            && !api_key(request.headers())
            && let Some(body) = account_model(kernel, request.body()).await
        {
            *request.body_mut() = Body::from(body);
        }
        if is_hofund_model(request.body()) {
            if compact {
                return error(400, COMPACT_MESSAGE);
            }
            let (body, compaction) = input::for_hofund(request.body().bytes());
            if let Some(body) = body {
                *request.body_mut() = Body::from(body);
            }
            if compaction {
                return summarise(kernel, &request, state).await;
            }
            return in_process(kernel, &request).await;
        }
        if request.headers().contains_key(ACCOUNT_HEADER) {
            return error(400, &format!("{ACCOUNT_HEADER} {ACCOUNT_MESSAGE}"));
        }
        let tool_back = input::tool_names_back(request.body().bytes());
        if let Some(body) = input::for_backend(request.body().bytes()) {
            *request.body_mut() = Body::from(body);
        }
        return relay(kernel, bases, &rest, request, state, tool_back).await;
    }
    relay(kernel, bases, &rest, request, state, BTreeMap::new()).await
}

/// Whether the request asks to become a websocket.
fn wants_websocket(headers: &HeaderMap) -> bool {
    headers
        .get_all("upgrade")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .any(|value| value.to_ascii_lowercase().contains("websocket"))
}

/// Whether the body names one of hofund's models: an id with a `/`.
fn is_hofund_model(body: &Body) -> bool {
    json::from_slice::<Value>(body.bytes())
        .ok()
        .as_ref()
        .and_then(|value| value.get("model"))
        .and_then(Value::as_str)
        .is_some_and(|model| model.contains('/'))
}

/// The body as `codex/<model>` when it names Codex's own `<model>` and the
/// gateway lists `codex/<model>`: that turn is served by hofund's Codex
/// accounts. A list that cannot be had leaves the turn to the relay.
async fn account_model(kernel: &dyn Kernel, body: &Body) -> Option<Vec<u8>> {
    let mut value: Value = json::from_slice(body.bytes()).ok()?;
    let model = value.get("model").and_then(Value::as_str)?;
    if model.is_empty() || model.contains('/') {
        return None;
    }
    let routed = format!("{ACCOUNTS}{model}");
    if !ids(kernel).await.ok()?.contains(&routed) {
        return None;
    }
    *value.get_mut("model")? = Value::String(routed);
    Some(value.to_string().into_bytes())
}

/// An OpenAI-shaped error with `status`.
fn error(status: u16, message: &str) -> HttpResponse {
    let body = json!({"error": {"message": message, "type": "invalid_request_error"}});
    json_response(status, body.to_string())
}

/// The request as the gateway's `/v1/responses`.
fn local_call(request: &HttpRequest) -> HttpRequest {
    let mut call = HttpRequest::new(request.body().clone());
    *call.method_mut() = request.method().clone();
    if let Ok(uri) = "/v1/responses".parse() {
        *call.uri_mut() = uri;
    }
    for name in ["content-type", "accept"] {
        if let Some(value) = request.headers().get(name) {
            call.headers_mut().insert(name, value.clone());
        }
    }
    call
}

/// The request as the gateway's `/v1/responses`, served in process.
async fn in_process(kernel: &dyn Kernel, request: &HttpRequest) -> HttpResponse {
    match kernel.call(local_call(request)).await {
        Ok(response) => response,
        Err(failure) => error_response(&failure),
    }
}

/// A hofund model's compaction: the conversation is summarised in
/// process, and the summary is answered as the one compaction item of a
/// Responses stream, which Codex keeps in place of what it summarised.
async fn summarise(kernel: &dyn Kernel, request: &HttpRequest, state: &State) -> HttpResponse {
    let marker = StoppedWaiting::new();
    let mut deadline = (state.compact_clock)(COMPACT_WITHIN);
    match summary_attempt_within(kernel, request, &marker, &mut deadline).await {
        SummaryAttempt::Done(response) => response,
        SummaryAttempt::StoppedWaiting => local_compaction(request, SUMMARY_TIMEOUT),
        SummaryAttempt::SizeRefused => {
            let expired =
                poll_fn(|context| Poll::Ready(deadline.as_mut().poll(context).is_ready())).await;
            if expired {
                marker.stop();
                return local_compaction(request, SUMMARY_TIMEOUT);
            }
            let Some(body) = input::text_summary_body(request.body().bytes()) else {
                return local_compaction(
                    request,
                    "the conversation could not be trimmed for a text retry",
                );
            };
            let mut retry = local_call(request);
            *retry.body_mut() = Body::from(body);
            set(retry.headers_mut(), "accept", "application/json");
            match summary_attempt_within(kernel, &retry, &marker, &mut deadline).await {
                SummaryAttempt::Done(response) => response,
                SummaryAttempt::StoppedWaiting => local_compaction(request, SUMMARY_TIMEOUT),
                SummaryAttempt::SizeRefused => {
                    local_compaction(request, "compaction: the text summary retry failed")
                }
                SummaryAttempt::PassThrough(status, said) => local_compaction(
                    request,
                    &format!(
                        "compaction: hofund answered the text summary request {status}: {said}"
                    ),
                ),
                SummaryAttempt::Fail { status, said } => {
                    let why = if let Some(status) = status {
                        format!(
                            "compaction: hofund answered the text summary request {status}: {said}"
                        )
                    } else {
                        format!("compaction: {said}")
                    };
                    local_compaction(request, &why)
                }
            }
        }
        SummaryAttempt::PassThrough(status, said) => error(
            status,
            &format!("compaction: hofund answered the summary request {status}: {said}"),
        ),
        SummaryAttempt::Fail { status, said } => {
            let status = status.unwrap_or(502);
            error(status, &said)
        }
    }
}

enum SummaryAttempt {
    Done(HttpResponse),
    SizeRefused,
    PassThrough(u16, String),
    Fail { status: Option<u16>, said: String },
    StoppedWaiting,
}

async fn summary_attempt_within(
    kernel: &dyn Kernel,
    request: &HttpRequest,
    marker: &StoppedWaiting,
    deadline: &mut CompactDeadline,
) -> SummaryAttempt {
    let mut attempt = std::pin::pin!(summary_attempt(kernel, request, marker));
    poll_fn(|context| {
        if let Poll::Ready(attempt) = attempt.as_mut().poll(context) {
            return Poll::Ready(attempt);
        }
        if deadline.as_mut().poll(context).is_ready() {
            marker.stop();
            return Poll::Ready(SummaryAttempt::StoppedWaiting);
        }
        Poll::Pending
    })
    .await
}

async fn summary_attempt(
    kernel: &dyn Kernel,
    request: &HttpRequest,
    marker: &StoppedWaiting,
) -> SummaryAttempt {
    let mut call = local_call(request);
    set(call.headers_mut(), "accept", "application/json");
    call.extensions_mut().insert(marker.clone());
    let response = match kernel.call(call).await {
        Ok(response) => response,
        Err(failure) => {
            return SummaryAttempt::Fail {
                status: failure.status().filter(|status| *status >= 400),
                said: failure.message().to_owned(),
            };
        }
    };
    let status = response.status().as_u16();
    let Some(body) = read_up_to(response.into_body(), SUMMARY_CAP).await else {
        return SummaryAttempt::Fail {
            status: Some(502),
            said: "compaction: the summary reply could not be read".to_owned(),
        };
    };
    if status >= 400 {
        let said = said(&body);
        if status == 502 && said.ends_with(ENDED_SHORT) {
            return SummaryAttempt::Done(local_compaction(request, &said));
        }
        if summary_refused_for_size(status, &body) {
            return SummaryAttempt::SizeRefused;
        }
        return SummaryAttempt::PassThrough(status, said);
    }
    let reply = match Reply::of(&body) {
        Ok(reply) => reply,
        Err(why) if why.ends_with(ENDED_SHORT) => {
            return SummaryAttempt::Done(local_compaction(request, &why));
        }
        Err(why) => {
            return SummaryAttempt::Fail {
                status: Some(502),
                said: format!("compaction: {why}, so there is no summary"),
            };
        }
    };
    if reply.status == "incomplete" {
        let why = reply
            .incomplete
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        return SummaryAttempt::Fail {
            status: Some(502),
            said: format!("compaction: the summary was cut short ({why}), so there is no summary"),
        };
    }
    let summary = reply.summary();
    if summary.trim().is_empty() {
        let with = answered_with(&reply.output);
        return SummaryAttempt::Fail {
            status: Some(502),
            said: format!("compaction: the model wrote no summary{with}"),
        };
    }
    SummaryAttempt::Done(compaction_stream(&reply.id(), &summary, &reply.usage))
}

/// Whether a refused summary request should be retried as plain text.
fn summary_refused_for_size(status: u16, body: &[u8]) -> bool {
    if status == 413 {
        return true;
    }
    // `too_long` already refuses anything outside 4xx and a 429
    too_long(status, &words_of(&said(body)))
}

/// What the gateway says ends a stream that broke off after it began.
pub(crate) const ENDED_SHORT: &str = "the reply ended before it was complete";
pub(crate) const LOCAL_SUMMARY_MAX: usize = 80_000;

fn local_compaction(request: &HttpRequest, why: &str) -> HttpResponse {
    let summary = local_summary(request.body().bytes(), why);
    compaction_stream(&Reply::default().id(), &summary, &Value::Null)
}

pub(crate) fn local_summary(body: &[u8], why: &str) -> String {
    let value = json::from_slice::<Value>(body).unwrap_or(Value::Null);
    let mut asked = Vec::new();
    let mut last = String::new();
    for item in value
        .get("input")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if item
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("message")
            != "message"
        {
            continue;
        }
        let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
        let content = item.get("content").unwrap_or(&Value::Null);
        let text = if let Some(text) = content.as_str() {
            text.to_owned()
        } else {
            content
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let text = text.trim();
        if text.is_empty() || text == super::input::COMPACT_PROMPT {
            continue;
        }
        match role {
            "user" => asked.push(text.to_owned()),
            "assistant" => last = text.to_owned(),
            _ => {}
        }
    }
    let mut first = asked.len();
    let mut kept = 0;
    while first > 0 && kept + asked[first - 1].len() <= LOCAL_SUMMARY_MAX {
        first -= 1;
        kept += asked[first].len();
    }
    let mut summary = format!(
        "hofund compacted this conversation without a model ({why}), so this is no summary: it is what the user asked"
    );
    if first > 0 {
        summary.push_str(&format!(
            " (the latest {} of {} messages)",
            asked.len() - first,
            asked.len()
        ));
    }
    summary.push_str(
        " and the last reply. Look at the workspace for where the work stands before going on.\n\n## The user's messages\n",
    );
    for text in &asked[first..] {
        summary.push_str(&format!("\n{text}\n"));
    }
    if !last.is_empty() {
        summary.push_str(&format!("\n## The last reply\n\n{last}\n"));
    }
    summary
}

/// The compaction stream: created, the item added and done, completed.
pub(crate) fn compaction_stream(id: &str, summary: &str, usage: &Value) -> HttpResponse {
    let item = json!({
        "type": "compaction",
        "id": format!("cmp_{}", id.strip_prefix("resp_").unwrap_or(id)),
        "encrypted_content": sealed(summary),
    });
    let mut done = json!({"id": id, "object": "response", "status": "completed", "output": [item]});
    if !usage.is_null() {
        done["usage"] = usage.clone();
    }
    let events = [
        json!({"type": "response.created", "response": {"id": id, "object": "response", "status": "in_progress", "output": []}}),
        json!({"type": "response.output_item.added", "output_index": 0, "item": item}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": item}),
        json!({"type": "response.completed", "response": done}),
    ];
    let mut text = String::new();
    for event in events {
        let name = event["type"].as_str().unwrap_or_default();
        text.push_str(&format!("event: {name}\ndata: {event}\n\n"));
    }
    let mut response = text_response(200, text);
    set(response.headers_mut(), "content-type", "text/event-stream");
    set(response.headers_mut(), "cache-control", "no-cache");
    response
}

/// Sets `name` to `value` in `headers`.
fn set(headers: &mut HeaderMap, name: &'static str, value: &'static str) {
    if let Ok(value) = value.parse() {
        headers.insert(name, value);
    }
}

/// What a refused summary request said: `error.message`, or the text.
fn said(body: &[u8]) -> String {
    let parsed = json::from_slice::<Value>(body).ok();
    if let Some(message) = parsed
        .as_ref()
        .and_then(|value| value.pointer("/error/message"))
        .and_then(Value::as_str)
        .filter(|message| !message.trim().is_empty())
    {
        return message.to_owned();
    }
    let text = String::from_utf8_lossy(body).trim().to_owned();
    if text.is_empty() {
        "nothing more".to_owned()
    } else {
        text
    }
}

/// What a summary reply without a summary held instead, as
/// ` (it answered with function_call shell, reasoning)`.
fn answered_with(output: &[Value]) -> String {
    let mut kinds: Vec<String> = Vec::new();
    for item in output {
        let mut kind = item
            .get("type")
            .and_then(Value::as_str)
            .filter(|kind| !kind.is_empty())
            .unwrap_or("an untyped item")
            .to_owned();
        if let Some(name) = item
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
        {
            kind = format!("{kind} {name}");
        }
        if !kinds.contains(&kind) {
            kinds.push(kind);
        }
    }
    if kinds.is_empty() {
        " (it answered with nothing)".to_owned()
    } else {
        format!(" (it answered with {})", kinds.join(", "))
    }
}

/// A summary reply: a response as a whole, or a Responses stream.
#[derive(Debug, Default)]
struct Reply {
    id: String,
    output: Vec<Value>,
    usage: Value,
    status: String,
    incomplete: Value,
}

impl Reply {
    /// The reply in `body`, or why it is none: a failed stream, or not a
    /// response at all.
    fn of(body: &[u8]) -> Result<Self, String> {
        let text = String::from_utf8_lossy(body);
        if text.trim_start().starts_with('{') {
            let value: Value =
                json::from_str(&text).map_err(|_| "the summary reply is not JSON".to_owned())?;
            return Ok(Self::whole(&value));
        }
        let mut reply = Self::default();
        let mut items = Vec::new();
        for line in text.lines() {
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let Ok(event) = json::from_str::<Value>(data.trim_start()) else {
                continue;
            };
            let response = event.get("response");
            if let Some(id) = response
                .and_then(|response| response.get("id"))
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
            {
                id.clone_into(&mut reply.id);
            }
            let kind = event
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            match kind {
                "response.output_item.done" => {
                    items.push(event.get("item").cloned().unwrap_or(Value::Null));
                }
                "response.completed" | "response.incomplete" => {
                    let id = std::mem::take(&mut reply.id);
                    reply = Self::whole(response.unwrap_or(&Value::Null));
                    reply.id = id;
                    if reply.status.is_empty() {
                        kind.trim_start_matches("response.")
                            .clone_into(&mut reply.status);
                    }
                }
                "response.failed" | "error" => return Err(failed(&event)),
                _ => {}
            }
        }
        if !items.is_empty() {
            reply.output = items;
        }
        if reply.status.is_empty() {
            return Err(ENDED_SHORT.to_owned());
        }
        Ok(reply)
    }

    /// The reply of a whole response.
    fn whole(value: &Value) -> Self {
        let field = |key: &str| value.get(key).cloned().unwrap_or(Value::Null);
        let text = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        Self {
            id: text("id"),
            output: value
                .get("output")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            usage: field("usage"),
            status: text("status"),
            incomplete: field("incomplete_details"),
        }
    }

    /// The reply's id, or one made up when it has none.
    fn id(&self) -> String {
        if self.id.is_empty() {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |since| since.as_nanos());
            format!("resp_hofund_{nanos}")
        } else {
            self.id.clone()
        }
    }

    /// The text of the reply's messages.
    fn summary(&self) -> String {
        let mut out = String::new();
        for item in &self.output {
            if item.get("type").and_then(Value::as_str) != Some("message") {
                continue;
            }
            for part in item
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                out.push_str(part.get("text").and_then(Value::as_str).unwrap_or_default());
            }
        }
        out
    }
}

/// What a failed stream said it failed of.
fn failed(event: &Value) -> String {
    ["/response/error/message", "/message", "/error/message"]
        .iter()
        .filter_map(|pointer| event.pointer(pointer).and_then(Value::as_str))
        .find(|message| !message.is_empty())
        .unwrap_or("the model failed")
        .to_owned()
}

/// Up to `cap` bytes of `stream`, or `None` when it fails or is longer.
async fn read_up_to(mut stream: BodyStream, cap: usize) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    while let Some(chunk) = stream.next_chunk().await {
        out.extend_from_slice(&chunk.ok()?);
        if out.len() > cap {
            return None;
        }
    }
    Some(out)
}

/// The base a relayed call to `rest` goes to: the API for an API key,
/// except the model list, which only the ChatGPT backend has.
fn base_for(bases: Bases, headers: &HeaderMap, rest: &str) -> &'static str {
    if api_key(headers) && rest != "/models" {
        bases.api
    } else {
        bases.chatgpt
    }
}

/// Whether the caller's bearer is an API key (`sk-`).
fn api_key(headers: &HeaderMap) -> bool {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().strip_prefix("Bearer "))
        .is_some_and(|token| token.trim_start().starts_with("sk-"))
}

/// `headers` less the ones a relay does not pass on, and the ones the
/// `connection` header names.
fn relayed_headers(headers: &HeaderMap) -> HeaderMap {
    let nominated: Vec<String> = headers
        .get_all("connection")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|name| name.trim().to_ascii_lowercase())
        .collect();
    let mut out = headers.clone();
    for name in DROPPED {
        out.remove(name);
    }
    for name in nominated {
        out.remove(name.as_str());
    }
    out
}

/// The outbound request to `base` + `rest` with the caller's query.
fn outbound(
    base: &str,
    rest: &str,
    request: &HttpRequest,
    headers: HeaderMap,
) -> Result<HttpRequest, PluginError> {
    let mut target = format!("{base}{rest}");
    if let Some(query) = request.uri().query() {
        target.push('?');
        target.push_str(query);
    }
    let mut call = HttpRequest::new(request.body().clone());
    *call.method_mut() = request.method().clone();
    *call.uri_mut() = target
        .parse()
        .map_err(|_| PluginError::internal(format!("not a URL: {base}{rest}")))?;
    *call.headers_mut() = headers;
    Ok(call)
}

const REFUSAL_CAP: usize = 1024 * 1024;

/// The request as it came, to the upstream, and the answer as it streams.
async fn relay(
    kernel: &dyn Kernel,
    bases: Bases,
    rest: &str,
    mut request: HttpRequest,
    state: &State,
    tool_back: BTreeMap<String, String>,
) -> HttpResponse {
    let base = base_for(bases, request.headers(), rest);
    let turn =
        matches!(rest, "/responses" | "/responses/compact") && request.method().as_str() == "POST";
    let compact = rest == "/responses/compact";
    if turn && let Some(body) = state.strip(base, request.body().bytes()) {
        *request.body_mut() = Body::from(body);
    }
    let headers = relayed_headers(request.headers());
    loop {
        let sent = match outbound(base, rest, &request, headers.clone()) {
            Ok(call) => kernel.send(call, None).await,
            Err(failure) => Err(failure),
        };
        let response = match sent {
            Ok(response) => response,
            Err(failure) => return error_response(&failure),
        };
        let status = response.status().as_u16();
        let fallbacks = kernel.codex_relay_fallbacks();
        if turn
            && !compact
            && !api_key(request.headers())
            && rest == "/responses"
            && !fallbacks.is_empty()
        {
            let stream = relay_reply_is_stream(response.headers());
            let likely = stream || matches!(status, 403 | 429 | 529) || status >= 500;
            if likely {
                let (parts, body) = response.into_parts();
                let bytes = read_up_to(body, REFUSAL_CAP).await.unwrap_or_default();
                if account_refusal::codex_account_refusal(status, &parts.headers, &bytes, stream) {
                    for model in fallbacks {
                        let Some(rewritten) =
                            account_refusal::with_model(request.body().bytes(), model)
                        else {
                            continue;
                        };
                        let (body, compaction) = input::for_hofund(&rewritten);
                        if compaction {
                            continue;
                        }
                        let mut retry = HttpRequest::new(Body::from(body.unwrap_or(rewritten)));
                        *retry.method_mut() = request.method().clone();
                        *retry.uri_mut() = request.uri().clone();
                        *retry.headers_mut() = request.headers().clone();
                        let answer = in_process(kernel, &retry).await;
                        if answer.status().is_success() {
                            return answer;
                        }
                    }
                }
                return restore_relay_response(
                    HttpResponse::from_parts(parts, BodyStream::from_bytes(bytes)),
                    &tool_back,
                );
            }
        }
        if !turn || status != 400 {
            return restore_relay_response(response, &tool_back);
        }
        let (parts, body) = response.into_parts();
        let Some(body) = read_up_to(body, REFUSAL_CAP).await else {
            return error(502, "the upstream refusal could not be read");
        };
        if let Some(field) = refused_passthrough(&body) {
            let (stripped, changed) = without_passthrough(request.body().bytes(), &field);
            if changed && state.learn(base, &field) {
                *request.body_mut() = Body::from(stripped);
                continue;
            }
        }
        return restore_relay_response(
            HttpResponse::from_parts(parts, BodyStream::from_bytes(body)),
            &tool_back,
        );
    }
}

fn relay_reply_is_stream(headers: &HeaderMap) -> bool {
    headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"))
}

pub(crate) fn restore_relay_response(
    mut response: HttpResponse,
    back: &BTreeMap<String, String>,
) -> HttpResponse {
    if back.is_empty() {
        return response;
    }
    // The restored body can differ in length from the upstream's.
    response.headers_mut().remove("content-length");
    let stream = relay_reply_is_stream(response.headers());
    let body = std::mem::replace(
        response.body_mut(),
        BodyStream::from_bytes(Vec::<u8>::new()),
    );
    *response.body_mut() = restore_relay_body(body, back, stream);
    response
}

fn restore_relay_body(
    body: BodyStream,
    back: &BTreeMap<String, String>,
    event_stream: bool,
) -> BodyStream {
    let back = back.clone();
    if !event_stream {
        return BodyStream::unfold(Some(body), move |body| {
            let back = back.clone();
            async move {
                let mut body = body?;
                let mut bytes = Vec::new();
                while let Some(chunk) = body.next_chunk().await {
                    match chunk {
                        Ok(chunk) => bytes.extend_from_slice(&chunk),
                        Err(error) => return Some((Err(error), None)),
                    }
                }
                Some((Ok(input::restore_unary_reply(&bytes, &back)), None))
            }
        });
    }
    BodyStream::unfold(
        ReplyLines {
            body,
            back,
            pending: Vec::new(),
            finished: false,
        },
        |mut lines| async move {
            if lines.finished {
                return None;
            }
            loop {
                match lines.body.next_chunk().await {
                    None => {
                        lines.finished = true;
                        let rest = std::mem::take(&mut lines.pending);
                        if rest.is_empty() {
                            return None;
                        }
                        return Some((Ok(input::restore_sse_line(&rest, &lines.back)), lines));
                    }
                    Some(Err(error)) => {
                        lines.finished = true;
                        return Some((Err(error), lines));
                    }
                    Some(Ok(chunk)) => {
                        lines.pending.extend_from_slice(&chunk);
                        let Some(end) = lines.pending.iter().rposition(|byte| *byte == b'\n')
                        else {
                            continue;
                        };
                        let rest = lines.pending.split_off(end + 1);
                        let whole = std::mem::replace(&mut lines.pending, rest);
                        let mut out = Vec::with_capacity(whole.len());
                        for line in whole.split_inclusive(|byte| *byte == b'\n') {
                            out.extend(input::restore_sse_line(line, &lines.back));
                        }
                        return Some((Ok(out), lines));
                    }
                }
            }
        },
    )
}

struct ReplyLines {
    body: BodyStream,
    back: BTreeMap<String, String>,
    pending: Vec<u8>,
    finished: bool,
}

/// The backend's model list for this sign-in, with hofund's models after
/// it. A refused sign-in is the caller's to see. A list that does not come,
/// in time or at all, leaves hofund's models alone.
async fn models(kernel: &dyn Kernel, bases: Bases, request: &HttpRequest) -> HttpResponse {
    let mut headers = relayed_headers(request.headers());
    headers.remove("if-none-match");
    headers.remove("if-modified-since");
    let mut own = Vec::new();
    let sent = match outbound(bases.chatgpt, "/models", request, headers) {
        Ok(mut call) => {
            *call.body_mut() = Body::empty();
            kernel.send(call, Some(LIST_WAIT)).await
        }
        Err(failure) => Err(failure),
    };
    if let Ok(response) = sent {
        let status = response.status().as_u16();
        if status == 401 || status == 403 {
            return response;
        }
        if status < 300 {
            own = own_models(response.into_body()).await;
        }
    }
    let ours = listed_models(kernel).await.unwrap_or_default();
    let after = own
        .iter()
        .filter_map(|model| model.get("priority").and_then(Value::as_u64))
        .max()
        .unwrap_or(0);
    let cap = !official::reads_whole_catalog(client_version(request));
    let lookup = official::Lookup::with_codex_home();
    let mut all = own;
    for (rank, model) in ours.iter().enumerate() {
        let priority = usize::try_from(after)
            .unwrap_or(usize::MAX)
            .saturating_add(rank + 1);
        all.push(entry(&model.id, priority, &lookup, model.context_window));
    }
    let (body, left_out) = capped_models(all, cap);
    let mut response = json_response(200, body);
    if cap
        && left_out > 0
        && let Ok(value) = left_out.to_string().parse()
    {
        response.headers_mut().insert(crate::LEFT_OUT_HEADER, value);
    }
    response
}

/// The entries of a backend model list, or none when it is not one.
async fn own_models(mut stream: BodyStream) -> Vec<Value> {
    let mut body = Vec::new();
    while let Some(chunk) = stream.next_chunk().await {
        let Ok(chunk) = chunk else {
            return Vec::new();
        };
        body.extend_from_slice(&chunk);
        if body.len() > OWN_LIST_CAP {
            return Vec::new();
        }
    }
    json::from_slice::<Value>(&body)
        .ok()
        .and_then(|mut list| list.get_mut("models").map(Value::take))
        .and_then(|models| match models {
            Value::Array(models) => Some(models),
            _ => None,
        })
        .unwrap_or_default()
}
