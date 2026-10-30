//! A surface never sees the gateway's own token: each presented secret that
//! matches it is taken out before the surface runs, and the caller's own
//! credentials stay. Stand-in surface. No vendor names.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

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

static ID: SurfaceId = SurfaceId::from_static("seen");
static ROUTES: [RouteSpec; 1] = [RouteSpec::from_static(Method::Get, "/seen")];

const CREDENTIALS: [&str; 3] = ["authorization", "x-api-key", "x-goog-api-key"];

/// What the surface saw: the target, and each credential header.
type Sight = (String, Vec<Option<String>>);

/// Keeps what it was given and answers 204.
#[derive(Default)]
struct Seen {
    seen: Mutex<Vec<Sight>>,
}

#[async_trait]
impl InboundSurface for Seen {
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
        _host: &SurfaceHost,
    ) -> Result<axum::http::Response<BodyStream>, PluginError> {
        let headers = CREDENTIALS
            .iter()
            .map(|name| {
                request
                    .headers()
                    .get(*name)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned)
            })
            .collect();
        self.seen
            .lock()
            .expect("seen")
            .push((request.uri().to_string(), headers));
        Ok(axum::http::Response::builder()
            .status(204)
            .body(BodyStream::from_bytes(Vec::new()))
            .expect("response"))
    }
}

/// What the surface sees of `GET target` with `headers`.
async fn sight(target: &str, headers: &[(&str, &str)]) -> Sight {
    let dir = tempfile::tempdir().expect("temp dir");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let surface = Arc::new(Seen::default());
    let app = Gateway::new(
        Snapshot::new(Router::new(BTreeMap::new(), FailoverPolicy::default())),
        "secret".to_owned(),
        HashMap::new(),
        usage,
    )
    .with_surfaces(vec![Arc::clone(&surface) as Arc<dyn InboundSurface>])
    .into_router();
    let mut builder = Request::builder().method("GET").uri(target);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let response = app
        .oneshot(builder.body(Body::empty()).expect("request"))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let mut seen = surface.seen.lock().expect("seen");
    assert_eq!(seen.len(), 1);
    seen.remove(0)
}

fn some(value: &str) -> Option<String> {
    Some(value.to_owned())
}

#[tokio::test]
async fn the_bearer_token_is_taken_out() {
    let (target, headers) = sight("/seen", &[("authorization", "Bearer secret")]).await;
    assert_eq!(target, "/seen");
    assert_eq!(headers, vec![None, None, None]);
}

#[tokio::test]
async fn the_api_key_token_goes_and_the_callers_bearer_stays() {
    let (_, headers) = sight(
        "/seen",
        &[
            ("x-api-key", "secret"),
            ("authorization", "Bearer upstream"),
        ],
    )
    .await;
    assert_eq!(headers, vec![some("Bearer upstream"), None, None]);
}

#[tokio::test]
async fn the_goog_key_token_goes_and_other_keys_stay() {
    let (_, headers) = sight(
        "/seen",
        &[("x-goog-api-key", " secret "), ("x-api-key", "theirs")],
    )
    .await;
    assert_eq!(headers, vec![None, some("theirs"), None]);
}

#[tokio::test]
async fn a_matching_key_pair_leaves_the_query_and_the_rest_stays() {
    let (target, headers) = sight(
        "/seen?alt=sse&key=s%65cret&key=theirs&x",
        &[("authorization", "Bearer upstream")],
    )
    .await;
    assert_eq!(target, "/seen?alt=sse&key=theirs&x");
    assert_eq!(headers, vec![some("Bearer upstream"), None, None]);
}

#[tokio::test]
async fn a_query_of_only_the_token_is_dropped() {
    let (target, _) = sight("/seen?key=secret", &[]).await;
    assert_eq!(target, "/seen");
}

#[tokio::test]
async fn a_query_without_the_token_is_kept_as_it_came() {
    let (target, _) = sight("/seen?client_version=1.2&key", &[("x-api-key", "secret")]).await;
    assert_eq!(target, "/seen?client_version=1.2&key");
}
