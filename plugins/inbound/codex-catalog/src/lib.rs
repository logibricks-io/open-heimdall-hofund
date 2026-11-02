//! Codex's model catalog, and its ChatGPT backend, on the gateway's own
//! address.
//!
//! A Codex whose model provider is this gateway (`base_url` ending `/v1`)
//! reads its model picker from `model_catalog_url`, `GET /v1/codex/models`.
//! `/v1/models` is OpenAI's list, which Codex cannot read, so this route
//! answers the same models in Codex's own `models.json` shape:
//! `{"models": [...]}`, one entry per model `/v1/models` lists.
//!
//! None of it opens a socket: the list is the gateway's own, read in
//! process. The kernel knows no window, effort levels, image support or
//! service tiers per model yet, so each entry takes text only, offers no
//! effort level and no tier, and leaves the window to Codex. Codex before
//! 0.166.0 reads at most [`MOST`] bytes of a catalog and loads none of it
//! past that, so entries that would go past are left out, the last first, and
//! the `x-hofund-left-out` header says how many. From 0.166.0 on, Codex
//! sends `?client_version=` and reads the whole list; the cap is off then.
//!
//! The same surface serves `/backend-api/codex`, for a Codex that keeps its
//! ChatGPT sign-in: see the `backend` module. It lives here because it lists
//! hofund's models with the same catalog entries, and the registry takes
//! one surface per plugin.

#![deny(missing_docs)]

use std::time::Duration;

use hofund_plugin_api::{
    Body, BodyStream, GatewayHandle, InboundSurface, Method, PluginError, RouteSpec, SurfaceHost,
    SurfaceId,
};
use hofund_plugin_sdk::json::{self, Value, json};
use hofund_plugin_sdk::{Contribution, HttpRequest, HttpResponse, async_trait, text_response};

/// Registers the Codex surface: the catalog and the backend.
#[must_use]
pub fn contribute() -> Contribution {
    Contribution::new().surface(CodexCatalog::default())
}

/// The most of a catalog Codex reads; past it the whole list fails to load.
pub const MOST: usize = 1 << 20;

mod official;

mod backend;

mod input;

/// The header that says how many models were left out to fit [`MOST`].
pub const LEFT_OUT_HEADER: &str = "x-hofund-left-out";

/// How much of the gateway's model list is read.
const LIST_CAP: usize = 4 * 1024 * 1024;

/// Default auto-compact threshold for catalog models with no known window.
pub const DEFAULT_AUTO_COMPACT_TOKEN_LIMIT: u64 = 272_000;

/// One model `/v1/models` lists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ListedModel {
    pub id: String,
    pub context_window: Option<u64>,
}

const CATALOG_PATH: &str = "/v1/codex/models";
const HEAD: &str = r#"{"models":["#;
const TAIL: &str = "]}\n";

static ID: SurfaceId = SurfaceId::from_static("codex-catalog");
static ROUTES: [RouteSpec; 3] = [
    RouteSpec::from_static(Method::Get, CATALOG_PATH),
    RouteSpec::from_static(Method::Get, backend::PREFIX),
    RouteSpec::from_static(Method::Post, backend::PREFIX),
];
static RESPONSES_SOCKET_PATHS: [&str; 1] = ["/backend-api/codex/responses"];

/// The Codex surface: the catalog, and the ChatGPT backend under `/backend-api/codex`.
#[derive(Clone, Debug, Default)]
pub struct CodexCatalog {
    backend: backend::State,
}

#[async_trait]
impl InboundSurface for CodexCatalog {
    fn id(&self) -> &SurfaceId {
        &ID
    }

    fn routes(&self) -> &'static [RouteSpec] {
        &ROUTES
    }

    fn responses_websocket_paths(&self) -> &'static [&'static str] {
        &RESPONSES_SOCKET_PATHS
    }

    async fn handle(
        &self,
        route: &RouteSpec,
        request: HttpRequest,
        host: &SurfaceHost,
    ) -> Result<HttpResponse, PluginError> {
        let host = Host(host);
        if route.prefix() == backend::PREFIX {
            return Ok(
                backend::serve_with_state(&host, backend::BASES, request, &self.backend).await,
            );
        }
        Ok(serve(&host, &request).await)
    }
}

