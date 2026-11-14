//! Tests for gateway-coordinated Claude warm-up.

use hofund_control_proto::{Quota, QuotaKind};
use hofund_router::quota::QuotaStore;

use super::claude;

#[test]
fn a3a_29_stale_allowance_clears_read_timestamp() {
    let store = QuotaStore::in_memory();
    let card = Quota {
        provider: "claude".to_owned(),
        kind: QuotaKind::Subscription,
        user: "me@example.com".to_owned(),
        ..Quota::default()
    };
    store.record_success(card.clone(), Vec::new(), store.now());
    let before = store.report(&[]);
    assert!(before[0].read_at.is_some());
    claude::stale_allowance(&store, &card);
    let after = store.report(&[]);
    assert!(after[0].read_at.is_none());
}

#[tokio::test]
async fn a3a_30_is_claude_subscription_matches_config_id() {
    assert!(claude::is_claude_subscription("claude"));
    assert!(!claude::is_claude_subscription("codex"));
}
