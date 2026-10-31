//! The daemon's quota reports, shared by the model gateway's quota routes
//! and the control API behind `hofund quota`.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::future::BoxFuture;
use hofund_control_proto::{
    QuotaHistory, QuotaHistoryQuery, QuotaHistoryResponse, QuotaQuery, QuotaResponse,
};
use hofund_gateway::QuotaSource;
use hofund_router::quota::{SERVED_RETENTION, ServedRequest, apply_last_served};
use hofund_usage::Ledger;

use crate::runtime::Runtime;

/// Quota reads over the runtime's store, decorated with when each account
/// last answered through this gateway.
pub(crate) struct DaemonQuotas {
    runtime: Arc<Runtime>,
    usage: Arc<Ledger>,
}

impl DaemonQuotas {
    pub(crate) fn new(runtime: Arc<Runtime>, usage: Arc<Ledger>) -> Self {
        Self { runtime, usage }
    }

    /// The report with standings; a kept query never asks a vendor.
    pub(crate) async fn response(&self, query: &QuotaQuery) -> QuotaResponse {
        let (mut data, standings) = self.runtime.quota_report(query.kept).await;
        let now = self.runtime.quota_now();
        let served = served_since(Arc::clone(&self.usage), now).await;
        apply_last_served(&mut data, &served, now);
        QuotaResponse {
            data,
            standings,
            ..QuotaResponse::default()
        }
    }

    /// Sampled history for the query.
    pub(crate) fn lines(&self, query: &QuotaHistoryQuery) -> Vec<QuotaHistory> {
        self.runtime.quota_histories(
            query.days.as_deref(),
            query.provider.as_deref(),
            query.user.as_deref(),
        )
    }

    /// The history response for the control API.
    pub(crate) fn history_response(&self, query: &QuotaHistoryQuery) -> QuotaHistoryResponse {
        QuotaHistoryResponse {
            data: self.lines(query),
            ..QuotaHistoryResponse::default()
        }
    }
}

impl QuotaSource for DaemonQuotas {
    fn report(&self, query: QuotaQuery) -> BoxFuture<'_, QuotaResponse> {
        Box::pin(async move { self.response(&query).await })
    }

    fn history(&self, query: QuotaHistoryQuery) -> Vec<QuotaHistory> {
        self.lines(&query)
    }
}

/// Answered requests within the served retention, newest per upstream. An
/// unreadable ledger decorates nothing.
async fn served_since(usage: Arc<Ledger>, now: SystemTime) -> Vec<ServedRequest> {
    let since = now
        .checked_sub(SERVED_RETENTION)
        .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        });
    let rows = tokio::task::spawn_blocking(move || usage.last_served(since))
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    rows.into_iter()
        .filter_map(|(upstream, created)| {
            let seconds = u64::try_from(created).ok()?;
            Some(ServedRequest {
                member: member_key(&upstream),
                at: UNIX_EPOCH + Duration::from_secs(seconds),
            })
        })
        .collect()
}

/// A ledger upstream as a card's served key: a subscription member's
/// account compares without case.
pub(crate) fn member_key(upstream: &str) -> String {
    match upstream
        .strip_prefix("account:")
        .and_then(|rest| rest.split_once(':'))
    {
        Some((subscription, account)) => {
            format!("account:{subscription}:{}", account.to_lowercase())
        }
        None => upstream.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn q4b_member_key_lowercases_only_the_account() {
        assert_eq!(
            member_key("account:Acct:Me@Example.com"),
            "account:Acct:me@example.com"
        );
        assert_eq!(member_key("plain"), "plain");
        assert_eq!(member_key("account:broken"), "account:broken");
    }
}
