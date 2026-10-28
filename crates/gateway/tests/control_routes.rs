//! Full control route and failure-envelope coverage against a recording fake.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use futures_util::StreamExt;
use hofund_control_proto::{
    API_VERSION, AgentAction, AgentChangeResponse, AgentConnectRequest, AgentDisconnectRequest,
    AgentListResponse, AgentPlanRequest, AgentPlanResponse, AgentStatusResponse, ControlError,
    ControlErrorKind, ErrorEnvelope, HealthResponse, MAX_REQUEST_BODY_BYTES, ModelListResponse,
    PicksResponse, PicksSetRequest, ProviderListResponse, ProviderTestRequest,
    ProviderTestResponse, RouteModelListResponse, ShutdownRequest, ShutdownResponse,
    StatusResponse, TokenRotateRequest, TokenRotateResponse, UsageQuery, UsageResponse,
    decode_error_envelope,
};
use hofund_control_proto::{
    ConfigApplyRequest, ConfigResponse, ConfigValidateRequest, ConfigValidationResponse,
    ReloadRequest, Topic,
};
use hofund_gateway::control::{
    Control, ControlFailure, ControlFuture, ControlState, EventSubscription, EventsQuery, SseFrame,
};
use tower::ServiceExt;

const LISTEN: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 3426);
const HOST: &str = "127.0.0.1:3426";
const TOKEN: &str = "route-test-token";

struct FakeControlState {
    calls: Mutex<Vec<String>>,
    queries: Mutex<Vec<EventsQuery>>,
    draining: AtomicBool,
    failure: Mutex<Option<(ControlErrorKind, Option<u16>)>>,
}

impl Default for FakeControlState {
    fn default() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            queries: Mutex::new(Vec::new()),
            draining: AtomicBool::new(false),
            failure: Mutex::new(None),
        }
    }
}

impl FakeControlState {
    fn reply<T: Default + Send + 'static>(
        &self,
        call: impl Into<String>,
    ) -> ControlFuture<'static, T> {
        self.calls.lock().expect("calls").push(call.into());
        let failure = *self.failure.lock().expect("failure");
        let result = match failure {
            Some((kind, status)) => Err(ControlFailure {
                error: ControlError::new(kind, "injected failure", "test"),
                status,
            }),
            None => Ok(T::default()),
        };
        Box::pin(async move { result })
    }

    fn set_failure(&self, kind: ControlErrorKind, status: Option<u16>) {
        *self.failure.lock().expect("failure") = Some((kind, status));
    }
}

impl ControlState for FakeControlState {
    fn health(&self) -> HealthResponse {
        let mut response = HealthResponse::default();
        response.daemon_version = "test".to_owned();
        response
    }

    fn status(&self) -> StatusResponse {
        StatusResponse::default()
    }

    fn draining(&self) -> bool {
        self.draining.load(Ordering::SeqCst)
    }

