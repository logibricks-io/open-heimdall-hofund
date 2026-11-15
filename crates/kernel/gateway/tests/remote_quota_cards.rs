//! Peer-facing kept-card routes on the model gateway.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures_util::future::BoxFuture;
use hofund_control_proto::{Quota, QuotaKind, QuotaQuery, QuotaResponse};
use hofund_gateway::{Gateway, QuotaCaller, QuotaSource};
use hofund_router::{FailoverPolicy, Router, Snapshot};
use hofund_usage::Ledger;
use serde_json::Value;
use tower::ServiceExt;

struct CardsSource {
    cards: Mutex<Vec<Quota>>,
}

impl QuotaSource for CardsSource {
    fn report(&self, _query: QuotaQuery) -> BoxFuture<'_, QuotaResponse> {
        Box::pin(async { QuotaResponse::default() })
    }

    fn history(
        &self,
        _query: hofund_control_proto::QuotaHistoryQuery,
    ) -> Vec<hofund_control_proto::QuotaHistory> {
        Vec::new()
    }

    fn kept_cards(&self) -> BoxFuture<'_, Vec<Quota>> {
        let cards = self.cards.lock().expect("cards").clone();
        Box::pin(async move { cards })
    }
}

#[tokio::test]
async fn q5f_cards_route_lists_kept_cards_with_gateway_key() {
    let source = Arc::new(CardsSource {
        cards: Mutex::new(vec![Quota {
            provider: "codex".to_owned(),
            name: "Codex".to_owned(),
            kind: QuotaKind::Subscription,
            ..Quota::default()
        }]),
    });
    let dir = tempfile::tempdir().expect("temp");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let app = Gateway::new(
        Snapshot::new(Router::new(BTreeMap::new(), FailoverPolicy::default())),
        "secret".to_owned(),
        HashMap::new(),
        usage,
    )
    .with_quotas(source as Arc<dyn QuotaSource>)
    .into_router();
    let response = app
        .oneshot(
            Request::get("/v1/hofund/quotas/cards")
                .header("authorization", "Bearer secret")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body");
    let body: Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(body["data"][0]["provider"], "codex");
}

#[test]
fn q5g_caller_carries_optional_identity() {
    let _ = QuotaCaller::new("key", None);
}
