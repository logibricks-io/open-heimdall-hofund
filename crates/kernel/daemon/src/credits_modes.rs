//! Operator credits modes beside quota reports.

use std::sync::Arc;

use hofund_control_proto::{
    Quota, QuotaCreditsModeEntry, QuotaCreditsModesResponse, QuotaCreditsSetRequest,
    QuotaCreditsSetResponse,
};
use hofund_plugin_api::AccountId;
use hofund_router::credits_mode::{CreditsMode, CreditsModeStore};

use crate::runtime::Runtime;
use hofund_gateway::control::ControlFailure;

pub(crate) fn decorate_quotas(cards: &mut [Quota], store: &CreditsModeStore) {
    for card in cards {
        if !card.kind.is_account() || card.user.is_empty() {
            continue;
        }
        let mode = store.mode(&card.provider, &card.user);
        if !mode.is_default() {
            card.credits_mode = mode.as_str().to_owned();
        }
    }
}

pub(crate) fn list(store: &CreditsModeStore) -> QuotaCreditsModesResponse {
    QuotaCreditsModesResponse {
        accounts: store
            .listed()
            .into_iter()
            .map(|(provider, user, mode)| QuotaCreditsModeEntry {
                provider,
                user,
                mode: mode.as_str().to_owned(),
            })
            .collect(),
        ..QuotaCreditsModesResponse::default()
    }
}

pub(crate) async fn set(
    runtime: &Arc<Runtime>,
    store: &CreditsModeStore,
    request: QuotaCreditsSetRequest,
) -> Result<QuotaCreditsSetResponse, ControlFailure> {
    let Some(mode) = CreditsMode::parse(&request.mode) else {
        return Err(bad_request(format!(
            "credits mode must be on, first, last, or off, not {:?}",
            request.mode
        )));
    };
    let provider = request.provider.trim();
    if provider.is_empty() {
        return Err(bad_request("provider is required".to_owned()));
    }
    let account_id = AccountId::try_from(request.user.as_str())
        .map_err(|error| bad_request(error.to_string()))?;
    let subscriptions = runtime.subscription_states();
    let state = subscriptions
        .get(provider)
        .ok_or_else(|| bad_request(format!("unknown subscription {provider:?}")))?;
    if !state
        .members()
        .iter()
        .any(|member| member.account == account_id)
    {
        return Err(bad_request(format!(
            "unknown account {} on subscription {provider}",
            account_id
        )));
    }
    store.set(provider, account_id.as_str(), mode);
    Ok(QuotaCreditsSetResponse {
        provider: provider.to_owned(),
        user: account_id.to_string(),
        mode: mode.as_str().to_owned(),
        ..QuotaCreditsSetResponse::default()
    })
}

fn bad_request(message: String) -> ControlFailure {
    hofund_control_proto::ControlError::new(
        hofund_control_proto::ControlErrorKind::BadRequest,
        message,
        "quotas",
    )
    .into()
}