    fn providers(&self) -> ControlFuture<'_, ProviderListResponse> {
        self.reply("providers")
    }

    fn provider_models(&self, id: String) -> ControlFuture<'_, ModelListResponse> {
        self.reply(format!("provider_models:{id}"))
    }

    fn refresh_models(&self, id: String) -> ControlFuture<'_, ModelListResponse> {
        self.reply(format!("refresh_models:{id}"))
    }

    fn test_provider(
        &self,
        id: String,
        _request: ProviderTestRequest,
    ) -> ControlFuture<'_, ProviderTestResponse> {
        self.reply(format!("test_provider:{id}"))
    }

    fn picks(&self, id: String) -> ControlFuture<'_, PicksResponse> {
        self.reply(format!("picks:{id}"))
    }

    fn set_picks(&self, id: String, _request: PicksSetRequest) -> ControlFuture<'_, PicksResponse> {
        self.reply(format!("set_picks:{id}"))
    }

    fn route_models(&self) -> ControlFuture<'_, RouteModelListResponse> {
        self.reply("route_models")
    }

    fn agents(&self) -> ControlFuture<'_, AgentListResponse> {
        self.reply("agents")
    }

    fn agent_status(&self) -> ControlFuture<'_, AgentStatusResponse> {
        self.reply("agent_status")
    }

    fn plan_agent(
        &self,
        id: String,
        _request: AgentPlanRequest,
    ) -> ControlFuture<'_, AgentPlanResponse> {
        self.reply(format!("plan_agent:{id}"))
    }

    fn connect_agent(
        &self,
        id: String,
        _request: AgentConnectRequest,
    ) -> ControlFuture<'_, AgentChangeResponse> {
        self.reply(format!("connect_agent:{id}"))
    }

    fn disconnect_agent(
        &self,
        id: String,
        _request: AgentDisconnectRequest,
    ) -> ControlFuture<'_, AgentChangeResponse> {
        self.reply(format!("disconnect_agent:{id}"))
    }

    fn usage(&self, query: UsageQuery) -> ControlFuture<'_, UsageResponse> {
        self.reply(format!("usage:{:?}", query.period))
    }

    fn rotate_token(&self, _request: TokenRotateRequest) -> ControlFuture<'_, TokenRotateResponse> {
        self.reply("rotate_token")
    }

    fn shutdown(&self, _request: ShutdownRequest) -> ControlFuture<'_, ShutdownResponse> {
        self.reply("shutdown")
    }

    fn config(&self) -> ControlFuture<'_, ConfigResponse> {
        self.reply("config")
    }

    fn validate_config(
        &self,
        _request: ConfigValidateRequest,
    ) -> ControlFuture<'_, ConfigValidationResponse> {
        self.reply("config_validate")
    }

    fn apply_config(&self, _request: ConfigApplyRequest) -> ControlFuture<'_, ConfigResponse> {
        self.reply("config_apply")
    }

    fn reload_config(&self, _request: ReloadRequest) -> ControlFuture<'_, ConfigResponse> {
        self.reply("config_reload")
    }

    /// One frame, then an open stream so heartbeats can be observed.
    fn events(&self, query: EventsQuery) -> Result<EventSubscription, ControlFailure> {
        self.calls.lock().expect("calls").push("events".to_owned());
        self.queries.lock().expect("queries").push(query);
        let frame = SseFrame::new("boot:control=1".to_owned(), "snapshot", "{}".into());
        let frames = futures_util::stream::iter([frame]).chain(futures_util::stream::pending());
        Ok(EventSubscription::new(Box::pin(frames)))
    }
}

fn app(state: &Arc<FakeControlState>) -> axum::Router {
    Control::new(state.clone(), TOKEN.to_owned()).into_router(LISTEN)
}

macro_rules! json {
    ($value:expr) => {
        serde_json::to_vec(&$value).expect("request JSON")
    };
}

