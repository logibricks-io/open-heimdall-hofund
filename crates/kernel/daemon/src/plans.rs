//! Fresh reads of plans bought with provider keys.

use std::path::Path;
use std::sync::Arc;

use futures_util::StreamExt;
use hofund_config::{Config, Provider};
use hofund_control_proto::{Quota, QuotaKind, QuotaSpan};
use hofund_plugin_api::{
    HostContext, PlanCard, PlanQuery, PlanSupport, PluginError, PluginErrorKind, PluginId,
};
use hofund_plugin_host::HttpLimits;
use hofund_router::quota::{QuotaStore, WindowInfo};

use crate::provider_keys::{self, ResolvedKey};
use crate::{DaemonError, DaemonRegistry, RawValue, beside, raw_value, show};

const READS_AT_ONCE: usize = 8;

struct Job {
    query: PlanQuery,
    card: Quota,
    support: PlanSupport,
}

/// Reads every supported plan in `config` and records the results in provider
/// and key order. Kept reports never call this module.
pub(crate) async fn poll(
    config: &Config,
    path: &Path,
    max_idle_per_host: Option<usize>,
    registry: &Arc<dyn DaemonRegistry>,
    quotas: &Arc<QuotaStore>,
) {
    let Some(reader) = registry.plan_quota() else {
        return;
    };
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
            match job(provider, &key, several, registry) {
                Ok(Some(job)) => jobs.push(job),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(provider.id = provider.id, %error, "plan query could not be built");
                }
            }
        }
        let reader = reader.clone();
        let mut readings = futures_util::stream::iter(jobs.into_iter().enumerate().map(
            |(index, job)| {
                let reader = reader.clone();
                let root = root.clone();
                async move {
                    let host = match host(&root, limits, &job) {
                        Ok(host) => host,
                        Err(error) => {
                            return (index, job, Err(error));
                        }
                    };
                    let result = reader.read(&job.query, &host).await;
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
    }
}

fn host(root: &Path, limits: HttpLimits, job: &Job) -> Result<HostContext, PluginError> {
    hofund_plugin_host::runtime_host(
        PluginId::from_static("plan-quota"),
        job.query.api_key(),
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
) -> Result<Option<Job>, DaemonError> {
    let secret = hofund_secrets::resolve(&key.entry.key).map_err(show)?;
    let bases = bases(provider, key.entry.plan.as_deref(), registry)?;
    let mut query = PlanQuery::new(bases, secret.clone());
    let access_id = option(provider, "access_key_id")?;
    let access_secret = option(provider, "secret_access_key")?;
    query = query
        .with_access_key(access_id, access_secret)
        .with_session_token(option(provider, "session_token")?)
        .with_plan_url(option(provider, "plan_quota_url")?)
        .with_team(
            option(provider, "organization")?,
            option(provider, "project")?,
        );
    let Some(reader) = registry.plan_quota() else {
        return Ok(None);
    };
    let support = reader.support(&query);
    if support == PlanSupport::Unsupported {
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
    let user = if several { key.id.clone() } else { String::new() };
    let member = if several {
        provider_keys::built_id(&provider.id, &key.id)
    } else {
        provider.id.clone()
    };
    let mut card = Quota {
        provider: provider.id.clone(),
        name: provider.id.clone(),
        kind: QuotaKind::Plan,
        user,
        alias,
        ..Quota::default()
    };
    card.member = member;
    Ok(Some(Job {
        query,
        card,
        support,
    }))
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

fn option(provider: &Provider, key: &str) -> Result<String, DaemonError> {
    let Some(value) = provider.options.get(key) else {
        return Ok(String::new());
    };
    match raw_value(
        value,
        Some(("provider", provider.id.as_str(), key)),
    )? {
        RawValue::String(value) => Ok(value),
        _ => Err(DaemonError::new(format!(
            "provider `{}` option `{key}` must be a string",
            provider.id
        ))),
    }
}

fn record(quotas: &QuotaStore, job: Job, result: Result<Vec<PlanCard>, PluginError>) {
    match result {
        Ok(cards) => {
            let ambiguous = cards.len() > 1;
            for plan in cards {
                let mut card = job.card.clone();
                card.plan = plan.plan().to_owned();
                if card.user.is_empty() {
                    card.user = plan.user().to_owned();
                }
                card.until = plan.until().map(hofund_router::quota::quota_time);
                card.renew = plan.renew().to_owned();
                card.display_only = ambiguous;
                if !plan.error().is_empty() {
                    record_failure(quotas, &job, card, plan.error().to_owned(), true);
                    continue;
                }
                let windows = plan.windows().iter().map(window_info).collect::<Vec<_>>();
                if windows.is_empty() && card.plan.is_empty() {
                    continue;
                }
                quotas.record_success(card, windows, quotas.now());
            }
        }
        Err(error) => {
            let mut card = job.card.clone();
            card.error = error.to_string();
            let transient = transient_error(&error);
            record_failure(quotas, &job, card, error.to_string(), transient);
        }
    }
}

fn record_failure(quotas: &QuotaStore, job: &Job, mut card: Quota, error: String, transient: bool) {
    if job.support == PlanSupport::Possible && !quotas.has_reading(&card) {
        return;
    }
    card.error = error;
    quotas.record_failure(card, transient);
}

fn window_info(window: &hofund_plugin_api::PlanWindow) -> WindowInfo {
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
    ) || error.status().is_some_and(|status| status == 429 || status >= 500)
}

#[cfg(test)]
mod tests;
