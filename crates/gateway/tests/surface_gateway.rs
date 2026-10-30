//! An inbound surface reaches the gateway's codec routes in process.
//! Stand-in surface and provider. No vendor names.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use hofund_codec::{Codec, OpenAiChat};
use hofund_gateway::Gateway;
use hofund_ir::{ContentPart, Message, Request as ModelRequest, Response, Role};
use hofund_plugin_api::{
    BodyStream, GatewayHandle, InboundSurface, MemberId, MemberKey, Method, PluginError,
    PluginErrorKind, PresetId, RouteSpec, SurfaceHost, SurfaceId,
};
use hofund_router::{Completion, FailoverPolicy, Member, Router, Snapshot};
use hofund_usage::Ledger;
use tower::ServiceExt;

struct Fake {
    member_id: MemberId,
    hits: Arc<AtomicUsize>,
}

#[async_trait]
impl Member for Fake {
    fn id(&self) -> &MemberId {
        &self.member_id
    }

    fn route_id(&self) -> &str {
        "primary"
    }

    fn protocol(&self) -> &str {
        "plain"
    }

    async fn execute(
        &self,
        _request: ModelRequest,
        _inbound_protocol: &str,
    ) -> Result<Completion, PluginError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        let mut message = Message::new(Role::Assistant);
        message.content.push(ContentPart::text("relayed"));
        Ok(Completion::Unary(Box::new(Response::new(message))))
    }
}

static ID: SurfaceId = SurfaceId::from_static("relay");
static ROUTES: [RouteSpec; 2] = [
    RouteSpec::from_static(Method::Get, "/relay"),
    RouteSpec::from_static(Method::Post, "/relay"),
];

/// Forwards `/relay/<path>` to `/<path>` on the gateway, presenting its own
/// wrong credentials, and keeps the handle it was given.
#[derive(Default)]
struct Relay {
    kept: Mutex<Vec<GatewayHandle>>,
}

#[async_trait]
impl InboundSurface for Relay {
    fn id(&self) -> &SurfaceId {
        &ID
    }

    fn routes(&self) -> &'static [RouteSpec] {
        &ROUTES
    }

    async fn handle(
        &self,
        _route: &RouteSpec,
        request: axum::http::Request<hofund_plugin_api::Body>,
        host: &SurfaceHost,
    ) -> Result<axum::http::Response<BodyStream>, PluginError> {
        self.kept.lock().expect("kept").push(host.gateway().clone());
        let (parts, body) = request.into_parts();
        let target = parts
            .uri
            .path_and_query()
            .map_or("/", |path| path.as_str())
            .strip_prefix("/relay")
            .unwrap_or("/")
            .to_owned();
        let target = if target.contains('?') {
            format!("{target}&key=wrong")
        } else {
            format!("{target}?key=wrong")
        };
        let forwarded = axum::http::Request::builder()
            .method(parts.method)
            .uri(target)
            .header("authorization", "Bearer wrong")
            .header("x-api-key", "wrong")
            .header("x-goog-api-key", "wrong")
            .header("content-type", "application/json")
            .body(body)
            .expect("forwarded");
        host.gateway().call(forwarded).await
    }
}

fn gateway(hits: &Arc<AtomicUsize>, relay: Arc<Relay>) -> (axum::Router, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temp dir");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let member: Arc<dyn Member> = Arc::new(Fake {
        member_id: MemberId::from_preset(
            PresetId::try_from("primary").expect("preset"),
            MemberKey::from_static("row"),
        ),
        hits: Arc::clone(hits),
    });
    let mut routes = BTreeMap::new();
    routes.insert("demo".to_owned(), vec![member]);
    let mut codecs = HashMap::new();
    codecs.insert(
        "/v1/chat/completions".to_owned(),
        Arc::new(OpenAiChat) as Arc<dyn Codec>,
    );
    let app = Gateway::new(
        Snapshot::new(Router::new(routes, FailoverPolicy::default())),
        "secret".to_owned(),
        codecs,
        usage,
    )
    .with_surfaces(vec![relay as Arc<dyn InboundSurface>])
    .into_router();
    (app, dir)
}

async fn send(
    app: axum::Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: &'static str,
) -> (StatusCode, String) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .oneshot(builder.body(Body::from(body)).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

const CHAT: &str = r#"{"model":"demo","messages":[{"role":"user","content":"hi"}]}"#;

#[tokio::test]
async fn a_surface_reaches_a_codec_route_with_the_gateway_credential() {
    let hits = Arc::new(AtomicUsize::new(0));
    let (app, _dir) = gateway(&hits, Arc::new(Relay::default()));
    let (status, body) = send(
        app.clone(),
        "POST",
        "/relay/v1/chat/completions",
        Some("secret"),
        CHAT,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("relayed"), "{body}");
    assert!(body.contains("chat.completion"), "{body}");
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    let (status, body) = send(
        app.clone(),
        "GET",
        "/relay/v1/models?x=1",
        Some("secret"),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains(r#""id":"demo""#), "{body}");
}

#[tokio::test]
async fn the_inbound_request_is_still_authenticated_first() {
    let hits = Arc::new(AtomicUsize::new(0));
    let (app, _dir) = gateway(&hits, Arc::new(Relay::default()));
    let (status, _) = send(
        app.clone(),
        "POST",
        "/relay/v1/chat/completions",
        None,
        CHAT,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = send(
        app,
        "POST",
        "/relay/v1/chat/completions",
        Some("nope"),
        CHAT,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_surface_cannot_reach_a_surface_or_an_unknown_path() {
    let hits = Arc::new(AtomicUsize::new(0));
    let (app, _dir) = gateway(&hits, Arc::new(Relay::default()));
    let (status, _) = send(
        app.clone(),
        "POST",
        "/relay/relay/v1/chat/completions",
        Some("secret"),
        CHAT,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(app, "POST", "/relay/v1/hofund/status", Some("secret"), "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_kept_handle_fails_once_the_gateway_is_gone() {
    let hits = Arc::new(AtomicUsize::new(0));
    let relay = Arc::new(Relay::default());
    let (app, dir) = gateway(&hits, Arc::clone(&relay));
    let (status, _) = send(app.clone(), "GET", "/relay/v1/models", Some("secret"), "").await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(app.clone(), "GET", "/relay/v1/models", Some("secret"), "").await;
    assert_eq!(status, StatusCode::OK);
    drop(app);
    drop(dir);
    let handles = relay.kept.lock().expect("kept").clone();
    assert_eq!(handles.len(), 2);
    assert_eq!(handles[0], handles[0].clone());
    assert_ne!(handles[0], handles[1], "each request has its own backend");
    let kept = handles[0].clone();
    assert!(kept.is_available());
    assert!(format!("{kept:?}").contains("available: true"));
    let request = axum::http::Request::builder()
        .uri("/v1/models")
        .body(hofund_plugin_api::Body::empty())
        .expect("request");
    let error = kept.call(request).await.expect_err("gone");
    assert_eq!(error.kind(), PluginErrorKind::Cancelled);
}

#[tokio::test]
async fn a_host_without_a_gateway_says_so() {
    let host = hofund_plugin_host::surface_host(Vec::new(), Vec::new(), true);
    assert!(!host.gateway().is_available());
    let request = axum::http::Request::builder()
        .uri("/v1/models")
        .body(hofund_plugin_api::Body::empty())
        .expect("request");
    let error = host.gateway().call(request).await.expect_err("no gateway");
    assert_eq!(error.kind(), PluginErrorKind::Unsupported);
    assert_eq!(host.gateway(), host.clone().gateway());
    assert!(format!("{:?}", host.gateway()).contains("available: false"));
}
