//! Quota routes on the model gateway: token auth, the list envelope, the
//! kept switch, history filters, and what a narrowed key is told.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures_util::future::BoxFuture;
use hofund_control_proto::{
    Quota, QuotaHistory, QuotaHistoryQuery, QuotaKind, QuotaQuery, QuotaResponse,
};
use hofund_gateway::{Gateway, QuotaAccess, QuotaCaller, QuotaScope, QuotaScopeHook, QuotaSource};
use hofund_router::{FailoverPolicy, Router, Snapshot};
use hofund_usage::Ledger;
use serde_json::Value;
use tower::ServiceExt;

#[derive(Default)]
struct Source {
    reports: Mutex<Vec<bool>>,
    histories: Mutex<Vec<QuotaHistoryQuery>>,
}

fn card(provider: &str, user: &str) -> Quota {
    Quota {
        provider: provider.to_owned(),
        name: provider.to_owned(),
        kind: QuotaKind::Subscription,
        user: user.to_owned(),
        ..Quota::default()
    }
}

fn line(provider: &str, user: &str) -> QuotaHistory {
    QuotaHistory {
        provider: provider.to_owned(),
        user: user.to_owned(),
        lines: Vec::new(),
    }
}

impl QuotaSource for Source {
    fn report(&self, query: QuotaQuery) -> BoxFuture<'_, QuotaResponse> {
        self.reports.lock().expect("reports").push(query.kept);
        Box::pin(async move {
            QuotaResponse {
                data: vec![
                    card("acct", "a@x.io"),
                    card("acct", "b@x.io"),
                    card("plan", ""),
                ],
                ..QuotaResponse::default()
            }
        })
    }

    fn history(&self, query: QuotaHistoryQuery) -> Vec<QuotaHistory> {
        self.histories.lock().expect("histories").push(query);
        vec![line("acct", "a@x.io"), line("plan", "")]
    }
}

struct OnlyAccount(&'static str);

impl QuotaScope for OnlyAccount {
    fn card(&self, provider: &str, user: &str) -> bool {
        provider == "acct" && user.eq_ignore_ascii_case(self.0)
    }
}

fn app_with(
    source: &Arc<Source>,
    scope: Option<QuotaScopeHook>,
) -> (axum::Router, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temp dir");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let mut gateway = Gateway::new(
        Snapshot::new(Router::new(BTreeMap::new(), FailoverPolicy::default())),
        "secret".to_owned(),
        HashMap::new(),
        usage,
    )
    .with_quotas(Arc::clone(source) as Arc<dyn QuotaSource>);
    if let Some(scope) = scope {
        gateway = gateway.with_quota_scope(scope);
    }
    (gateway.into_router(), dir)
}

