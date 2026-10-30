//! The Ollama API on the gateway's own address.
//!
//! For a client that takes a custom endpoint only as an Ollama server, such
//! as Visual Studio's Copilot or VS Code's Copilot "Ollama" provider. The
//! client asks `GET /api/version`, lists `GET /api/tags`, asks
//! `POST /api/show` of each model, and chats on `POST /api/chat` or
//! `POST /api/generate`, or on the gateway's own `/v1/chat/completions`.
//!
//! None of it opens a socket. The model list is the gateway's
//! `GET /v1/models`, and a chat is translated to a Chat Completions request
//! the gateway runs in process, its reply translated back. The upstream call
//! always streams, so a long reply is not cut by the surface's time limit;
//! one asked with `"stream": false` is gathered here. A failure after the
//! reply started is an `{"error": ...}` line in a `200` reply, as Ollama's
//! own.
//!
//! The kernel knows no window, image, or thinking support per model yet, so
//! every model shows the capabilities `completion` and `tools` and a window
//! of 32768, the one VS Code's Copilot assumes without one.

#![deny(missing_docs)]

mod reply;
mod request;

use std::time::Instant;

use hofund_plugin_api::{
    Body, BodyStream, GatewayHandle, InboundSurface, Method, PluginError, RouteSpec, SurfaceHost,
    SurfaceId,
};
use hofund_plugin_sdk::json::{self, Value, json};
use hofund_plugin_sdk::{
    Contribution, HeaderMap, HttpRequest, HttpResponse, async_trait, sigv4, text_response,
};

use crate::reply::Writer;
use crate::request::text;

/// Registers the Ollama surface.
#[must_use]
pub fn contribute() -> Contribution {
    Contribution::new().surface(Ollama)
}

/// The Ollama release `/api/version` answers: past every client's minimum
/// (VS Code's Copilot asks for 0.6.4).
pub const VERSION: &str = "0.12.6";

/// The architecture every model is shown as. `model_info`'s keys are named
/// after it.
const ARCHITECTURE: &str = "hofund";

/// The window shown for a model whose own is unknown.
const DEFAULT_CONTEXT: u64 = 32_768;

/// Stands in for when a model was pulled, which none here was.
const MODIFIED: &str = "2025-01-01T00:00:00Z";

/// How much of a failed reply is read for its message.
const ERROR_CAP: usize = 64 * 1024;

const VERSION_PATH: &str = "/api/version";
const TAGS_PATH: &str = "/api/tags";
const SHOW_PATH: &str = "/api/show";
const CHAT_PATH: &str = "/api/chat";
const GENERATE_PATH: &str = "/api/generate";

static ID: SurfaceId = SurfaceId::from_static("ollama");
static ROUTES: [RouteSpec; 5] = [
    RouteSpec::from_static(Method::Get, VERSION_PATH),
    RouteSpec::from_static(Method::Get, TAGS_PATH),
    RouteSpec::from_static(Method::Post, SHOW_PATH),
    RouteSpec::from_static(Method::Post, CHAT_PATH),
    RouteSpec::from_static(Method::Post, GENERATE_PATH),
];

/// The Ollama surface.
#[derive(Clone, Copy, Debug)]
pub struct Ollama;

#[async_trait]
impl InboundSurface for Ollama {
    fn id(&self) -> &SurfaceId {
        &ID
    }

    fn routes(&self) -> &'static [RouteSpec] {
        &ROUTES
    }

    async fn handle(
        &self,
        route: &RouteSpec,
        request: HttpRequest,
        host: &SurfaceHost,
    ) -> Result<HttpResponse, PluginError> {
        Ok(serve(host.gateway(), route.prefix(), &request).await)
    }
}

/// What the surface asks of the gateway, so tests can stand in for it.
#[async_trait]
trait Kernel: Send + Sync {
    async fn call(&self, request: HttpRequest) -> Result<HttpResponse, PluginError>;
}

#[async_trait]
impl Kernel for GatewayHandle {
    async fn call(&self, request: HttpRequest) -> Result<HttpResponse, PluginError> {
        Self::call(self, request).await
    }
}

/// Answers `request` on the route at `prefix`. A route matches a deeper
/// path too; Ollama has none, so one is not found.
async fn serve(kernel: &dyn Kernel, prefix: &str, request: &HttpRequest) -> HttpResponse {
    if request.uri().path() != prefix {
        return text_response(404, "404 page not found");
    }
    let body = request.body().bytes();
    let answer = match prefix {
        VERSION_PATH => Ok(json_response(
            200,
            &json!({"version": VERSION}),
            "application/json",
        )),
        TAGS_PATH => tags(kernel).await,
        SHOW_PATH => show(kernel, body).await,
        CHAT_PATH => chat(kernel, body, false).await,
        GENERATE_PATH => chat(kernel, body, true).await,
        _ => return text_response(404, "404 page not found"),
    };
    answer.unwrap_or_else(|error| {
        let status = error
            .status()
            .filter(|status| *status >= 400)
            .unwrap_or(502);
        failure(status, error.message())
    })
}

