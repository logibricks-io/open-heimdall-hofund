//! HTTP edge of the local gateway.
//!
//! The process authenticates a request, decodes it with the codec it was
//! given for that path, asks the router, and writes the encoded body. Vendor
//! names do not belong in this crate. Generative Language paths are matched
//! in [`endpoints`].

mod endpoints;

pub use endpoints::{GENERATIVE_LIST, GENERATIVE_STREAM, GENERATIVE_UNARY};

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};

use axum::response::Response;
use axum::routing::{get, post};
use endpoints::{GENERATIVE_CATCHALL, inject_target, match_count, match_generate, query_key};
use futures_util::{Stream, StreamExt};
use hofund_codec::{Codec, StreamEncoder};
use hofund_ir::{Request as ModelRequest, StreamEvent};
use hofund_router::{Completion, ProviderError, ProviderErrorKind, Router, RouterError};
use hofund_usage::{Ledger, Row};
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

const BODY_LIMIT: usize = 8 * 1024 * 1024;

/// The HTTP server and the state it shares across requests.
pub struct Gateway {
    inner: Arc<Inner>,
}

struct Inner {
    router: Router,
    /// Last token read from [`Self::token_file`], or the token passed to
    /// [`Gateway::new`] when that file cannot be read.
    token: Mutex<String>,
    token_file: Option<PathBuf>,
    codecs: HashMap<String, Arc<dyn Codec>>,
    usage: Arc<Ledger>,
}

/// A bound server.
pub struct Running {
    /// The address the listener accepted.
    pub local_addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

/// The listener could not be bound.
#[derive(Debug, Error)]
pub enum GatewayError {
    /// Binding the address failed.
    #[error("cannot bind {listen}: {source}")]
    Bind {
        /// The address that was requested.
        listen: SocketAddr,
        /// The operating-system error.
        source: std::io::Error,
    },
}

impl Gateway {
    /// A gateway that routes with `router`, requires `token`, and decodes each
    /// path with the codec in `codecs`.
    #[must_use]
    pub fn new(
        router: Router,
        token: String,
        codecs: HashMap<String, Arc<dyn Codec>>,
        usage: Arc<Ledger>,
    ) -> Self {
        Self::build(router, token, None, codecs, usage)
    }

    /// Like [`Self::new`], and re-reads `token_file` on each request.
    ///
    /// A failed or empty read keeps the last token that was read. The file is
    /// replaced by rename, so a reader sees the previous token or the new one.
    #[must_use]
    pub fn with_token_file(
        router: Router,
        token: String,
        token_file: PathBuf,
        codecs: HashMap<String, Arc<dyn Codec>>,
        usage: Arc<Ledger>,
    ) -> Self {
        Self::build(router, token, Some(token_file), codecs, usage)
    }

    fn build(
        router: Router,
        token: String,
        token_file: Option<PathBuf>,
        codecs: HashMap<String, Arc<dyn Codec>>,
        usage: Arc<Ledger>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                router,
                token: Mutex::new(token),
                token_file,
                codecs,
                usage,
            }),
        }
    }

    /// The axum router. `/health` is open. Every other route requires a token.
    ///
    /// Codec paths that contain `{` are map keys, not routable patterns. When
    /// a Generative Language codec is registered, the process binds the
    /// catch-all generate route and the model list instead.
    pub fn into_router(self) -> axum::Router {
        let mut routes = axum::Router::new()
            .route("/health", get(health))
            .route("/v1/models", get(models));
        let generative = self.inner.codecs.contains_key(GENERATIVE_UNARY)
            || self.inner.codecs.contains_key(GENERATIVE_STREAM);
        let paths: Vec<String> = self.inner.codecs.keys().cloned().collect();
        let messages = paths.iter().any(|path| path == "/v1/messages");
        for path in paths {
            if path.contains('{') {
                continue;
            }
            routes = routes.route(&path, post(complete));
        }
        if messages {
            routes = routes.route("/v1/messages/count_tokens", post(complete));
        }
        if generative {
            routes = routes
                .route(GENERATIVE_CATCHALL, post(complete))
                .route(GENERATIVE_LIST, get(generative_models));
        }
        routes.with_state(Arc::clone(&self.inner))
    }

    /// Binds `listen` and serves until [`Running`] shuts down.
    ///
    /// # Errors
    ///
    /// Returns [`GatewayError`] when the address cannot be bound.
    pub async fn bind(self, listen: SocketAddr) -> Result<Running, GatewayError> {
        let listener = TcpListener::bind(listen)
            .await
            .map_err(|source| GatewayError::Bind { listen, source })?;
        let local_addr = listener
            .local_addr()
            .map_err(|source| GatewayError::Bind { listen, source })?;
        let router = self.into_router();
        let (shutdown, signal) = oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let _ = signal.await;
                })
                .await
        });
        Ok(Running {
            local_addr,
            shutdown: Some(shutdown),
            task: Some(task),
        })
    }
}

