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
use hofund_gateway::{
    Gateway, QuotaAccess, QuotaCaller, QuotaScope, QuotaScopeHook, QuotaSource,
};
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
            let mut response = QuotaResponse::default();
            response.data = vec![card("codex", "a@x.io"), card("codex", "b@x.io"), card("kimi", "")];
            response
        })
    }

    fn history(&self, query: QuotaHistoryQuery) -> Vec<QuotaHistory> {
        self.histories.lock().expect("histories").push(query);
        vec![line("codex", "a@x.io"), line("kimi", "")]
    }
}

struct OnlyAccount(&'static str);

impl QuotaScope for OnlyAccount {
    fn card(&self, provider: &str, user: &str) -> bool {
        provider == "codex" && user.eq_ignore_ascii_case(self.0)
    }
}

fn app(source: &Arc<Source>, scope: Option<QuotaScopeHook>) -> (axum::Router, tempfile::TempDir) {
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
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn bearer() -> Option<(&'static str, &'static str)> {
    Some(("authorization", "Bearer secret"))
}

#[tokio::test]
async fn q5a_quotas_answer_a_list_envelope_to_a_token_holder() {
    let source = Arc::new(Source::default());
    let (app, _dir) = app(&source, None);
    let (status, body) = get(&app, "/v1/hofund/quotas", bearer()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["object"], "list");
    assert_eq!(body["data"].as_array().map(Vec::len), Some(3));
    assert_eq!(body["data"][0]["provider"], "codex");
    assert!(body.get("version").is_none(), "the model API answers the bare envelope");
    assert!(body.get("standings").is_none(), "standings are for the control API only");
    for header in [("x-api-key", "secret"), ("x-goog-api-key", "secret")] {
        let (status, _) = get(&app, "/v1/hofund/quotas", Some(header)).await;
        assert_eq!(status, StatusCode::OK, "{} is accepted", header.0);
    }
    let (status, _) = get(&app, "/v1/hofund/quotas?key=secret", None).await;
    assert_eq!(status, StatusCode::OK, "a key query pair is accepted");
}

#[tokio::test]
async fn q5b_quotas_refuse_a_missing_or_wrong_token() {
    let source = Arc::new(Source::default());
    let (app, _dir) = app(&source, None);
    for auth in [None, Some(("authorization", "Bearer wrong"))] {
        let (status, body) = get(&app, "/v1/hofund/quotas", auth).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["type"], "authentication_error");
        let (status, _) = get(&app, "/v1/hofund/quotas/history", auth).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    assert!(source.reports.lock().expect("reports").is_empty(), "no read before auth");
}

#[tokio::test]
async fn q5c_kept_one_selects_the_kept_report_and_anything_else_reads_fresh() {
    let source = Arc::new(Source::default());
    let (app, _dir) = app(&source, None);
    for uri in [
        "/v1/hofund/quotas?kept=1",
        "/v1/hofund/quotas",
        "/v1/hofund/quotas?kept=true",
        "/v1/hofund/quotas?kept=0&other=1",
    ] {
        let (status, _) = get(&app, uri, bearer()).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
    }
    assert_eq!(
        source.reports.lock().expect("reports").as_slice(),
        [true, false, false, false]
    );
}

#[tokio::test]
async fn q5d_history_passes_its_filters_and_answers_a_list() {
    let source = Arc::new(Source::default());
    let (app, _dir) = app(&source, None);
    let (status, body) = get(
        &app,
        "/v1/hofund/quotas/history?days=7&provider=codex&user=A%40x.io&extra=1",
        bearer(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["object"], "list");
    assert_eq!(body["data"].as_array().map(Vec::len), Some(2));
    let seen = source.histories.lock().expect("histories");
    assert_eq!(seen[0].days.as_deref(), Some("7"));
    assert_eq!(seen[0].provider.as_deref(), Some("codex"));
    assert_eq!(seen[0].user.as_deref(), Some("A@x.io"));
}

#[tokio::test]
async fn q6g_a_narrowed_key_is_told_only_the_cards_it_may_use() {
    let source = Arc::new(Source::default());
    let callers = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&callers);
    let scope: QuotaScopeHook = Arc::new(move |caller: QuotaCaller<'_>| {
        seen.lock().expect("callers").push(caller.key().to_owned());
        QuotaAccess::Scoped(Arc::new(OnlyAccount("A@X.IO")))
    });
    let (app, _dir) = app(&source, Some(scope));
    let (status, body) = get(&app, "/v1/hofund/quotas", bearer()).await;
    assert_eq!(status, StatusCode::OK);
    let data = body["data"].as_array().expect("data");
    assert_eq!(data.len(), 1);
    assert_eq!(data[0]["user"], "a@x.io");
    let (_, body) = get(&app, "/v1/hofund/quotas/history", bearer()).await;
    let data = body["data"].as_array().expect("data");
    assert_eq!(data.len(), 1);
    assert_eq!(data[0]["provider"], "codex");
    assert_eq!(callers.lock().expect("callers").as_slice(), ["secret", "secret"]);
}

#[tokio::test]
async fn q6h_a_refused_key_is_answered_403_without_a_read() {
    let source = Arc::new(Source::default());
    let scope: QuotaScopeHook = Arc::new(|_: QuotaCaller<'_>| QuotaAccess::Refused);
    let (app, _dir) = app(&source, Some(scope));
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