/// Answers `request`, which came in on the route with `prefix`.
#[cfg(test)]
async fn dispatch(kernel: &dyn Kernel, prefix: &str, request: HttpRequest) -> HttpResponse {
    if prefix == backend::PREFIX {
        return backend::serve(kernel, backend::BASES, request).await;
    }
    serve(kernel, &request).await
}

/// What the surface asks of the kernel, so tests can stand in for it.
#[async_trait]
trait Kernel: Send + Sync {
    /// A call to the gateway's own routes, in process.
    async fn call(&self, request: HttpRequest) -> Result<HttpResponse, PluginError>;

    /// A call out on the kernel's pooled client, given up after `within`.
    async fn send(
        &self,
        request: HttpRequest,
        within: Option<Duration>,
    ) -> Result<HttpResponse, PluginError>;
}

/// The kernel as the surface host gives it.
struct Host<'a>(&'a SurfaceHost);

#[async_trait]
impl Kernel for Host<'_> {
    async fn call(&self, request: HttpRequest) -> Result<HttpResponse, PluginError> {
        GatewayHandle::call(self.0.gateway(), request).await
    }

    async fn send(
        &self,
        request: HttpRequest,
        within: Option<Duration>,
    ) -> Result<HttpResponse, PluginError> {
        let Some(http) = self.0.http() else {
            return Err(PluginError::unsupported(
                "this gateway gives surfaces no outbound HTTP",
            ));
        };
        match within {
            None => http.send(request).await,
            Some(within) => hofund_plugin_sdk::timeout(within, http.send(request))
                .await
                .unwrap_or_else(|| {
                    Err(PluginError::timeout("the upstream did not answer in time"))
                }),
        }
    }
}

/// Answers `request`. The route matches a deeper path too; Codex asks none, so
/// one is not found.
async fn serve(kernel: &dyn Kernel, request: &HttpRequest) -> HttpResponse {
    if request.uri().path() != CATALOG_PATH {
        return text_response(404, "404 page not found");
    }
    match listed_models(kernel).await {
        Ok(models) => {
            let cap = !official::reads_whole_catalog(client_version(request));
            catalog_response(&models, cap)
        }
        Err(error) => error_response(&error),
    }
}

/// Builds a catalog HTTP answer for `models`, capping size when `cap` is set.
pub(crate) fn catalog_response(models: &[ListedModel], cap: bool) -> HttpResponse {
    let (body, left_out) = catalog(models, cap);
    let mut response = json_response(200, body);
    if cap
        && left_out > 0
        && let Ok(value) = left_out.to_string().parse()
    {
        response.headers_mut().insert(LEFT_OUT_HEADER, value);
    }
    response
}

pub(crate) fn client_version(request: &HttpRequest) -> Option<&str> {
    let query = request.uri().query()?;
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        if key == "client_version" {
            return Some(value);
        }
    }
    None
}

/// `error` as a JSON reply: its own status when that is a failure, else 502.
fn error_response(error: &PluginError) -> HttpResponse {
    let status = error
        .status()
        .filter(|status| *status >= 400)
        .unwrap_or(502);
    let body = json!({"error": {"message": error.message(), "type": "hofund_error"}});
    json_response(status, body.to_string())
}

/// `body` as a JSON reply.
fn json_response(status: u16, body: String) -> HttpResponse {
    let mut response = text_response(status, body);
    if let Ok(value) = "application/json".parse() {
        response.headers_mut().insert("content-type", value);
    }
    response
}

