//! R-7a control routes: presets, provider writes, groups, profile rename, status fields.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use hofund_control_proto::{
    API_VERSION, ControlErrorKind, ProviderListResponse, decode_error_envelope,
};
use hofund_gateway::control::{Control, ControlFailure, ControlFuture, ControlState};
use tower::ServiceExt;

const LISTEN: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 3427);
const HOST: &str = "127.0.0.1:3427";
const TOKEN: &str = "r7a-test-token";

struct R7aFake {
    calls: Mutex<Vec<String>>,
    provider_summary: Mutex<Option<hofund_control_proto::ProviderSummary>>,
}

impl R7aFake {
    fn reply<T: Default + Send + 'static>(
        &self,
        call: impl Into<String>,
    ) -> ControlFuture<'static, T> {
        self.calls.lock().expect("calls").push(call.into());
        Box::pin(async move { Ok(T::default()) })
    }
}

impl ControlState for R7aFake {
    fn health(&self) -> hofund_control_proto::HealthResponse {
        hofund_control_proto::HealthResponse::default()
    }

    fn status(&self) -> hofund_control_proto::StatusResponse {
        let mut status = hofund_control_proto::StatusResponse::default();
        status.serves = Some(hofund_control_proto::ServeRole::Serve);
        status.owned_by_pid = 42;
        status
    }

    fn draining(&self) -> bool {
        false
    }

    fn providers(&self) -> ControlFuture<'_, hofund_control_proto::ProviderListResponse> {
        let summary = self
            .provider_summary
            .lock()
            .expect("summary")
            .clone()
            .unwrap_or_default();
        Box::pin(async move {
            let mut response = ProviderListResponse::default();
            response.providers.push(summary);
            Ok(response)
        })
    }

    fn presets(&self) -> ControlFuture<'_, hofund_control_proto::PresetListResponse> {
        self.reply("presets")
    }

    fn patch_provider(
        &self,
        id: String,
        _request: hofund_control_proto::ProviderPatchRequest,
    ) -> ControlFuture<'_, hofund_control_proto::ProviderWriteResponse> {
        self.calls
            .lock()
            .expect("calls")
            .push(format!("patch_provider:{id}"));
        Box::pin(async move { Ok(hofund_control_proto::ProviderWriteResponse::default()) })
    }

    fn rename_profile(
        &self,
        _request: hofund_control_proto::ProfileRenameRequest,
    ) -> ControlFuture<'_, hofund_control_proto::ProfileListResponse> {
        self.reply("rename_profile")
    }

    fn provider_models(
        &self,
        id: String,
    ) -> ControlFuture<'_, hofund_control_proto::ModelListResponse> {
        self.reply(format!("provider_models:{id}"))
    }

    fn refresh_models(
        &self,
        id: String,
    ) -> ControlFuture<'_, hofund_control_proto::ModelListResponse> {
        self.reply(format!("refresh_models:{id}"))
    }

    fn test_provider(
        &self,
        id: String,
        _request: hofund_control_proto::ProviderTestRequest,
    ) -> ControlFuture<'_, hofund_control_proto::ProviderTestResponse> {
        self.reply(format!("test_provider:{id}"))
    }

    fn picks(&self, id: String) -> ControlFuture<'_, hofund_control_proto::PicksResponse> {
        self.reply(format!("picks:{id}"))
    }

    fn set_picks(
        &self,
        id: String,
        _request: hofund_control_proto::PicksSetRequest,
    ) -> ControlFuture<'_, hofund_control_proto::PicksResponse> {
        self.reply(format!("set_picks:{id}"))
    }

    fn route_models(&self) -> ControlFuture<'_, hofund_control_proto::RouteModelListResponse> {
        self.reply("route_models")
    }

    fn agents(&self) -> ControlFuture<'_, hofund_control_proto::AgentListResponse> {
        self.reply("agents")
    }

    fn agent_status(&self) -> ControlFuture<'_, hofund_control_proto::AgentStatusResponse> {
        self.reply("agent_status")
    }

    fn plan_agent(
        &self,
        id: String,
        _request: hofund_control_proto::AgentPlanRequest,
    ) -> ControlFuture<'_, hofund_control_proto::AgentPlanResponse> {
        self.reply(format!("plan_agent:{id}"))
    }

    fn connect_agent(
        &self,
        id: String,
        _request: hofund_control_proto::AgentConnectRequest,
    ) -> ControlFuture<'_, hofund_control_proto::AgentChangeResponse> {
        self.reply(format!("connect_agent:{id}"))
    }

    fn disconnect_agent(
        &self,
        id: String,
        _request: hofund_control_proto::AgentDisconnectRequest,
    ) -> ControlFuture<'_, hofund_control_proto::AgentChangeResponse> {
        self.reply(format!("disconnect_agent:{id}"))
    }

    fn set_agent_model(
        &self,
        id: String,
        _request: hofund_control_proto::AgentModelRequest,
    ) -> ControlFuture<'_, hofund_control_proto::AgentChangeResponse> {
        self.reply(format!("set_agent_model:{id}"))
    }

    fn usage(
        &self,
        _query: hofund_control_proto::UsageQuery,
    ) -> ControlFuture<'_, hofund_control_proto::UsageResponse> {
        self.reply("usage")
    }

    fn quotas(
        &self,
        _query: hofund_control_proto::QuotaQuery,
    ) -> ControlFuture<'_, hofund_control_proto::QuotaResponse> {
        self.reply("quotas")
    }

    fn quota_history(
        &self,
        _query: hofund_control_proto::QuotaHistoryQuery,
    ) -> ControlFuture<'_, hofund_control_proto::QuotaHistoryResponse> {
        self.reply("quota_history")
    }

    fn rotate_token(
        &self,
        _request: hofund_control_proto::TokenRotateRequest,
    ) -> ControlFuture<'_, hofund_control_proto::TokenRotateResponse> {
        self.reply("rotate_token")
    }

    fn shutdown(
        &self,
        _request: hofund_control_proto::ShutdownRequest,
    ) -> ControlFuture<'_, hofund_control_proto::ShutdownResponse> {
        self.reply("shutdown")
    }

    fn config(&self) -> ControlFuture<'_, hofund_control_proto::ConfigResponse> {
        self.reply("config")
    }

    fn validate_config(
        &self,
        _request: hofund_control_proto::ConfigValidateRequest,
    ) -> ControlFuture<'_, hofund_control_proto::ConfigValidationResponse> {
        self.reply("validate_config")
    }

    fn apply_config(
        &self,
        _request: hofund_control_proto::ConfigApplyRequest,
    ) -> ControlFuture<'_, hofund_control_proto::ConfigResponse> {
        self.reply("apply_config")
    }

    fn reload_config(
        &self,
        _request: hofund_control_proto::ReloadRequest,
    ) -> ControlFuture<'_, hofund_control_proto::ConfigResponse> {
        self.reply("reload_config")
    }

    fn events(
        &self,
        _query: hofund_gateway::control::EventsQuery,
    ) -> Result<hofund_gateway::control::EventSubscription, ControlFailure> {
        Err(ControlFailure::from(
            hofund_control_proto::ControlError::new(
                ControlErrorKind::Internal,
                "events unavailable",
                "test",
            ),
        ))
    }
}

