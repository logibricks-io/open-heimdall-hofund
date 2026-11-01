//! Fresh reads of provider key balances.

use std::path::Path;
use std::sync::Arc;

use futures_util::StreamExt;
use hofund_config::{Config, Provider};
use hofund_control_proto::{Quota, QuotaKind, QuotaSpan};
use hofund_plugin_api::{
    BalanceCard, BalanceQuery, BalanceSupport, BalanceWallet, HostContext, PluginError,
    PluginErrorKind, PluginId, PlanWindow,
};
use hofund_plugin_host::HttpLimits;
use hofund_provider::balance::{BalanceOptions, BalanceReader};
use hofund_provider::{Credential, Endpoint};
use hofund_router::quota::{QuotaStore, WindowInfo};

use crate::provider_keys::{self, ResolvedKey};
use crate::{DaemonError, DaemonRegistry, beside, show};

const READS_AT_ONCE: usize = 8;

enum JobKind {
    Plugin(BalanceQuery),
    Custom {
        endpoint: Endpoint,
        options: BalanceOptions,
    },
}

struct Job {
    kind: JobKind,
    card: Quota,
    support: BalanceSupport,
}

/// Reads every supported balance in `config` and records the results.
pub(crate) async fn poll(
    config: &Config,
    path: &Path,
    max_idle_per_host: Option<usize>,
    registry: &Arc<dyn DaemonRegistry>,
    quotas: &Arc<QuotaStore>,
) {
    let plugin = registry.balance_quota();
    let root = beside(path, ".hofund");
    let limits = crate::http_limits(config, max_idle_per_host);
    for provider in &config.providers {
        let Ok(keys) = provider_keys::resolve(provider) else {
            continue;
        };
        let enabled = keys
            .into_iter()
            .filter(|key| key.entry.on)
            .collect::<Vec<_>>();
        let several = enabled.len() > 1;
        let mut jobs = Vec::new();
        for key in enabled {
            match job(provider, &key, several, registry, limits) {
                Ok(Some(job)) => jobs.push(job),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(provider.id = provider.id, %error, "balance query could not be built");
                }
            }
        }
        let mut readings = futures_util::stream::iter(jobs.into_iter().enumerate().map(
            |(index, job)| {
                let plugin = plugin.clone();
                let root = root.clone();
                async move {
                    let result = match &job.kind {
                        JobKind::Plugin(query) => {
                            let Some(reader) = &plugin else {
                                return (
                                    index,
                                    job,
                                    Err(PluginError::internal("balance reader missing")),
                                );
                            };
                            let host = match balance_host(&root, limits, query) {
                                Ok(host) => host,
                                Err(error) => return (index, job, Err(error)),
                            };
                            reader.read(query, &host).await
                        }
                        JobKind::Custom { endpoint, options } => {
                            let endpoint = endpoint.clone();
                            let options = options.clone();
                            match BalanceReader::new(endpoint, options) {
                                Ok(reader) => match reader.read().await {
                                    Ok(Some(report)) => Ok(Some(report_to_card(report))),
                                    Ok(None) => Ok(None),
                                    Err(error) => Err(PluginError::internal(error.message)),
                                },
                                Err(error) => Err(PluginError::internal(error.message)),
                            }
                        }
                    };
                    (index, job, result)
                }
            },
        ))
        .buffer_unordered(READS_AT_ONCE)
        .collect::<Vec<_>>()
        .await;
        readings.sort_by_key(|(index, _, _)| *index);
        for (_, job, result) in readings {
            record(quotas, job, result);
        }
        quotas.share_provider_balances(&provider.id);
    }
}

fn balance_host(
    root: &Path,
    limits: HttpLimits,
    query: &BalanceQuery,
) -> Result<HostContext, PluginError> {
    hofund_plugin_host::runtime_host(
        PluginId::from_static("plan-quota"),
        query.api_key(),
        root,
        limits,
    )
    .map_err(|error| PluginError::internal(error.to_string()))
}

