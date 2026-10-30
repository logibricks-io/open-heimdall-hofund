//! Muse Code's model list on the gateway's own address.
//!
//! Muse Code asks `GET /muse-code/models` on its endpoint's host before every
//! session and starts none without it. It shows a row only when the row's
//! `metadata["muse-code"]` says how to treat the model, and it hides a model
//! whose `limit` lacks the window or the reply size. The ids are the ones
//! `GET /v1/models` lists, which Muse sends back as the model it asks for.
//!
//! None of it opens a socket: the list is the gateway's own, read in process.
//! The kernel knows no window, reply size, image, or reasoning support per
//! model yet, so every model is told a window of 128000 tokens, replies of
//! 32000, text in and out, and tool calls.

#![deny(missing_docs)]

use hofund_plugin_api::{
    Body, BodyStream, GatewayHandle, InboundSurface, Method, PluginError, RouteSpec, SurfaceHost,
    SurfaceId,
};
use hofund_plugin_sdk::json::{self, Value, json};
use hofund_plugin_sdk::{Contribution, HttpRequest, HttpResponse, async_trait, text_response};

/// Registers the Muse surface.
#[must_use]
pub fn contribute() -> Contribution {
    Contribution::new().surface(Muse)
}

/// The window a model of unknown window is shown with.
pub const CONTEXT: u64 = 128_000;

/// The most a reply may hold, for a model whose own limit is unknown: Muse's
/// own default. Without one Muse asks for replies as long as the window.
pub const OUTPUT: u64 = 32_000;

/// The key Muse reads a row's metadata under.
const METADATA_KEY: &str = "muse-code";

/// How much of the gateway's model list is read.
const LIST_CAP: usize = 4 * 1024 * 1024;

const MODELS_PATH: &str = "/muse-code/models";

static ID: SurfaceId = SurfaceId::from_static("muse");
static ROUTES: [RouteSpec; 1] = [RouteSpec::from_static(Method::Get, MODELS_PATH)];

/// The Muse surface.
#[derive(Clone, Copy, Debug)]
pub struct Muse;

#[async_trait]
impl InboundSurface for Muse {
    fn id(&self) -> &SurfaceId {
        &ID
    }

    fn routes(&self) -> &'static [RouteSpec] {
        &ROUTES
    }

    async fn handle(
        &self,
        _route: &RouteSpec,
        request: HttpRequest,
        host: &SurfaceHost,
    ) -> Result<HttpResponse, PluginError> {
        Ok(serve(host.gateway(), &request).await)
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

/// Answers `request`. The route matches a deeper path too; Muse asks none,
/// so one is not found.
async fn serve(kernel: &dyn Kernel, request: &HttpRequest) -> HttpResponse {
    if request.uri().path() != MODELS_PATH {
        return text_response(404, "404 page not found");
    }
    match models(kernel).await {
        Ok(rows) => json_response(200, &json!({"object": "list", "data": rows})),
        Err(error) => {
            let status = error
                .status()
                .filter(|status| *status >= 400)
                .unwrap_or(502);
            json_response(
                status,
                &json!({"error": {"message": error.message(), "type": "hofund_error"}}),
            )
        }
    }
}

/// `value` as a JSON reply.
fn json_response(status: u16, value: &Value) -> HttpResponse {
    let mut response = text_response(status, value.to_string());
    if let Ok(value) = "application/json".parse() {
        response.headers_mut().insert("content-type", value);
    }
    response
}

/// The gateway's models, as Muse's rows.
async fn models(kernel: &dyn Kernel) -> Result<Vec<Value>, PluginError> {
    let mut request = HttpRequest::new(Body::empty());
    *request.uri_mut() = "/v1/models"
        .parse()
        .map_err(|_| PluginError::internal("the model list path"))?;
    let response = kernel.call(request).await?;
    let status = response.status().as_u16();
    let body = read_capped(response.into_body()).await?;
    if status >= 300 {
        return Err(PluginError::status_error(status, failure_message(&body)));
    }
    let list: Value = json::from_slice(&body)
        .map_err(|error| PluginError::decode(format!("the model list: {error}")))?;
    Ok(list
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(row)
        .collect())
}

/// One model of `/v1/models` as Muse's row, or none without an id.
fn row(model: &Value) -> Option<Value> {
    let id = model.get("id").and_then(Value::as_str)?;
    let owner = model
        .get("owned_by")
        .and_then(Value::as_str)
        .filter(|owner| !owner.is_empty())
        .unwrap_or("hofund");
    Some(json!({
        "id": id, "object": "model", "created": 0, "owned_by": owner,
        "metadata": {METADATA_KEY: {
            "name": id, "family": owner, "is_hidden": false, "attachment": false,
            "reasoning": false, "temperature": false, "tool_call": true,
            "modalities": {"input": ["text"], "output": ["text"]},
            "options": {"include": []}, "variants": {},
            "description": format!("{id} via hofund"),
            "limit": {"context": CONTEXT, "output": OUTPUT},
        }},
    }))
}

/// Up to [`LIST_CAP`] bytes of `stream`.
async fn read_capped(mut stream: BodyStream) -> Result<Vec<u8>, PluginError> {
    let mut out = Vec::new();
    while let Some(chunk) = stream.next_chunk().await {
        out.extend_from_slice(&chunk?);
        if out.len() > LIST_CAP {
            return Err(PluginError::decode(format!(
                "the model list is over {LIST_CAP} bytes"
            )));
        }
    }
    Ok(out)
}

/// The message of a failed model list: `error.message`, or the text.
fn failure_message(body: &[u8]) -> String {
    let parsed = json::from_slice::<Value>(body).ok();
    if let Some(message) = parsed
        .as_ref()
        .and_then(|value| value.pointer("/error/message"))
        .and_then(Value::as_str)
        .filter(|message| !message.is_empty())
    {
        return message.to_owned();
    }
    let text = String::from_utf8_lossy(body).trim().to_owned();
    if text.is_empty() {
        "the model list failed".to_owned()
    } else {
        text
    }
}

#[cfg(test)]
mod tests;