/// The catalog of `ids`, and how many of them were left out to fit [`MOST`]
/// when `cap` is true.
pub(crate) fn catalog(models: &[ListedModel], cap: bool) -> (String, usize) {
    let lookup = official::Lookup::with_codex_home();
    let entries = models
        .iter()
        .enumerate()
        .map(|(rank, model)| entry(&model.id, rank + 1, &lookup, model.context_window));
    capped_models(entries, cap)
}

/// Serializes catalog entries, optionally enforcing [`MOST`].
pub(crate) fn capped_models(models: impl IntoIterator<Item = Value>, cap: bool) -> (String, usize) {
    let mut out = String::from(HEAD);
    let (mut kept, mut left_out) = (0_usize, 0_usize);
    for model in models {
        let entry = model.to_string();
        if cap && !fits(out.len(), entry.len()) {
            left_out += 1;
            continue;
        }
        if kept > 0 {
            out.push(',');
        }
        out.push_str(&entry);
        kept += 1;
    }
    out.push_str(TAIL);
    (out, left_out)
}

/// Whether an entry `entry` bytes long, after `used` bytes of catalog, a
/// comma, and the tail, stays within [`MOST`].
const fn fits(used: usize, entry: usize) -> bool {
    used + 1 + entry + TAIL.len() <= MOST
}

/// The catalog entry of model `id`, ranked `priority` in the picker. Only
/// fields Codex requires or that change what it does are set.
pub(crate) fn entry(
    id: &str,
    priority: usize,
    lookup: &official::Lookup,
    context_window: Option<u64>,
) -> Value {
    let mut value = json!({
        "slug": id,
        "display_name": id,
        "description": format!("{id} via hofund"),
        "default_reasoning_level": null,
        "supported_reasoning_levels": [],
        "shell_type": "unified_exec",
        "visibility": "list",
        "supported_in_api": true,
        "priority": priority,
        "support_verbosity": false,
        "default_verbosity": null,
        "apply_patch_tool_type": "freeform",
        "truncation_policy": {"mode": "tokens", "limit": 10_000},
        "experimental_supported_tools": [],
        "input_modalities": ["text"],
        "service_tiers": [],
        "supports_search_tool": false,
        "supports_parallel_tool_calls": true,
        "supports_reasoning_summaries": false,
    });
    lookup.for_id(id).apply(&mut value);
    apply_auto_compact(&mut value, context_window);
    value
}

fn apply_auto_compact(entry: &mut Value, known_window: Option<u64>) {
    let object = entry.as_object_mut().expect("catalog entry object");
    if object.contains_key("context_window") {
        return;
    }
    if let Some(window) = known_window {
        // Codex derives its own auto-compact point from a known window
        object.insert("context_window".to_owned(), Value::Number(window.into()));
        return;
    }
    if object
        .get("auto_compact_token_limit")
        .and_then(Value::as_u64)
        .is_some_and(|limit| limit <= DEFAULT_AUTO_COMPACT_TOKEN_LIMIT)
    {
        return;
    }
    object.insert(
        "auto_compact_token_limit".to_owned(),
        Value::Number(DEFAULT_AUTO_COMPACT_TOKEN_LIMIT.into()),
    );
}

/// The ids the gateway's `/v1/models` lists, in its order.
async fn listed_models(kernel: &dyn Kernel) -> Result<Vec<ListedModel>, PluginError> {
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
        .filter_map(|model| {
            let id = model.get("id").and_then(Value::as_str)?;
            let context_window = model.get("context_window").and_then(Value::as_u64);
            Some(ListedModel {
                id: id.to_owned(),
                context_window,
            })
        })
        .collect())
}

/// The ids the gateway's `/v1/models` lists, in its order.
async fn ids(kernel: &dyn Kernel) -> Result<Vec<String>, PluginError> {
    Ok(listed_models(kernel)
        .await?
        .into_iter()
        .map(|model| model.id)
        .collect())
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
mod d47_tests;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod d34_tests;

#[cfg(test)]
mod backend_tests;

#[cfg(test)]
mod input_tests;