fn operation_routes() -> Vec<(Method, &'static str, Vec<u8>)> {
    vec![
        (Method::GET, "/v1/hofund/status", Vec::new()),
        (Method::GET, "/v1/hofund/providers", Vec::new()),
        (
            Method::GET,
            "/v1/hofund/providers/provider/models",
            Vec::new(),
        ),
        (
            Method::POST,
            "/v1/hofund/providers/provider/models/refresh",
            Vec::new(),
        ),
        (
            Method::POST,
            "/v1/hofund/providers/provider/test",
            json!(&ProviderTestRequest::new(None)),
        ),
        (
            Method::GET,
            "/v1/hofund/providers/provider/picks",
            Vec::new(),
        ),
        (
            Method::PUT,
            "/v1/hofund/providers/provider/picks",
            json!(&PicksSetRequest::replace(
                "0".repeat(64),
                vec!["model".to_owned()],
            )),
        ),
        (Method::GET, "/v1/hofund/models", Vec::new()),
        (Method::GET, "/v1/hofund/agents", Vec::new()),
        (Method::GET, "/v1/hofund/agents/status", Vec::new()),
        (
            Method::POST,
            "/v1/hofund/agents/agent/plan",
            json!(&AgentPlanRequest::new(AgentAction::Connect, false)),
        ),
        (
            Method::POST,
            "/v1/hofund/agents/agent/connect",
            json!(&AgentConnectRequest::new(None)),
        ),
        (
            Method::POST,
            "/v1/hofund/agents/agent/disconnect",
            json!(&AgentDisconnectRequest::new(false, None)),
        ),
        (Method::GET, "/v1/hofund/usage?period=day", Vec::new()),
        (
            Method::POST,
            "/v1/hofund/token/rotate",
            json!(&TokenRotateRequest::new()),
        ),
        (
            Method::POST,
            "/v1/hofund/shutdown",
            json!(&ShutdownRequest::new()),
        ),
        (Method::GET, "/v1/hofund/config", Vec::new()),
        (
            Method::POST,
            "/v1/hofund/config/validate",
            json!(&ConfigValidateRequest::new("listen = \"127.0.0.1:0\"\n")),
        ),
        (
            Method::POST,
            "/v1/hofund/config/apply",
            json!(&ConfigApplyRequest::new(
                "0".repeat(64),
                "listen = \"127.0.0.1:0\"\n"
            )),
        ),
        (
            Method::POST,
            "/v1/hofund/config/reload",
            json!(&ReloadRequest::new("0".repeat(64))),
        ),
    ]
}

async fn call(
    app: axum::Router,
    method: Method,
    path: &str,
    body: Vec<u8>,
    authenticate: bool,
    json_content_type: bool,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, HOST);
    if authenticate {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {TOKEN}"));
    }
    if json_content_type {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
    }
    let response = app
        .oneshot(builder.body(Body::from(body)).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body")
        .to_vec();
    (status, headers, body)
}

fn assert_no_store(headers: &HeaderMap) {
    assert_eq!(
        headers.get(header::CACHE_CONTROL).expect("cache control"),
        "no-store"
    );
}

fn assert_error(body: &[u8], kind: ControlErrorKind) {
    let envelope: ErrorEnvelope = decode_error_envelope(API_VERSION, body).expect("error envelope");
    assert_eq!(envelope.error.kind, kind);
}

