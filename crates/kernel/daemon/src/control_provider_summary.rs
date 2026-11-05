//! Builds secret-free provider summaries for the control plane.

use std::sync::Arc;

use hofund_config::{Config, Provider};
use hofund_control_proto::ProviderSummary;
use hofund_router::Cooldowns;

use crate::membership;
use crate::models::{Cached, ModelsHost};
use crate::{DaemonRegistry, plugin_for, protocol_of};

pub(crate) fn summarize_provider(
    provider: &Provider,
    config: &Config,
    models: &ModelsHost,
    registry: &Arc<dyn DaemonRegistry>,
    cooldowns: &Cooldowns,
) -> ProviderSummary {
    let cached = models.cached(&provider.id);
    let planned = models.planned(provider);
    let id = membership::member_id(&provider.id);
    let resting = cooldowns.cooling(&id).is_some();
    let rests = cooldowns.scoped_entries();
    let mut summary = ProviderSummary::default();
    summary.id = provider.id.clone();
    summary.name = provider.name.clone();
    summary.enabled = provider.enabled;
    summary.listed = provider.listed;
    summary.family = provider.family.clone();
    summary.base_url = provider.base_url.clone();
    summary.preset = provider.preset.clone();
    summary.ready = planned.is_ok() && !resting;
    summary.model_count = u64::try_from(cached.models.len()).unwrap_or(u64::MAX);
    summary.resting_models = rests
        .iter()
        .filter(|(rest_id, model, _)| *rest_id == id && model.is_some())
        .filter_map(|(_, model, _)| model.clone())
        .collect();
    if let Ok(planned) = planned {
        summary.kind = plugin_for(provider, &planned, registry);
        summary.protocol = protocol_of(planned.wire).to_owned();
    }
    summary.key_masked = mask_key(&provider.api_key);
    summary.list_error = list_error(&cached);
    fill_vertex_hints(provider, &mut summary);
    fill_account_row(config, provider, &mut summary);
    summary
}

fn list_error(_cached: &Cached) -> String {
    String::new()
}

fn mask_key(spec: &str) -> String {
    let trimmed = spec.trim();
    if trimmed.is_empty() || trimmed.starts_with("env:") {
        return String::new();
    }
    let tail: String = trimmed
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("…{tail}")
}

fn fill_vertex_hints(provider: &Provider, summary: &mut ProviderSummary) {
    let project = provider
        .options
        .get("project")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let location = provider
        .options
        .get("location")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    summary.impersonate_account = provider
        .options
        .get("impersonate")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_owned();
    summary.project_address_ready =
        !project.trim().is_empty() && provider.preset.as_deref() == Some("google-vertex");
    let _ = location;
}

fn fill_account_row(_config: &Config, provider: &Provider, summary: &mut ProviderSummary) {
    let _ = provider;
    summary.account_label = String::new();
    summary.signed_out = false;
}
