//! Gateway-coordinated Claude Code warm-up and model probes.
//!
//! The reference runs one "hi" through Claude Code from the gateway layer and
//! wires scheduled warm-up, warm-up now, and model tests to that send. This
//! module is the daemon-side coordinator: it calls `Subscription::warmup_probe`
//! on the Claude subscription plugin, which spawns Claude Code with the official
//! argv shape.

use std::sync::Arc;
use std::time::Duration;

use hofund_control_proto::Quota;
use hofund_plugin_api::MemberId;
use hofund_plugin_api::{AccountId, PluginErrorKind};
use hofund_router::quota::QuotaStore;

use crate::runtime::Runtime;
use crate::subscriptions::SubscriptionState;

/// Config id for the built-in Claude subscription.
pub(crate) const SUBSCRIPTION_ID: &str = "claude";

const ASK_TIMEOUT: Duration = Duration::from_secs(2 * 60);

pub(crate) fn is_claude_subscription(subscription: &str) -> bool {
    subscription == SUBSCRIPTION_ID
}

/// One "hi" at Haiku, as the scheduled warm-up sends.
pub(crate) async fn warm(state: &SubscriptionState, account: &AccountId) -> Result<(), String> {
    ask(state, account, "haiku").await
}

/// One "hi" at `model`, as a model connectivity test sends.
pub(crate) async fn ask(
    state: &SubscriptionState,
    account: &AccountId,
    model: &str,
) -> Result<(), String> {
    let run = state.instance().warmup_probe(account, state.host(), model);
    match tokio::time::timeout(ASK_TIMEOUT, run).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) if error.kind() == PluginErrorKind::Unsupported => {
            Err("warm-up probe is not supported".to_owned())
        }
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => Err("warm-up probe timed out".to_owned()),
    }
}

/// After a successful warm-up, quota should be read again soon for routing.
pub(crate) async fn warmed_now(
    runtime: &Arc<Runtime>,
    member: &MemberId,
    state: &SubscriptionState,
    account: &AccountId,
) {
    stale_allowance(runtime.quotas(), &state.quota_card(account));
    runtime.wake_quota_poll(member).await;
}

/// Makes the next allowance read ask the vendor again rather than trust the cache.
pub(crate) fn stale_allowance(quotas: &QuotaStore, card: &Quota) {
    quotas.stale_allowance(card);
}
