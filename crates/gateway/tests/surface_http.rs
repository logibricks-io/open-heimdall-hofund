//! An inbound surface relays on the kernel's pooled client when the gateway
//! has one. Stand-in surface and backend. No vendor names.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use hofund_gateway::Gateway;
use hofund_plugin_api::{
    BodyStream, InboundSurface, Method, PluginError, RouteSpec, SurfaceHost, SurfaceId,
};
use hofund_plugin_host::RecordingHttp;
use hofund_router::{FailoverPolicy, Router, Snapshot};
use hofund_usage::Ledger;
use tower::ServiceExt;

static ID: SurfaceId = SurfaceId::from_static("outbound");
static ROUTES: [RouteSpec; 1] = [RouteSpec::from_static(Method::Post, "/outbound")];

/// Sends the inbound body to `https://upstream.test/echo` with the caller's
/// own `authorization`, and keeps the header it saw. Answers 501 when the host
/// has no HTTP handle.
#[derive(Default)]
struct Outbound {
    seen: Mutex<Vec<Option<String>>>,
}

#[async_trait]
impl InboundSurface for Outbound {
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
        let (parts, body) = request.into_parts();
        let caller = parts
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        self.seen.lock().expect("seen").push(caller.clone());
        let Some(http) = host.http() else {
            return Ok(axum::http::Response::builder()
                .status(501)
                .body(BodyStream::from_bytes(b"no http".to_vec()))
                .expect("response"));
        };
        let mut outbound = axum::http::Request::builder()
            .method("POST")
            .uri("https://upstream.test/echo");
        if let Some(caller) = caller {
            outbound = outbound.header("authorization", caller);
        }
        http.send(outbound.body(body).expect("outbound")).await
    }
}

fn gateway(
    surface: Arc<Outbound>,
    http: Option<Arc<RecordingHttp>>,
) -> (axum::Router, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temp dir");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let mut gateway = Gateway::new(
        Snapshot::new(Router::new(BTreeMap::new(), FailoverPolicy::default())),
        "secret".to_owned(),
        HashMap::new(),
        usage,
    )
    .with_surfaces(vec![surface as Arc<dyn InboundSurface>]);
    if let Some(http) = http {
        gateway = gateway.with_surface_http(http);
    }
    (gateway.into_router(), dir)
}

async fn send(app: axum::Router, headers: &[(&str, &str)]) -> (StatusCode, String) {
    let mut builder = Request::builder().method("POST").uri("/outbound");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let response = app
        .oneshot(builder.body(Body::from("payload")).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
async fn a_surface_relays_on_the_pooled_client_with_the_callers_credential() {
    let http = Arc::new(RecordingHttp::default());
    http.enqueue_response(201, &[("content-type", "text/plain")], "relayed");
    let surface = Arc::new(Outbound::default());
    let (app, _dir) = gateway(Arc::clone(&surface), Some(Arc::clone(&http)));
    // The gateway token in x-api-key is enough; the bearer is the caller's own.
    let (status, body) = send(
        app,
        &[
            ("x-api-key", "secret"),
            ("authorization", "Bearer upstream"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body, "relayed");
    let calls = http.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].method(), "POST");
    assert_eq!(calls[0].uri(), "https://upstream.test/echo");
    assert_eq!(calls[0].body(), b"payload");
    assert_eq!(
        surface.seen.lock().expect("seen").as_slice(),
        &[Some("Bearer upstream".to_owned())]
    );
}

#[tokio::test]
async fn a_gateway_without_a_pool_gives_surfaces_no_http() {
    let surface = Arc::new(Outbound::default());
    let (app, _dir) = gateway(Arc::clone(&surface), None);
    let (status, body) = send(app, &[("authorization", "Bearer secret")]).await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    assert_eq!(body, "no http");
}

#[tokio::test]
async fn a_caller_credential_alone_is_still_rejected() {
    let http = Arc::new(RecordingHttp::default());
    let surface = Arc::new(Outbound::default());
    let (app, _dir) = gateway(Arc::clone(&surface), Some(Arc::clone(&http)));
    let (status, _) = send(app, &[("authorization", "Bearer upstream")]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(http.calls().is_empty());
    assert!(surface.seen.lock().expect("seen").is_empty());
}
