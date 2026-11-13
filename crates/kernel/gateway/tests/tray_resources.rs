//! Resources retained by a running gateway for the menu-bar tray.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use hofund_control_proto::{QuotaHistory, QuotaHistoryQuery, QuotaQuery, QuotaResponse};
use hofund_gateway::{Gateway, QuotaSource, Running};
use hofund_router::{FailoverPolicy, Router, Snapshot};
use hofund_usage::Ledger;

struct EmptyQuotas;

impl QuotaSource for EmptyQuotas {
    fn report(&self, _query: QuotaQuery) -> BoxFuture<'_, QuotaResponse> {
        Box::pin(async { QuotaResponse::default() })
    }

    fn history(&self, _query: QuotaHistoryQuery) -> Vec<QuotaHistory> {
        Vec::new()
    }
}

async fn running(dir: &Path, quotas: Option<Arc<dyn QuotaSource>>) -> Running {
    let usage = Arc::new(Ledger::open(&dir.join("usage.sqlite")).expect("usage"));
    let running = Gateway::new(
        Snapshot::new(Router::new(BTreeMap::new(), FailoverPolicy::default())),
        "secret".to_owned(),
        HashMap::new(),
        usage,
    )
    .bind("127.0.0.1:0".parse().expect("listen"))
    .await
    .expect("gateway");
    match quotas {
        Some(quotas) => running.with_resource(quotas),
        None => running,
    }
}

#[tokio::test]
async fn running_returns_an_attached_quota_source() {
    let dir = tempfile::tempdir().expect("temp dir");
    let quotas: Arc<dyn QuotaSource> = Arc::new(EmptyQuotas);
    let server = running(dir.path(), Some(Arc::clone(&quotas))).await;
    let found = server.quota_source().expect("quota source");
    assert!(Arc::ptr_eq(&found, &quotas));
    server.shutdown().await;

    let server = running(dir.path(), None).await;
    assert!(server.quota_source().is_none());
    server.shutdown().await;
}