impl Running {
    /// Asks the server to stop and waits for it.
    pub async fn shutdown(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

async fn health() -> Response {
    json_response(StatusCode::OK, br#"{"status":"ok"}"#.to_vec())
}

async fn models(State(app): State<Arc<Inner>>, request: Request) -> Response {
    if let Some(response) = reject(&current_token(&app), request.headers(), request.uri(), None) {
        return response;
    }
    let data: Vec<_> = app
        .router
        .models()
        .into_iter()
        .map(|id| serde_json::json!({"id": id, "object": "model", "owned_by": "hofund"}))
        .collect();
    json_response(
        StatusCode::OK,
        serde_json::to_vec(&serde_json::json!({"object": "list", "data": data}))
            .unwrap_or_else(|_| b"{\"object\":\"list\",\"data\":[]}".to_vec()),
    )
}

async fn generative_models(State(app): State<Arc<Inner>>, request: Request) -> Response {
    let codec = app
        .codecs
        .get(GENERATIVE_UNARY)
        .or_else(|| app.codecs.get(GENERATIVE_STREAM))
        .map(AsRef::as_ref);
    if let Some(response) = reject(
        &current_token(&app),
        request.headers(),
        request.uri(),
        codec,
    ) {
        return response;
    }
    let models: Vec<_> = app
        .router
        .models()
        .into_iter()
        .map(|id| {
            serde_json::json!({
                "name": format!("models/{id}"),
                "displayName": id,
                "supportedGenerationMethods": ["generateContent", "streamGenerateContent"],
            })
        })
        .collect();
    json_response(
        StatusCode::OK,
        serde_json::to_vec(&serde_json::json!({"models": models}))
            .unwrap_or_else(|_| b"{\"models\":[]}".to_vec()),
    )
}

async fn complete(State(app): State<Arc<Inner>>, request: Request) -> Response {
    let path = request.uri().path().to_owned();
    let uri = request.uri().clone();
    let headers = request.headers().clone();
    if path == "/v1/messages/count_tokens" {
        return count_tokens(app, headers, uri, request, "/v1/messages", None).await;
    }
    if let Some(model) = match_count(&path) {
        return count_tokens(app, headers, uri, request, GENERATIVE_UNARY, Some(model)).await;
    }
    let matched = match_generate(&path);
    let codec_key = matched
        .as_ref()
        .map(|route| route.key)
        .unwrap_or(path.as_str());
    let Some(codec) = app.codecs.get(codec_key).cloned() else {
        return text_response(StatusCode::NOT_FOUND, "not found");
    };
    if let Some(response) = reject(&current_token(&app), &headers, &uri, Some(codec.as_ref())) {
        return response;
    }
    let started = Instant::now();
    let bytes = match axum::body::to_bytes(request.into_body(), BODY_LIMIT).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return coded(
                codec.as_ref(),
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body is too large",
            );
        }
    };
    let bytes = if let Some(route) = &matched {
        Bytes::from(inject_target(&bytes, &route.model, route.stream))
    } else {
        bytes
    };
    let model_request = match codec.decode_request(&bytes) {
        Ok(model_request) => model_request,
        Err(error) => {
            record(
                &app.usage,
                &ModelRequest::new(model_name(&bytes)),
                "",
                None,
                started,
                StatusCode::BAD_REQUEST,
            );
            return coded(codec.as_ref(), StatusCode::BAD_REQUEST, &error.to_string());
        }
    };
    match app.router.dispatch(model_request.clone(), codec.id()).await {
        Ok(routed) => {
            success(
                app,
                codec,
                model_request,
                routed.upstream_id,
                routed.completion,
                started,
            )
            .await
        }
        Err(error) => failure(app, codec.as_ref(), &model_request, error, started),
    }
}

async fn count_tokens(
    app: Arc<Inner>,
    headers: HeaderMap,
    uri: Uri,
    request: Request,
    codec_key: &str,
    path_model: Option<String>,
) -> Response {
    let Some(codec) = app.codecs.get(codec_key).cloned() else {
        return text_response(StatusCode::NOT_FOUND, "not found");
    };
    if let Some(response) = reject(&current_token(&app), &headers, &uri, Some(codec.as_ref())) {
        return response;
    }
    let bytes = match axum::body::to_bytes(request.into_body(), BODY_LIMIT).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return coded(
                codec.as_ref(),
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body is too large",
            );
        }
    };
    let bytes = if let Some(model) = &path_model {
        Bytes::from(inject_target(&bytes, model, false))
    } else {
        bytes
    };
    let model_request = match codec.decode_request(&bytes) {
        Ok(model_request) => model_request,
        Err(error) => return coded(codec.as_ref(), StatusCode::BAD_REQUEST, &error.to_string()),
    };
    match app.router.count(model_request, codec.id()).await {
        Ok(count) => json_response(
            StatusCode::OK,
            codec.encode_token_count(count.tokens, count.estimated),
        ),
        Err(error) => {
            let (status, message) = match error {
                RouterError::NoRoute { .. } => (StatusCode::NOT_FOUND, error.to_string()),
                RouterError::Ambiguous { .. } => (StatusCode::BAD_REQUEST, error.to_string()),
                RouterError::Upstream { error, .. } => (status_of(&error), error.message),
            };
            coded(codec.as_ref(), status, &message)
        }
    }
}

