//! Codex's ChatGPT backend on the gateway's own address.
//!
//! A Codex signed in to ChatGPT can point its built-in provider at
//! `<gateway>/backend-api/codex` and keep that sign-in. Its own models go on
//! to the ChatGPT backend as they came; hofund's models, the ids with a `/`,
//! are served here, in process, as `/v1/responses`. The gateway token never
//! reaches this code: the gateway takes it out first, so what is relayed is
//! the caller's own sign-in.

use std::time::Duration;

use hofund_plugin_api::{Body, BodyStream, PluginError};
use hofund_plugin_sdk::json::{self, Value, json};
use hofund_plugin_sdk::{HeaderMap, HttpRequest, HttpResponse, text_response};

use crate::{Kernel, entry, error_response, ids, json_response};

/// The route prefix Codex's base URL ends with.
pub(crate) const PREFIX: &str = "/backend-api/codex";

/// How long the backend's model list is waited for before hofund's models
/// are answered alone. Codex gives up on the list at five seconds.
pub(crate) const LIST_WAIT: Duration = Duration::from_secs(3);

/// How much of the backend's model list is read.
const OWN_LIST_CAP: usize = 16 * 1024 * 1024;

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
pub(crate) async fn serve(kernel: &dyn Kernel, bases: Bases, request: HttpRequest) -> HttpResponse {
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
    if !get && (compact || rest == "/responses") && is_hofund_model(request.body()) {
        if compact {
            return error(400, COMPACT_MESSAGE);
        }
        return in_process(kernel, &request).await;
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

/// The request as the gateway's `/v1/responses`, served in process.
async fn in_process(kernel: &dyn Kernel, request: &HttpRequest) -> HttpResponse {
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
    match kernel.call(call).await {
        Ok(response) => response,
        Err(failure) => error_response(&failure),
    }
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
