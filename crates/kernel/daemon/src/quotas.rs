//! The daemon's quota reports, shared by the model gateway's quota routes
//! and the control API behind `hofund quota`.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::future::BoxFuture;
use hofund_control_proto::{
    Quota, QuotaHistory, QuotaHistoryQuery, QuotaHistoryResponse, QuotaQuery, QuotaRefreshRequest,
    QuotaRefreshResponse, QuotaResponse,
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
    http: reqwest::Client,
}

impl DaemonQuotas {
    pub(crate) fn new(runtime: Arc<Runtime>, usage: Arc<Ledger>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(8))
            .build()
            .expect("remote quota client");
        Self {
            runtime,
            usage,
            http,
        }
    }

    async fn decorate(mut data: Vec<Quota>, usage: Arc<Ledger>, runtime: &Runtime) -> Vec<Quota> {
        let now = runtime.quota_now();
        let served = served_since(usage, now).await;
        apply_last_served(&mut data, &served, now);
        crate::credits_modes::decorate_quotas(&mut data, runtime.credits_modes());
        if let Some(config) = runtime.config_snapshot() {
            runtime
                .remote_quotas()
                .merge(&config, runtime.remote_http(), &mut data)
                .await;
        }
        data
    }

    /// The report with standings; a kept query never asks a vendor.
    pub(crate) async fn response(&self, query: &QuotaQuery) -> QuotaResponse {
        let (mut data, standings) = self.runtime.quota_report(query.kept).await;
        data = Self::decorate(data, Arc::clone(&self.usage), &self.runtime).await;
        QuotaResponse {
            data,
            standings,
            ..QuotaResponse::default()
        }
    }

    /// Kept local cards for export to remote peers (no remote merge).
    pub(crate) async fn kept_cards(&self) -> Vec<Quota> {
        let (mut data, _) = self.runtime.quota_report(true).await;
        data.retain(|card| !card.provider.contains('/'));
        let now = self.runtime.quota_now();
        let served = served_since(Arc::clone(&self.usage), now).await;
        apply_last_served(&mut data, &served, now);
        crate::credits_modes::decorate_quotas(&mut data, self.runtime.credits_modes());
        data
    }

    /// Re-reads cards, honoring the refresh gap, then returns kept cards.
    async fn refresh_exported_cards(&self, provider: Option<String>, user: String) -> Vec<Quota> {
        match provider.as_deref() {
            Some(provider) if !provider.is_empty() => {
                let _ = self.runtime.quota_refresh_cards(provider, &user).await;
            }
            _ => {
                self.runtime.quota_refresh_all().await;
            }
        }
        self.kept_cards().await
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

    /// Re-reads one card when allowed, or returns what was already read.
    pub(crate) async fn refresh_response(
        &self,
        request: &QuotaRefreshRequest,
    ) -> QuotaRefreshResponse {
        if let Some(config) = self.runtime.config_snapshot()
            && let Some((remote_id, card)) =
                crate::remote_quotas::split_remote_card(&config, &request.provider)
        {
            let _ = self
                .runtime
                .remote_quotas()
                .refresh(&self.http, &config, &remote_id, &card, &request.user)
                .await;
            let data = Self::decorate(Vec::new(), Arc::clone(&self.usage), &self.runtime).await;
            return QuotaRefreshResponse {
                data,
                ..QuotaRefreshResponse::default()
            };
        }
        let (data, standings, throttled, last_asked_secs) = self
            .runtime
            .quota_refresh_cards(&request.provider, &request.user)
            .await;
        let data = Self::decorate(data, Arc::clone(&self.usage), &self.runtime).await;
        QuotaRefreshResponse {
            data,
            standings,
            throttled,
            last_asked_secs,
            ..QuotaRefreshResponse::default()
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

    fn kept_cards(&self) -> BoxFuture<'_, Vec<Quota>> {
        Box::pin(async move { self.kept_cards().await })
    }

    fn refresh_kept_cards(
        &self,
        provider: Option<String>,
        user: String,
    ) -> BoxFuture<'_, Vec<Quota>> {
        Box::pin(async move { self.refresh_exported_cards(provider, user).await })
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