async fn get(app: &axum::Router, uri: &str, auth: Option<(&str, &str)>) -> (StatusCode, Value) {
    let mut request = Request::get(uri);
    if let Some((name, value)) = auth {
        request = request.header(name, value);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::empty()).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn bearer() -> Option<(&'static str, &'static str)> {
    Some(("authorization", "Bearer secret"))
}

#[tokio::test]
async fn q5a_snapshot_route_supports_kept_and_fresh_reads() {
    let source = Arc::new(Source::default());
    let (app, _dir) = app_with(&source, None);
    let (status, body) = get(&app, "/v1/hofund/quotas", bearer()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["object"], "list");
    assert_eq!(body["data"].as_array().map(Vec::len), Some(3));
    assert_eq!(body["data"][0]["provider"], "acct");
    assert!(
        body.get("version").is_none(),
        "the model API answers the bare envelope"
    );
    assert!(
        body.get("standings").is_none(),
        "standings are for the control API only"
    );
    for header in [("x-api-key", "secret"), ("x-goog-api-key", "secret")] {
        let (status, _) = get(&app, "/v1/hofund/quotas", Some(header)).await;
        assert_eq!(status, StatusCode::OK, "{} is accepted", header.0);
    }
    let (status, _) = get(&app, "/v1/hofund/quotas?key=secret", None).await;
    assert_eq!(status, StatusCode::OK, "a key query pair is accepted");
    source.reports.lock().expect("reports").clear();
    for uri in [
        "/v1/hofund/quotas?kept=1",
        "/v1/hofund/quotas",
        "/v1/hofund/quotas?kept=true",
        "/v1/hofund/quotas?kept=0&other=1",
    ] {
        let (status, body) = get(&app, uri, bearer()).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(body["object"], "list", "{uri}");
    }
    assert_eq!(
        source.reports.lock().expect("reports").as_slice(),
        [true, false, false, false],
        "only kept=1 selects the kept report"
    );
}

#[tokio::test]
async fn q5c_snapshot_requires_gateway_authentication() {
    let source = Arc::new(Source::default());
    let (app, _dir) = app_with(&source, None);
    for auth in [None, Some(("authorization", "Bearer wrong"))] {
        let (status, body) = get(&app, "/v1/hofund/quotas", auth).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["type"], "authentication_error");
        let (status, _) = get(&app, "/v1/hofund/quotas/history", auth).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    assert!(
        source.reports.lock().expect("reports").is_empty(),
        "no read before auth"
    );
    let (_, body) = get(
        &app,
        "/v1/hofund/quotas",
        Some(("authorization", "Bearer wrong")),
    )
    .await;
    assert!(
        !body.to_string().contains("wrong"),
        "the presented key is never echoed"
    );
    assert!(!body.to_string().contains("secret"));
}

#[tokio::test]
async fn q6g_history_route_filters_and_scopes() {
    let source = Arc::new(Source::default());
    let (app, _dir) = app_with(&source, None);
    let (status, body) = get(
        &app,
        "/v1/hofund/quotas/history?days=7&provider=acct&user=A%40x.io&extra=1",
        bearer(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["object"], "list");
    assert_eq!(body["data"].as_array().map(Vec::len), Some(2));
    let seen = source.histories.lock().expect("histories")[0].clone();
    assert_eq!(seen.days.as_deref(), Some("7"));
    assert_eq!(seen.provider.as_deref(), Some("acct"));
    assert_eq!(seen.user.as_deref(), Some("A@x.io"));
    let scope: QuotaScopeHook =
        Arc::new(|_: QuotaCaller<'_>| QuotaAccess::Scoped(Arc::new(OnlyAccount("A@X.IO"))));
    let (scoped, _dir) = app_with(&source, Some(scope));
    let (_, body) = get(&scoped, "/v1/hofund/quotas/history?days=1", bearer()).await;
    let data = body["data"].as_array().expect("data");
    assert_eq!(data.len(), 1, "the scope narrows history as it does cards");
    assert_eq!(data[0]["provider"], "acct");
    assert_eq!(data[0]["user"], "a@x.io");
}

#[tokio::test]
async fn q5b_scope_hook_filters_snapshot_cards() {
    let source = Arc::new(Source::default());
    let callers = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&callers);
    let scope: QuotaScopeHook = Arc::new(move |caller: QuotaCaller<'_>| {
        seen.lock().expect("callers").push(caller.key().to_owned());
        QuotaAccess::Scoped(Arc::new(OnlyAccount("A@X.IO")))
    });
    let (app, _dir) = app_with(&source, Some(scope));
    let (unscoped, _other) = app_with(&source, None);
    let (status, body) = get(&app, "/v1/hofund/quotas", bearer()).await;
    assert_eq!(status, StatusCode::OK);
    let data = body["data"].as_array().expect("data");
    assert_eq!(data.len(), 1);
    assert_eq!(data[0]["user"], "a@x.io");
    let (_, body) = get(&app, "/v1/hofund/quotas?kept=1", bearer()).await;
    assert_eq!(
        body["data"].as_array().map(Vec::len),
        Some(1),
        "kept reports are scoped too"
    );
    let (_, body) = get(&unscoped, "/v1/hofund/quotas", bearer()).await;
    assert_eq!(
        body["data"].as_array().map(Vec::len),
        Some(3),
        "the default tells everything"
    );
    assert_eq!(
        callers.lock().expect("callers").as_slice(),
        ["secret", "secret"]
    );
}

#[tokio::test]
async fn q5d_a_refused_key_is_answered_403_without_a_read() {
    let source = Arc::new(Source::default());
    let scope: QuotaScopeHook = Arc::new(|_: QuotaCaller<'_>| QuotaAccess::Refused);
    let (app, _dir) = app_with(&source, Some(scope));
    for uri in ["/v1/hofund/quotas", "/v1/hofund/quotas/history"] {
        let (status, body) = get(&app, uri, bearer()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
        assert_eq!(body["error"]["type"], "permission_error");
    }
    assert!(source.reports.lock().expect("reports").is_empty());
    assert!(source.histories.lock().expect("histories").is_empty());
}

#[tokio::test]
async fn q5e_no_quota_source_means_no_quota_route() {
    let dir = tempfile::tempdir().expect("temp dir");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let app = Gateway::new(
        Snapshot::new(Router::new(BTreeMap::new(), FailoverPolicy::default())),
        "secret".to_owned(),
        HashMap::new(),
        usage,
    )
    .into_router();
    let (status, _) = get(&app, "/v1/hofund/quotas", bearer()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
