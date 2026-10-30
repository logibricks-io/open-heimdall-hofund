//! Codex's ChatGPT backend on the gateway's own address.
//!
//! A Codex signed in to ChatGPT can point its built-in provider at
//! `<gateway>/backend-api/codex` and keep that sign-in. Its own models go on
//! to the ChatGPT backend, their input put right for it first; hofund's
//! models, the ids with a `/`, are served here, in process, as
//! `/v1/responses`, and compacted here too. The gateway token never
//! reaches this code: the gateway takes it out first, so what is relayed is
//! the caller's own sign-in.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hofund_plugin_api::{Body, BodyStream, PluginError};
use hofund_plugin_sdk::json::{self, Value, json};
use hofund_plugin_sdk::{HeaderMap, HttpRequest, HttpResponse, text_response};

use crate::input::{self, sealed};
use crate::{Kernel, entry, error_response, ids, json_response};

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

/// Answers `request` under [`PREFIX`].
pub(crate) async fn serve(
    kernel: &dyn Kernel,
    bases: Bases,
    mut request: HttpRequest,
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
        if is_hofund_model(request.body()) {
            if compact {
                return error(400, COMPACT_MESSAGE);
            }
            let (body, compaction) = input::for_hofund(request.body().bytes());
            if let Some(body) = body {
                *request.body_mut() = Body::from(body);
            }
            if compaction {
                return summarise(kernel, &request).await;
            }
            return in_process(kernel, &request).await;
        }
        if let Some(body) = input::for_backend(request.body().bytes()) {
            *request.body_mut() = Body::from(body);
        }
    }
    relay(kernel, bases, &rest, request).await
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
async fn summarise(kernel: &dyn Kernel, request: &HttpRequest) -> HttpResponse {
    let mut call = local_call(request);
    set(call.headers_mut(), "accept", "application/json");
    let response = match kernel.call(call).await {
        Ok(response) => response,
        Err(failure) => return error_response(&failure),
    };
    let status = response.status().as_u16();
    let Some(body) = read_up_to(response.into_body(), SUMMARY_CAP).await else {
        return error(502, "compaction: the summary reply could not be read");
    };
    if status >= 400 {
        let said = said(&body);
        return error(
            status,
            &format!("compaction: hofund answered the summary request {status}: {said}"),
        );
    }
    let reply = match Reply::of(&body) {
        Ok(reply) => reply,
        Err(why) => return error(502, &format!("compaction: {why}, so there is no summary")),
    };
    if reply.status == "incomplete" {
        let why = reply
            .incomplete
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        return error(
            502,
            &format!("compaction: the summary was cut short ({why}), so there is no summary"),
        );
    }
    let summary = reply.summary();
    if summary.trim().is_empty() {
        let with = answered_with(&reply.output);
        return error(
            502,
            &format!("compaction: the model wrote no summary{with}"),
        );
    }
    compaction_stream(&reply.id(), &summary, &reply.usage)
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
    let api_key = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().strip_prefix("Bearer "))
        .is_some_and(|token| token.trim_start().starts_with("sk-"));
    if api_key && rest != "/models" {
        bases.api
    } else {
        bases.chatgpt
    }
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

/// The request as it came, to the upstream, and the answer as it streams.
async fn relay(
    kernel: &dyn Kernel,
    bases: Bases,
    rest: &str,
    request: HttpRequest,
) -> HttpResponse {
    let base = base_for(bases, request.headers(), rest);
    let headers = relayed_headers(request.headers());
    let sent = match outbound(base, rest, &request, headers) {
        Ok(call) => kernel.send(call, None).await,
        Err(failure) => Err(failure),
    };
    match sent {
        Ok(response) => response,
        Err(failure) => error_response(&failure),
    }
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
    let ours = ids(kernel).await.unwrap_or_default();
    let after = own
        .iter()
        .filter_map(|model| model.get("priority").and_then(Value::as_u64))
        .max()
        .unwrap_or(0);
    let mut all = own;
    for (rank, id) in ours.iter().enumerate() {
        let priority = usize::try_from(after)
            .unwrap_or(usize::MAX)
            .saturating_add(rank + 1);
        all.push(entry(id, priority));
    }
    json_response(200, json!({"models": all}).to_string())
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