fn reject(
    expected: &str,
    headers: &HeaderMap,
    uri: &Uri,
    codec: Option<&dyn Codec>,
) -> Option<Response> {
    if authorized(expected, headers, uri) {
        return None;
    }
    let body = match codec {
        Some(codec) => codec.encode_error(StatusCode::UNAUTHORIZED.as_u16(), "unauthorized"),
        None => list_error("unauthorized"),
    };
    Some(json_response(StatusCode::UNAUTHORIZED, body))
}

fn current_token(app: &Inner) -> String {
    let mut known = app
        .token
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(path) = &app.token_file
        && let Ok(text) = std::fs::read_to_string(path)
    {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            *known = trimmed.to_owned();
        }
    }
    known.clone()
}

/// Accepts the bearer token, `x-api-key`, `x-goog-api-key`, or a `key` query value.
///
/// Each presented secret is compared in constant time. One match is enough.
/// A request with no secret is still rejected.
fn authorized(expected: &str, headers: &HeaderMap, uri: &Uri) -> bool {
    let mut saw = false;
    let mut matched = false;
    if let Some(token) = bearer(headers) {
        saw = true;
        matched |= tokens_match(expected, token);
    }
    if let Some(token) = header_secret(headers, "x-api-key") {
        saw = true;
        matched |= tokens_match(expected, token);
    }
    if let Some(token) = header_secret(headers, "x-goog-api-key") {
        saw = true;
        matched |= tokens_match(expected, token);
    }
    if let Some(query) = uri.query()
        && let Some(token) = query_key(query)
    {
        saw = true;
        matched |= tokens_match(expected, &token);
    }
    saw && matched
}

fn header_secret<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let value = headers.get(name)?.to_str().ok()?;
    let token = value.trim();
    if token.is_empty() { None } else { Some(token) }
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    value.trim().strip_prefix("Bearer ")
}

fn tokens_match(expected: &str, presented: &str) -> bool {
    let expected = expected.as_bytes();
    let presented = presented.as_bytes();
    let mut diff = expected.len() ^ presented.len();
    let width = expected.len().max(presented.len());
    for index in 0..width {
        let left = expected.get(index).copied().unwrap_or(0);
        let right = presented.get(index).copied().unwrap_or(0);
        diff |= usize::from(left ^ right);
    }
    diff == 0
}

async fn success(
    app: Arc<Inner>,
    codec: Arc<dyn Codec>,
    request: ModelRequest,
    upstream: String,
    completion: Completion,
    started: Instant,
) -> Response {
    match completion {
        Completion::Unary(response) => {
            let input = response
                .usage
                .as_ref()
                .and_then(|usage| as_i64(usage.input_tokens));
            let output = response
                .usage
                .as_ref()
                .and_then(|usage| as_i64(usage.output_tokens));
            record(
                &app.usage,
                &request,
                &upstream,
                Some((input, output)),
                started,
                StatusCode::OK,
            );
            match codec.encode_response_for(&request, &response) {
                Ok(bytes) => json_response(StatusCode::OK, bytes),
                Err(error) => coded(codec.as_ref(), StatusCode::BAD_GATEWAY, &error.to_string()),
            }
        }
        Completion::Stream(events) => {
            let encoder = codec.stream_encoder_for(&request);
            let body = sse_body(
                events,
                encoder,
                RecordOnEnd {
                    usage: Arc::clone(&app.usage),
                    request,
                    upstream,
                    started,
                    input: None,
                    output: None,
                },
            );
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/event-stream")
                .body(Body::from_stream(body))
                .unwrap_or_else(|_| text_response(StatusCode::INTERNAL_SERVER_ERROR, "stream"))
        }
    }
}

fn failure(
    app: Arc<Inner>,
    codec: &dyn Codec,
    request: &ModelRequest,
    error: RouterError,
    started: Instant,
) -> Response {
    let (status, upstream, message) = match error {
        RouterError::NoRoute { .. } => (StatusCode::NOT_FOUND, String::new(), error.to_string()),
        RouterError::Ambiguous { .. } => {
            (StatusCode::BAD_REQUEST, String::new(), error.to_string())
        }
        RouterError::Upstream { upstream_id, error } => {
            (status_of(&error), upstream_id, error.message)
        }
    };
    record(&app.usage, request, &upstream, None, started, status);
    coded(codec, status, &message)
}