#[tokio::test]
async fn control_routes_success_and_error_envelopes_against_fake_state() {
    let state = Arc::new(FakeControlState::default());
    let (status, headers, body) = call(
        app(&state),
        Method::GET,
        "/v1/hofund/health",
        Vec::new(),
        false,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_no_store(&headers);
    let health: HealthResponse = serde_json::from_slice(&body).expect("health");
    assert_eq!(health.daemon_version, "test");

    for (method, path, body) in operation_routes() {
        let is_get = method == Method::GET;
        let (status, headers, response) =
            call(app(&state), method, path, body, true, !is_get).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_no_store(&headers);
        let value: serde_json::Value = serde_json::from_slice(&response).expect("response JSON");
        assert_eq!(
            value.get("version").and_then(|value| value.as_u64()),
            Some(1)
        );
    }
    assert_eq!(
        state.calls.lock().expect("calls").as_slice(),
        [
            "providers",
            "provider_models:provider",
            "refresh_models:provider",
            "test_provider:provider",
            "picks:provider",
            "set_picks:provider",
            "route_models",
            "agents",
            "agent_status",
            "plan_agent:agent",
            "connect_agent:agent",
            "disconnect_agent:agent",
            "usage:Day",
            "rotate_token",
            "shutdown",
            "config",
            "config_validate",
            "config_apply",
            "config_reload",
        ],
        "each control route must dispatch to its own handler"
    );

    state.set_failure(ControlErrorKind::Internal, None);
    for (method, path, body) in operation_routes()
        .into_iter()
        .filter(|(_, path, _)| *path != "/v1/hofund/status")
    {
        let is_get = method == Method::GET;
        let (status, headers, response) =
            call(app(&state), method, path, body, true, !is_get).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{path}");
        assert_no_store(&headers);
        assert_error(&response, ControlErrorKind::Internal);
    }

    let (status, headers, body) = call(
        app(&state),
        Method::GET,
        "/v1/hofund/not-a-route",
        Vec::new(),
        true,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_no_store(&headers);
    assert_error(&body, ControlErrorKind::NotFound);

    let (status, headers, body) = call(
        app(&state),
        Method::POST,
        "/v1/hofund/providers",
        Vec::new(),
        true,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_no_store(&headers);
    assert_error(&body, ControlErrorKind::MethodNotAllowed);
}

#[tokio::test]
async fn control_auth_precedes_route_resolution_for_new_routes() {
    let state = Arc::new(FakeControlState::default());
    for (method, path, body) in operation_routes() {
        let is_get = method == Method::GET;
        let (status, _, response) = call(app(&state), method, path, body, false, !is_get).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}");
        assert_error(&response, ControlErrorKind::Unauthorized);
    }
}

#[tokio::test]
async fn mutation_without_json_content_type_rejected_on_every_post_put() {
    let state = Arc::new(FakeControlState::default());
    for (method, path, body) in operation_routes()
        .into_iter()
        .filter(|(method, _, _)| method != Method::GET)
    {
        let (status, headers, response) = call(app(&state), method, path, body, true, false).await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{path}");
        assert_no_store(&headers);
        assert_error(&response, ControlErrorKind::BadRequest);
    }
}

#[tokio::test]
async fn refresh_rejects_nonempty_body() {
    let state = Arc::new(FakeControlState::default());
    let (status, _, body) = call(
        app(&state),
        Method::POST,
        "/v1/hofund/providers/provider/models/refresh",
        b"{}".to_vec(),
        true,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_error(&body, ControlErrorKind::BadRequest);
    assert!(state.calls.lock().expect("calls").is_empty());
}

#[tokio::test]
async fn upstream_429_override_only_for_upstream_kind() {
    let state = Arc::new(FakeControlState::default());
    for (kind, override_status, expected) in [
        (
            ControlErrorKind::Upstream,
            Some(429),
            StatusCode::TOO_MANY_REQUESTS,
        ),
        (
            ControlErrorKind::Upstream,
            Some(503),
            StatusCode::BAD_GATEWAY,
        ),
        (
            ControlErrorKind::Unavailable,
            Some(429),
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        (
            ControlErrorKind::Internal,
            Some(429),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    ] {
        state.set_failure(kind, override_status);
        let (status, _, body) = call(
            app(&state),
            Method::GET,
            "/v1/hofund/providers",
            Vec::new(),
            true,
            false,
        )
        .await;
        assert_eq!(status, expected);
        assert_error(&body, kind);
    }
}

#[tokio::test]
async fn draining_rejects_mutations_except_shutdown() {
    let state = Arc::new(FakeControlState::default());
    state.draining.store(true, Ordering::SeqCst);
    let (status, _, _) = call(
        app(&state),
        Method::GET,
        "/v1/hofund/providers",
        Vec::new(),
        true,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    for (method, path, body) in operation_routes()
        .into_iter()
        .filter(|(method, path, _)| method != Method::GET && *path != "/v1/hofund/shutdown")
    {
        let (status, _, response) = call(app(&state), method, path, body, true, true).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{path}");
        assert_error(&response, ControlErrorKind::Unavailable);
    }
    let (status, _, _) = call(
        app(&state),
        Method::POST,
        "/v1/hofund/not-a-route",
        Vec::new(),
        true,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    let (status, _, _) = call(
        app(&state),
        Method::POST,
        "/v1/hofund/shutdown",
        json!(&ShutdownRequest::new()),
        true,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn no_token_show_route_exists() {
    let state = Arc::new(FakeControlState::default());
    for method in [Method::GET, Method::POST] {
        for path in ["/v1/hofund/token", "/v1/hofund/token/show"] {
            let is_get = method == Method::GET;
            let (status, _, body) =
                call(app(&state), method.clone(), path, Vec::new(), true, !is_get).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}");
            assert_error(&body, ControlErrorKind::NotFound);
            let (status, _, body) = call(
                app(&state),
                method.clone(),
                path,
                Vec::new(),
                false,
                !is_get,
            )
            .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {path}");
            assert_error(&body, ControlErrorKind::Unauthorized);
        }
    }
}

#[tokio::test]
async fn path_ids_are_percent_decoded_and_bounded() {
    let state = Arc::new(FakeControlState::default());
    let (status, _, _) = call(
        app(&state),
        Method::GET,
        "/v1/hofund/providers/a%2Fb%20c%25%C3%A9/models",
        Vec::new(),
        true,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        state.calls.lock().expect("calls").as_slice(),
        ["provider_models:a/b c%é"]
    );

    let long = "x".repeat(257);
    let (status, _, body) = call(
        app(&state),
        Method::GET,
        &format!("/v1/hofund/providers/{long}/models"),
        Vec::new(),
        true,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_error(&body, ControlErrorKind::BadRequest);
    assert_eq!(state.calls.lock().expect("calls").len(), 1);
}

#[tokio::test]
async fn query_and_body_rejections_use_control_envelopes() {
    let state = Arc::new(FakeControlState::default());
    for path in [
        "/v1/hofund/usage?period=future",
        "/v1/hofund/usage?unknown=value",
    ] {
        let (status, _, body) = call(app(&state), Method::GET, path, Vec::new(), true, false).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_error(&body, ControlErrorKind::BadRequest);
    }
    let (status, _, _) = call(
        app(&state),
        Method::GET,
        "/v1/hofund/usage",
        Vec::new(),
        true,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        state
            .calls
            .lock()
            .expect("calls")
            .contains(&"usage:All".to_owned())
    );

    for body in [b"{".to_vec(), br#"{"version":2}"#.to_vec()] {
        let (status, _, response) = call(
            app(&state),
            Method::POST,
            "/v1/hofund/shutdown",
            body,
            true,
            true,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let envelope: ErrorEnvelope =
            decode_error_envelope(API_VERSION, &response).expect("error envelope");
        assert!(matches!(
            envelope.error.kind,
            ControlErrorKind::BadRequest | ControlErrorKind::UnsupportedVersion
        ));
    }
}

#[tokio::test]
async fn oversize_body_returns_413_with_no_store() {
    let state = Arc::new(FakeControlState::default());
    let (status, headers, body) = call(
        app(&state),
        Method::POST,
        "/v1/hofund/shutdown",
        vec![b' '; MAX_REQUEST_BODY_BYTES + 1],
        true,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_no_store(&headers);
    assert_error(&body, ControlErrorKind::BadRequest);
    assert!(state.calls.lock().expect("calls").is_empty());
}

#[tokio::test]
async fn body_above_extractor_default_within_protocol_limit_reaches_handler() {
    const PADDED_BYTES: usize = 3 * 1024 * 1024;
    const EXTRACTOR_DEFAULT_BYTES: usize = 2 * 1024 * 1024;
    let state = Arc::new(FakeControlState::default());
    let mut body = br#"{"version":1}"#.to_vec();
    body.resize(PADDED_BYTES, b' ');
    assert!(body.len() > EXTRACTOR_DEFAULT_BYTES);
    assert!(body.len() < MAX_REQUEST_BODY_BYTES);
    let (status, headers, response) = call(
        app(&state),
        Method::POST,
        "/v1/hofund/shutdown",
        body,
        true,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_no_store(&headers);
    let value: serde_json::Value = serde_json::from_slice(&response).expect("response JSON");
    assert_eq!(
        value.get("version").and_then(|value| value.as_u64()),
        Some(1)
    );
    assert_eq!(state.calls.lock().expect("calls").as_slice(), ["shutdown"]);
}

async fn open_events(
    app: axum::Router,
    path: &str,
    last_event_id: Option<&str>,
) -> axum::response::Response {
    let mut builder = Request::builder()
        .method(Method::GET)
        .uri(path)
        .header(header::HOST, HOST)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"));
    if let Some(id) = last_event_id {
        builder = builder.header("last-event-id", id);
    }
    app.oneshot(builder.body(Body::empty()).expect("request"))
        .await
        .expect("response")
}

#[tokio::test]
async fn events_query_cursor_is_rejected() {
    let state = Arc::new(FakeControlState::default());
    for path in [
        "/v1/hofund/events?last_event_id=boot:control=1",
        "/v1/hofund/events?lastEventId=boot:control=1",
        "/v1/hofund/events?topics=control&last_event_id=boot:control=1",
        "/v1/hofund/events?topics=bogus",
        "/v1/hofund/events?topics=control&topics=requests",
        "/v1/hofund/events?since=1",
    ] {
        let response = open_events(app(&state), path, None).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path}");
        assert_no_store(response.headers());
    }
    assert!(
        state.calls.lock().expect("calls").is_empty(),
        "a rejected query must not open a subscription"
    );
}

#[tokio::test]
async fn events_default_to_control_and_forward_the_last_event_id_header() {
    let state = Arc::new(FakeControlState::default());
    let response = open_events(app(&state), "/v1/hofund/events", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_no_store(response.headers());
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .expect("content type"),
        "text/event-stream"
    );
    let mut body = response.into_body().into_data_stream();
    let first = tokio::time::timeout(std::time::Duration::from_secs(5), body.next())
        .await
        .expect("first frame")
        .expect("frame")
        .expect("bytes");
    let text = String::from_utf8(first.to_vec()).expect("utf-8");
    assert!(text.contains("id: boot:control=1\n"), "{text}");
    assert!(text.contains("event: snapshot\n"), "{text}");
    assert!(text.contains("data: {}\n"), "{text}");
    drop(body);

    let response = open_events(
        app(&state),
        "/v1/hofund/events?topics=requests,control",
        Some("boot:control=1,requests=2"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let queries = state.queries.lock().expect("queries");
    assert_eq!(queries[0].topics, [Topic::Control]);
    assert_eq!(queries[0].last_event_id, None);
    assert_eq!(queries[1].topics, [Topic::Control, Topic::Requests]);
    assert_eq!(
        queries[1].last_event_id.as_deref(),
        Some("boot:control=1,requests=2")
    );
}

#[tokio::test]
async fn sse_heartbeat_rereads_token_and_closes_on_mismatch() {
    let dir = tempfile::tempdir().expect("temp dir");
    let token_file = dir.path().join("hofund.token");
    std::fs::write(&token_file, TOKEN).expect("token file");
    let state = Arc::new(FakeControlState::default());
    let router = Control::with_token_file(state, TOKEN.to_owned(), token_file.clone())
        .with_heartbeat(std::time::Duration::from_millis(20))
        .into_router(LISTEN);
    let response = open_events(router, "/v1/hofund/events", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body().into_data_stream();
    assert!(
        next_text(&mut body)
            .await
            .expect("snapshot")
            .contains("event: snapshot")
    );
    // The token still matches, so heartbeats keep the stream open.
    assert_eq!(next_text(&mut body).await.expect("heartbeat"), ": \n\n");
    assert_eq!(next_text(&mut body).await.expect("heartbeat"), ": \n\n");
    std::fs::write(&token_file, "rotated-token").expect("rotate token file");
    let mut ended = false;
    for _ in 0..3 {
        if next_text(&mut body).await.is_none() {
            ended = true;
            break;
        }
    }
    assert!(
        ended,
        "the stream must end at the first heartbeat after rotation"
    );
}

async fn next_text(body: &mut axum::body::BodyDataStream) -> Option<String> {
    tokio::time::timeout(std::time::Duration::from_secs(5), body.next())
        .await
        .expect("stream progresses")
        .map(|chunk| String::from_utf8(chunk.expect("bytes").to_vec()).expect("utf-8"))
}
