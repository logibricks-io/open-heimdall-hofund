//! Embeddings and reranking on the gateway's own address.
//!
//! `POST /v1/embeddings` and `POST /v1/rerank` take a JSON object that names
//! a `model`. The kernel routes it like a chat and forwards the body to each
//! provider of the model in turn, with `model` set to that provider's own id,
//! posting it to the same path under the provider's OpenAI-style base URL. A
//! provider that serves no such endpoint, is out of quota, or fails passes the
//! call on to the next. The first answer comes back as the provider sent it.

#![deny(missing_docs)]

use hofund_plugin_api::{
    BodyStream, Forward, GatewayHandle, InboundSurface, Method, PluginError, PluginErrorKind,
    RawAnswer, RawBody, RouteSpec, SurfaceHost, SurfaceId,
};
use hofund_plugin_sdk::json::{self, Map, Value, json};
use hofund_plugin_sdk::{Contribution, HttpRequest, HttpResponse, async_trait, text_response};

/// Registers the retrieval surface.
#[must_use]
pub fn contribute() -> Contribution {
    Contribution::new().surface(Retrieval)
}

const EMBEDDINGS: &str = "/v1/embeddings";
const RERANK: &str = "/v1/rerank";
const JSON: &str = "application/json";

static ID: SurfaceId = SurfaceId::from_static("retrieval");
static ROUTES: [RouteSpec; 2] = [
    RouteSpec::from_static(Method::Post, EMBEDDINGS),
    RouteSpec::from_static(Method::Post, RERANK),
];

/// The retrieval surface.
#[derive(Clone, Copy, Debug)]
pub struct Retrieval;

#[async_trait]
impl InboundSurface for Retrieval {
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
    async fn forward(&self, call: Forward) -> Result<RawAnswer, PluginError>;
}

#[async_trait]
impl Kernel for GatewayHandle {
    async fn forward(&self, call: Forward) -> Result<RawAnswer, PluginError> {
        Self::forward(self, call).await
    }
}

/// Answers `request`. The routes match a deeper path too; none is served.
async fn serve(kernel: &dyn Kernel, request: &HttpRequest) -> HttpResponse {
    let path = request.uri().path();
    let upstream_path = match path {
        EMBEDDINGS => "/embeddings",
        RERANK => "/rerank",
        _ => return text_response(404, "404 page not found"),
    };
    let call = match call(upstream_path, request.body().bytes()) {
        Ok(call) => call,
        Err(error) => return failure(&error),
    };
    match kernel.forward(call).await {
        Ok(answer) => {
            let mut response = text_response(answer.status(), "");
            *response.body_mut() = BodyStream::from_bytes(answer.body().clone());
            set_json(&mut response);
            response
        }
        Err(error) => failure(&error),
    }
}

/// The call for `body`, which must be a JSON object naming a model.
fn call(path: &str, body: &[u8]) -> Result<Forward, PluginError> {
    let Ok(Value::Object(object)) = json::from_slice::<Value>(body) else {
        return Err(PluginError::status_error(
            400,
            "the body must be a JSON object",
        ));
    };
    let model = match object.get("model") {
        Some(Value::String(model)) if !model.is_empty() => model.clone(),
        _ => return Err(PluginError::status_error(400, "model is required")),
    };
    Forward::new(model, path, move |model| Ok(with_model(&object, model)))
}

/// `object` with `model` set to `model`, as a JSON body.
fn with_model(object: &Map<String, Value>, model: &str) -> RawBody {
    let mut object = object.clone();
    object.insert("model".to_owned(), Value::String(model.to_owned()));
    RawBody::new(JSON, Value::Object(object).to_string())
}

/// `error` as an OpenAI-style error reply.
fn failure(error: &PluginError) -> HttpResponse {
    let status = match error.status().filter(|status| *status >= 400) {
        Some(status) => status,
        None if error.kind() == PluginErrorKind::Timeout => 504,
        None => 502,
    };
    let kind = if status == 400 {
        "invalid_request_error"
    } else {
        "hofund_error"
    };
    let body = json!({"error": {"message": error.message(), "type": kind}});
    let mut response = text_response(status, body.to_string());
    set_json(&mut response);
    response
}

fn set_json(response: &mut HttpResponse) {
    if let Ok(value) = JSON.parse() {
        response.headers_mut().insert("content-type", value);
    }
}

#[cfg(test)]
mod tests;