/// `value` as a JSON reply.
fn json_response(status: u16, value: &Value, content_type: &str) -> HttpResponse {
    let mut body = value.to_string().into_bytes();
    body.push(b'\n');
    bytes_response(status, body, content_type)
}

fn bytes_response(status: u16, body: Vec<u8>, content_type: &str) -> HttpResponse {
    let mut response = text_response(status, body);
    if let Ok(value) = content_type.parse() {
        response.headers_mut().insert("content-type", value);
    }
    response
}

/// An error as Ollama's API says one.
fn failure(status: u16, message: &str) -> HttpResponse {
    bytes_response(status, reply::error_body(message), "application/json")
}

/// A request for the gateway.
fn gateway_request(method: &str, path: &str, body: Vec<u8>) -> Result<HttpRequest, PluginError> {
    let mut request = HttpRequest::new(Body::from_bytes(body));
    *request.method_mut() = method
        .parse()
        .map_err(|_| PluginError::internal(format!("method {method}")))?;
    *request.uri_mut() = path
        .parse()
        .map_err(|_| PluginError::internal(format!("path {path}")))?;
    if let Ok(value) = "application/json".parse() {
        request.headers_mut().insert("content-type", value);
    }
    Ok(request)
}

/// The model ids the gateway lists.
async fn models(kernel: &dyn Kernel) -> Result<Vec<String>, PluginError> {
    let response = kernel
        .call(gateway_request("GET", "/v1/models", Vec::new())?)
        .await?;
    let status = response.status().as_u16();
    let body = read_capped(response.into_body()).await?;
    if status >= 300 {
        return Err(PluginError::status_error(
            status,
            reply::failure_message(status, &body),
        ));
    }
    let list: Value = json::from_slice(&body)
        .map_err(|error| PluginError::decode(format!("the model list: {error}")))?;
    Ok(list
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|model| model.get("id").and_then(Value::as_str))
        .map(str::to_owned)
        .collect())
}

/// Up to [`ERROR_CAP`] bytes of `stream`.
async fn read_capped(mut stream: BodyStream) -> Result<Vec<u8>, PluginError> {
    let mut out = Vec::new();
    while let Some(chunk) = stream.next_chunk().await {
        out.extend_from_slice(&chunk?);
        if out.len() >= ERROR_CAP {
            out.truncate(ERROR_CAP);
            break;
        }
    }
    Ok(out)
}

/// The model a request names: the ollama CLI names a model with no tag as
/// `name:latest`, which is the bare id unless a model is really called that.
fn resolve(ids: &[String], name: &str) -> String {
    if ids.iter().any(|id| id == name) {
        return name.to_owned();
    }
    match name.strip_suffix(":latest") {
        Some(bare) if ids.iter().any(|id| id == bare) => bare.to_owned(),
        _ => name.to_owned(),
    }
}

/// A model's details, as `/api/tags` and `/api/show` give them.
fn details() -> Value {
    json!({
        "parent_model": "", "format": "", "family": ARCHITECTURE, "families": [ARCHITECTURE],
        "parameter_size": "", "quantization_level": "",
    })
}

/// What every model is shown to do: complete and call tools.
fn capabilities() -> Value {
    json!(["completion", "tools"])
}

/// The models, as Ollama lists the pulled ones. `name` and `model` are the
/// id the client sends back to chat with.
async fn tags(kernel: &dyn Kernel) -> Result<HttpResponse, PluginError> {
    let models: Vec<Value> = models(kernel)
        .await?
        .into_iter()
        .map(|id| {
            json!({
                "name": id, "model": id, "modified_at": MODIFIED, "size": 0,
                "digest": sigv4::payload_hash(id.as_bytes()), "details": details(),
                "capabilities": capabilities(),
            })
        })
        .collect();
    Ok(json_response(
        200,
        &json!({"models": models}),
        "application/json",
    ))
}

/// The request body as a JSON object, or the reply refusing it.
fn object(body: &[u8]) -> Result<Value, HttpResponse> {
    match json::from_slice::<Value>(body) {
        Ok(value @ Value::Object(_)) => Ok(value),
        Ok(_) => Err(failure(
            400,
            "invalid request: the body is not a JSON object",
        )),
        Err(error) => Err(failure(400, &format!("invalid request: {error}"))),
    }
}