fn app(state: &Arc<R7aFake>) -> axum::Router {
    Control::new(state.clone(), TOKEN.to_owned()).into_router(LISTEN)
}

async fn send(
    app: axum::Router,
    method: Method,
    path: &str,
    body: Vec<u8>,
) -> (StatusCode, Vec<u8>) {
    let has_body = !body.is_empty();
    let mut request = Request::builder()
        .method(method)
        .uri(format!("http://{HOST}{path}"))
        .header(header::HOST, HOST)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .body(Body::from(body))
        .expect("request");
    if has_body {
        request.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/json"),
        );
    }
    let response = app.oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    (status, bytes.to_vec())
}

#[tokio::test]
async fn r7a_presets_route_reaches_state() {
    let state = Arc::new(R7aFake {
        calls: Mutex::new(Vec::new()),
        provider_summary: Mutex::new(None),
    });
    let (status, _) = send(app(&state), Method::GET, "/v1/hofund/presets", Vec::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        state
            .calls
            .lock()
            .expect("calls")
            .contains(&"presets".to_owned())
    );
}

#[tokio::test]
async fn r7a_status_includes_serve_role_fields() {
    let state = Arc::new(R7aFake {
        calls: Mutex::new(Vec::new()),
        provider_summary: Mutex::new(None),
    });
    let (status, body) = send(app(&state), Method::GET, "/v1/hofund/status", Vec::new()).await;
    assert_eq!(status, StatusCode::OK);
    let parsed: hofund_control_proto::StatusResponse =
        serde_json::from_slice(&body).expect("status json");
    assert_eq!(parsed.serves, Some(hofund_control_proto::ServeRole::Serve));
    assert_eq!(parsed.owned_by_pid, 42);
}

#[tokio::test]
async fn r7a_provider_create_unknown_field_is_rejected() {
    let state = Arc::new(R7aFake {
        calls: Mutex::new(Vec::new()),
        provider_summary: Mutex::new(None),
    });
    let body =
        br#"{"version":1,"expected_revision":"abc","preset":"fixture","extra":true}"#.to_vec();
    let (status, bytes) = send(app(&state), Method::POST, "/v1/hofund/providers", body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let envelope = decode_error_envelope(API_VERSION, &bytes).expect("error envelope");
    assert_eq!(envelope.error.kind, ControlErrorKind::BadRequest);
}

#[tokio::test]
async fn r7a_provider_list_response_has_no_api_key_field() {
    let mut summary = hofund_control_proto::ProviderSummary::default();
    summary.id = "chat".to_owned();
    summary.key_masked = "…abcd".to_owned();
    summary.enabled = true;
    let state = Arc::new(R7aFake {
        calls: Mutex::new(Vec::new()),
        provider_summary: Mutex::new(Some(summary)),
    });
    let (status, body) = send(app(&state), Method::GET, "/v1/hofund/providers", Vec::new()).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8(body).expect("utf8");
    assert!(!text.contains("api_key"));
    assert!(!text.contains("sk-"));
}

#[tokio::test]
async fn r7a_patch_provider_hits_state() {
    let state = Arc::new(R7aFake {
        calls: Mutex::new(Vec::new()),
        provider_summary: Mutex::new(None),
    });
    let body = format!(
        r#"{{"version":{},"expected_revision":"{}","enabled":false}}"#,
        API_VERSION,
        "0".repeat(64)
    )
    .into_bytes();
    let (status, _) = send(
        app(&state),
        Method::PATCH,
        "/v1/hofund/providers/chat",
        body,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        state
            .calls
            .lock()
            .expect("calls")
            .iter()
            .any(|call| call == "patch_provider:chat")
    );
}

#[tokio::test]
async fn r7a_profile_rename_route_exists() {
    let state = Arc::new(R7aFake {
        calls: Mutex::new(Vec::new()),
        provider_summary: Mutex::new(None),
    });
    let body = br#"{"version":1,"from":"work","to":"home"}"#.to_vec();
    let (status, _) = send(
        app(&state),
        Method::POST,
        "/v1/hofund/profiles/rename",
        body,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}