fn job(
    provider: &Provider,
    key: &ResolvedKey,
    several: bool,
    registry: &Arc<dyn DaemonRegistry>,
    limits: HttpLimits,
) -> Result<Option<Job>, DaemonError> {
    let secret = hofund_secrets::resolve(&key.entry.key).map_err(show)?;
    let bases = bases(provider, key.entry.plan.as_deref(), registry)?;
    let balance = &provider.balance;
    let mut query = BalanceQuery::new(bases.clone(), secret.clone()).with_balance_settings(
        balance.url.clone(),
        balance.path.clone(),
        balance.token.clone(),
        balance.method.clone(),
    );
    query = query.with_headers(provider_headers(provider, &secret));
    let plugin = registry.balance_quota();
    let custom = !balance.url.trim().is_empty();
    let support = if custom {
        BalanceSupport::Certain
    } else {
        plugin
            .as_ref()
            .map(|reader| reader.support(&query))
            .unwrap_or(BalanceSupport::Unsupported)
    };
    if support == BalanceSupport::Unsupported {
        return Ok(None);
    }
    let alias = if several {
        if key.entry.name.is_empty() {
            provider_keys::masked(&secret)
        } else {
            key.entry.name.clone()
        }
    } else {
        String::new()
    };
    let user = if several {
        key.id.clone()
    } else {
        String::new()
    };
    let member = if several {
        provider_keys::built_id(&provider.id, &key.id)
    } else {
        provider.id.clone()
    };
    let mut card = Quota {
        provider: provider.id.clone(),
        name: provider.id.clone(),
        kind: QuotaKind::Balance,
        user,
        alias,
        ..Quota::default()
    };
    card.member = member;
    let kind = if custom {
        let endpoint = balance_endpoint(provider, &secret, limits)?;
        JobKind::Custom {
            endpoint,
            options: BalanceOptions {
                url: balance.url.clone(),
                path: balance.path.clone(),
                token: balance.token.clone(),
                method: balance.method.clone(),
            },
        }
    } else {
        JobKind::Plugin(query)
    };
    Ok(Some(Job {
        kind,
        card,
        support,
    }))
}

fn provider_headers(provider: &Provider, secret: &str) -> Vec<(String, String)> {
    provider
        .headers
        .iter()
        .map(|(name, value)| {
            (
                name.clone(),
                value.replace("{key}", secret).replace("{token}", secret),
            )
        })
        .collect()
}

fn balance_endpoint(
    provider: &Provider,
    secret: &str,
    limits: HttpLimits,
) -> Result<Endpoint, DaemonError> {
    let mut endpoint = Endpoint::new(&provider.id, provider.base_url.clone());
    endpoint.api_key = secret.to_owned();
    endpoint.credential = Credential::Bearer;
    endpoint.timeout = limits.connect();
    endpoint.headers = provider
        .headers
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    Ok(endpoint)
}

fn bases(
    provider: &Provider,
    key_plan: Option<&str>,
    registry: &Arc<dyn DaemonRegistry>,
) -> Result<Vec<String>, DaemonError> {
    if !provider.base_url.trim().is_empty() {
        return Ok(vec![provider.base_url.clone()]);
    }
    let Some(preset) = provider.preset.as_deref() else {
        return Ok(Vec::new());
    };
    let Some((compiled, alias_region)) = registry.find_preset(preset) else {
        return Ok(Vec::new());
    };
    let region = match key_plan.filter(|plan| !plan.is_empty()) {
        Some(plan) => Some(plan),
        None => crate::option_present(provider, "region")?,
    }
    .or(alias_region);
    let region = crate::registry::resolve_region(compiled, region).map_err(show)?;
    let plan = crate::registry::model_plan_of(compiled, region);
    Ok([plan.chat, plan.anthropic, plan.responses]
        .into_iter()
        .filter(|base| !base.is_empty())
        .collect())
}