/// One model, as `/api/show` tells it. Every key is sent: the ollama CLI
/// before 0.7 reads `parameter_count`, `context_length`, and
/// `embedding_length` without checking them.
async fn show(kernel: &dyn Kernel, body: &[u8]) -> Result<HttpResponse, PluginError> {
    let request = match object(body) {
        Ok(request) => request,
        Err(refused) => return Ok(refused),
    };
    let mut name = text(&request, "model").trim();
    if name.is_empty() {
        // What clients before Ollama 0.5 send.
        name = text(&request, "name").trim();
    }
    if name.is_empty() {
        return Ok(failure(400, "model is required"));
    }
    let ids = models(kernel).await?;
    let id = resolve(&ids, name);
    if !ids.contains(&id) {
        return Ok(failure(404, &format!("model '{id}' not found")));
    }
    let mut info = json::Map::new();
    info.insert("general.architecture".to_owned(), json!(ARCHITECTURE));
    info.insert("general.basename".to_owned(), json!(id));
    info.insert("general.parameter_count".to_owned(), json!(0));
    info.insert(
        format!("{ARCHITECTURE}.context_length"),
        json!(DEFAULT_CONTEXT),
    );
    info.insert(format!("{ARCHITECTURE}.embedding_length"), json!(0));
    Ok(json_response(
        200,
        &json!({
            "modelfile": "", "parameters": "", "template": "", "details": details(),
            "model_info": info, "capabilities": capabilities(), "modified_at": MODIFIED,
        }),
        "application/json",
    ))
}

/// `/api/chat` or, with `generate`, `/api/generate`.
async fn chat(
    kernel: &dyn Kernel,
    body: &[u8],
    generate: bool,
) -> Result<HttpResponse, PluginError> {
    let start = Instant::now();
    let request = match object(body) {
        Ok(request) => request,
        Err(refused) => return Ok(refused),
    };
    let name = text(&request, "model").trim();
    if name.is_empty() {
        return Ok(failure(400, "model is required"));
    }
    let ids = models(kernel).await?;
    let model = resolve(&ids, name);
    let stream = request.get("stream") != Some(&Value::Bool(false));
    let content_type = if stream {
        "application/x-ndjson"
    } else {
        "application/json"
    };
    if request::is_empty(&request, generate) {
        // A request with nothing to say loads the model.
        if !ids.contains(&model) {
            return Ok(failure(404, &format!("model '{model}' not found")));
        }
        return Ok(bytes_response(
            200,
            reply::load_reply(&model, generate),
            content_type,
        ));
    }
    let wire = match request::chat_body(&request, &model, generate) {
        Ok(wire) => wire,
        Err(message) => return Ok(failure(400, &message)),
    };
    let upstream = kernel
        .call(gateway_request(
            "POST",
            "/v1/chat/completions",
            wire.to_string().into_bytes(),
        )?)
        .await?;
    let status = upstream.status().as_u16();
    let (parts, body) = upstream.into_parts();
    if status >= 300 {
        let text = read_capped(body).await?;
        let mut response = failure(status, &reply::failure_message(status, &text));
        pass_headers(&parts.headers, response.headers_mut());
        return Ok(response);
    }
    let sse = parts
        .headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"));
    let writer = Writer::new(
        &model,
        generate,
        stream,
        request::thinking_off(&request),
        sse,
        start,
    );
    let mut response = bytes_response(200, Vec::new(), content_type);
    pass_headers(&parts.headers, response.headers_mut());
    *response.body_mut() = translate(body, writer);
    Ok(response)
}

/// What says why a reply waits or failed (`Retry-After`, the gateway's own
/// `x-` marks) reaches the client as it would on `/v1`.
fn pass_headers(from: &HeaderMap, to: &mut HeaderMap) {
    for (name, value) in from {
        let name_text = name.as_str();
        if name_text == "retry-after" || name_text.starts_with("x-") {
            to.append(name.clone(), value.clone());
        }
    }
}

/// The Chat reply `upstream`, as Ollama's.
fn translate(upstream: BodyStream, writer: Writer) -> BodyStream {
    BodyStream::unfold(
        (Some(upstream), writer),
        |(mut upstream, mut writer)| async move {
            loop {
                let source = upstream.as_mut()?;
                let out = match source.next_chunk().await {
                    Some(Ok(chunk)) => writer.feed(&chunk, Instant::now()),
                    Some(Err(error)) => {
                        upstream = None;
                        writer.fail(error.message())
                    }
                    None => {
                        upstream = None;
                        writer.finish(Instant::now())
                    }
                };
                if writer.is_done() {
                    upstream = None;
                }
                if !out.is_empty() {
                    return Some((Ok(out), (upstream, writer)));
                }
            }
        },
    )
}

#[cfg(test)]
mod tests;