fn status_of(error: &ProviderError) -> StatusCode {
    let code = match error.kind {
        ProviderErrorKind::Status => error.status.unwrap_or(502),
        ProviderErrorKind::Connect | ProviderErrorKind::Timeout | ProviderErrorKind::Decode => 502,
    };
    StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_GATEWAY)
}

struct RecordOnEnd {
    usage: Arc<Ledger>,
    request: ModelRequest,
    upstream: String,
    started: Instant,
    input: Option<i64>,
    output: Option<i64>,
}

fn sse_body(
    incoming: Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
    encoder: Box<dyn StreamEncoder>,
    record: RecordOnEnd,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send {
    futures_util::stream::unfold(
        (incoming, encoder, record, false),
        |(mut incoming, mut encoder, mut record, flushed)| async move {
            if flushed {
                finish_record(&record, StatusCode::OK);
                return None;
            }
            loop {
                match incoming.next().await {
                    None => match encoder.finish() {
                        Ok(bytes) if bytes.is_empty() => {
                            finish_record(&record, StatusCode::OK);
                            return None;
                        }
                        Ok(bytes) => {
                            return Some((
                                Ok(Bytes::from(bytes)),
                                (incoming, encoder, record, true),
                            ));
                        }
                        Err(_) => {
                            finish_record(&record, StatusCode::BAD_GATEWAY);
                            return None;
                        }
                    },
                    Some(Err(_)) => {
                        finish_record(&record, StatusCode::BAD_GATEWAY);
                        return None;
                    }
                    Some(Ok(event)) => {
                        note_usage(&event, &mut record.input, &mut record.output);
                        match encoder.push(&event) {
                            Ok(bytes) if bytes.is_empty() => {}
                            Ok(bytes) => {
                                return Some((
                                    Ok(Bytes::from(bytes)),
                                    (incoming, encoder, record, false),
                                ));
                            }
                            Err(_) => {
                                finish_record(&record, StatusCode::BAD_GATEWAY);
                                return None;
                            }
                        }
                    }
                }
            }
        },
    )
}

fn note_usage(event: &StreamEvent, input: &mut Option<i64>, output: &mut Option<i64>) {
    let usage = match event {
        StreamEvent::MessageStart(start) => start.usage.as_ref(),
        StreamEvent::MessageDelta(delta) => delta.usage.as_ref(),
        _ => None,
    };
    let Some(usage) = usage else {
        return;
    };
    if let Some(value) = as_i64(usage.input_tokens) {
        *input = Some(value);
    }
    if let Some(value) = as_i64(usage.output_tokens) {
        *output = Some(value);
    }
}

fn finish_record(record: &RecordOnEnd, status: StatusCode) {
    record_row(
        &record.usage,
        &record.request,
        &record.upstream,
        Some((record.input, record.output)),
        record.started,
        status,
    );
}

fn record(
    usage: &Ledger,
    request: &ModelRequest,
    upstream: &str,
    tokens: Option<(Option<i64>, Option<i64>)>,
    started: Instant,
    status: StatusCode,
) {
    record_row(usage, request, upstream, tokens, started, status);
}

fn record_row(
    usage: &Ledger,
    request: &ModelRequest,
    upstream: &str,
    tokens: Option<(Option<i64>, Option<i64>)>,
    started: Instant,
    status: StatusCode,
) {
    let (input_tokens, output_tokens) = tokens.unwrap_or((None, None));
    let row = Row {
        created_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| {
                i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
            }),
        model: request.model.clone(),
        upstream: upstream.to_owned(),
        input_tokens,
        output_tokens,
        latency_ms: i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX),
        status: status.as_u16().to_string(),
    };
    let _ = usage.record(&row);
}

fn as_i64(value: Option<u64>) -> Option<i64> {
    value.and_then(|value| i64::try_from(value).ok())
}

fn model_name(bytes: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .and_then(|value| {
            value
                .get("model")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default()
}

fn coded(codec: &dyn Codec, status: StatusCode, message: &str) -> Response {
    json_response(status, codec.encode_error(status.as_u16(), message))
}

fn list_error(message: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "error": {"message": message, "type": "authentication_error", "param": null, "code": null}
    }))
    .unwrap_or_else(|_| b"{\"error\":{\"message\":\"unauthorized\"}}".to_vec())
}

fn json_response(status: StatusCode, bytes: Vec<u8>) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(bytes))
        .unwrap_or_else(|_| text_response(StatusCode::INTERNAL_SERVER_ERROR, "response"))
}

fn text_response(status: StatusCode, message: &str) -> Response {
    Response::builder()
        .status(status)
        .body(Body::from(message.to_owned()))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}