fn report_to_card(report: hofund_provider::balance::BalanceReport) -> BalanceCard {
    let balance = report.balance.as_ref();
    let text = balance.map(|balance| balance.text.clone()).unwrap_or_default();
    let wallets = balance
        .map(|balance| {
            balance
                .wallets
                .iter()
                .map(|wallet| BalanceWallet::new(wallet.amount, wallet.unit.clone()))
                .collect()
        })
        .unwrap_or_default();
    let mut card = BalanceCard::pack(
        text,
        Vec::new(),
        Vec::new(),
        wallets,
        report
            .windows
            .iter()
            .map(plan_window_from_balance)
            .collect(),
        report
            .plan
            .as_ref()
            .map(|plan| plan.name.clone())
            .unwrap_or_default(),
        report
            .plan
            .as_ref()
            .map(|plan| plan.windows.iter().map(plan_window_from_balance).collect())
            .unwrap_or_default(),
        report.plan.as_ref().and_then(|plan| plan.until),
        report
            .plan
            .as_ref()
            .map(|plan| plan.renew.clone())
            .unwrap_or_default(),
        report.account_wide,
    );
    if let Some(balance) = balance {
        let parts = balance
            .parts
            .iter()
            .map(|part| {
                hofund_plugin_api::BalancePart::new(part.text.clone())
                    .with_label(part.label.clone())
            })
            .collect();
        let split = balance
            .split
            .iter()
            .map(|part| {
                hofund_plugin_api::BalancePart::new(part.text.clone())
                    .with_label(part.label.clone())
            })
            .collect();
        card = card.with_parts(parts).with_split(split);
    }
    card
}

fn plan_window_from_balance(window: &hofund_provider::balance::BalanceWindow) -> PlanWindow {
    let mut plan = PlanWindow::new(window.name.clone(), window.used)
        .with_count(window.amount, window.limit, window.unit.clone())
        .with_display(window.display.clone())
        .with_optional_reset(window.resets_at)
        .with_blocking(window.blocking);
    if let Some(span) = window.span {
        plan = plan.with_span(span);
    }
    plan
}

fn record(quotas: &QuotaStore, job: Job, result: Result<Option<BalanceCard>, PluginError>) {
    match result {
        Ok(Some(card)) => {
            if !card.error().is_empty() {
                record_failure(quotas, &job, job.card.clone(), card.error().to_owned(), true);
                return;
            }
            let mut quota = job.card.clone();
            quota.balance = card.text().to_owned();
            quota.until = card.until().map(hofund_router::quota::quota_time);
            quota.renew = card.renew().to_owned();
            if !card.plan().is_empty() {
                quota.plan = card.plan().to_owned();
            }
            let mut windows = card
                .windows()
                .iter()
                .chain(card.plan_windows().iter())
                .map(window_info)
                .collect::<Vec<_>>();
            for wallet in card.wallets() {
                windows.push(wallet_info(wallet));
            }
            if quota.balance.is_empty() && windows.is_empty() {
                return;
            }
            quotas.record_success(quota, windows, quotas.now());
        }
        Ok(None) => {}
        Err(error) => {
            let mut card = job.card.clone();
            card.error = error.to_string();
            record_failure(quotas, &job, card, error.to_string(), transient_error(&error));
        }
    }
}

fn record_failure(
    quotas: &QuotaStore,
    job: &Job,
    mut card: Quota,
    error: String,
    transient: bool,
) {
    if job.support == BalanceSupport::Possible && !quotas.has_reading(&card) {
        return;
    }
    card.error = error;
    quotas.record_failure(card, transient);
}

fn window_info(window: &PlanWindow) -> WindowInfo {
    let used = window.used();
    WindowInfo {
        span: QuotaSpan {
            name: window.name().to_owned(),
            used,
            remaining: (100.0 - used).max(0.0),
            resets_at: window.resets_at().map(hofund_router::quota::quota_time),
            display: window.display().to_owned(),
            amount: window.amount(),
            limit: window.limit(),
            unit: window.unit().to_owned(),
            ..QuotaSpan::default()
        },
        blocking: window.blocking(),
        scope_model: None,
        window_span: window.span(),
        reset_after: None,
    }
}

fn wallet_info(wallet: &BalanceWallet) -> WindowInfo {
    let mut span = QuotaSpan {
        name: wallet.name().to_owned(),
        unit: wallet.unit().to_owned(),
        ..QuotaSpan::default()
    };
    span.balance = Some(wallet.amount());
    WindowInfo {
        span,
        blocking: true,
        scope_model: None,
        window_span: None,
        reset_after: None,
    }
}

fn transient_error(error: &PluginError) -> bool {
    matches!(
        error.kind(),
        PluginErrorKind::Connect
            | PluginErrorKind::Timeout
            | PluginErrorKind::Transport
            | PluginErrorKind::Interrupted
            | PluginErrorKind::Io
            | PluginErrorKind::Internal
            | PluginErrorKind::Quota
    ) || error
        .status()
        .is_some_and(|status| status == 429 || status >= 500)
}
