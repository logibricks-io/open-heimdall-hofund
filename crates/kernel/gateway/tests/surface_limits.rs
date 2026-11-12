//! How much a surface may be sent and how long it may take. Stand-in
//! surface, no members.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use hofund_gateway::Gateway;
use hofund_plugin_api::{
    BodyStream, InboundSurface, Method, PluginError, RouteSpec, SurfaceHost, SurfaceId,
};
use hofund_router::{FailoverPolicy, Router, Snapshot};
use hofund_usage::Ledger;
use tower::ServiceExt;

static ID: SurfaceId = SurfaceId::from_static("limits");
static ROUTES: [RouteSpec; 2] = [
    RouteSpec::from_static(Method::Post, "/size"),
    RouteSpec::from_static(Method::Post, "/sleep"),
];

/// `/size` answers with the length of the body it got. `/sleep` waits the
/// number of seconds the body names, then answers.
struct Limits;

#[async_trait]
impl InboundSurface for Limits {
    fn id(&self) -> &SurfaceId {
        &ID
    }

    fn routes(&self) -> &'static [RouteSpec] {
        &ROUTES
    }

    async fn handle(
        &self,
        route: &RouteSpec,
        request: axum::http::Request<hofund_plugin_api::Body>,
        _host: &SurfaceHost,
    ) -> Result<axum::http::Response<BodyStream>, PluginError> {
        let body = request.body().bytes();
        let answer = if route.prefix() == "/size" {
            body.len().to_string()
        } else {
            let seconds: u64 = String::from_utf8_lossy(body).parse().expect("seconds");
            tokio::time::sleep(Duration::from_secs(seconds)).await;
            "awake".to_owned()
        };
        Ok(axum::http::Response::builder()
            .status(200)
            .body(BodyStream::from_bytes(answer.into_bytes()))
            .expect("response"))
    }
}

async fn post(path: &str, body: Vec<u8>) -> (StatusCode, String) {
    let dir = tempfile::tempdir().expect("temp dir");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let app = Gateway::new(
        Snapshot::new(Router::from_hops(
            BTreeMap::new(),
            FailoverPolicy::default(),
        )),
        "secret".to_owned(),
        HashMap::new(),
        usage,
    )
    .with_surfaces(vec![Arc::new(Limits) as Arc<dyn InboundSurface>])
    .into_router();
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header("authorization", "Bearer secret")
        .body(Body::from(body))
        .expect("request");
    let response = app.oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

const MIB: usize = 1024 * 1024;

#[tokio::test]
async fn a_surface_takes_a_body_up_to_32_mib() {
    for size in [9 * MIB, 32 * MIB] {
        assert_eq!(
            post("/size", vec![b'a'; size]).await,
            (StatusCode::OK, size.to_string())
        );
    }
    assert_eq!(
        post("/size", vec![b'a'; 32 * MIB + 1]).await,
        (
            StatusCode::PAYLOAD_TOO_LARGE,
            "request body is too large".to_owned()
        )
    );
}

#[tokio::test(start_paused = true)]
async fn a_surface_has_three_minutes() {
    assert_eq!(
        post("/sleep", b"179".to_vec()).await,
        (StatusCode::OK, "awake".to_owned())
    );
    assert_eq!(
        post("/sleep", b"181".to_vec()).await,
        (
            StatusCode::GATEWAY_TIMEOUT,
            "kernel deadline exceeded".to_owned()
        )
    );
}
