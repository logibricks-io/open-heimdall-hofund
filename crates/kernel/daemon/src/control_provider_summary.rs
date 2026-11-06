//! Builds secret-free provider summaries for the control plane.

use std::sync::Arc;

use hofund_config::{Config, Provider};
use hofund_control_proto::ProviderSummary;
use hofund_router::Cooldowns;

use crate::membership;
use crate::models::{Cached, ModelsHost};
use crate::provider_keys;
use crate::{DaemonRegistry, plugin_for, protocol_of};

pub(crate) fn summarize_provider(
    provider: &Provider,
    config: &Config,
    models: &Arc<ModelsHost>,
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
    summary.key_masked = provider_keys::masked(&provider.api_key);
    summary.list_error = list_error(&cached);
    fill_vertex_hints(provider, &mut summary);
    fill_account_row(config, provider, &mut summary);
    summary
}

fn list_error(_cached: &Cached) -> String {
    String::new()
}

fn fill_vertex_hints(provider: &Provider, summary: &mut ProviderSummary) {
    if provider.preset.as_deref() != Some("google-vertex") {
        return;
    }
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
    summary.project_address_ready = vertex_address_formable(project, location);
}

fn vertex_address_formable(project: &str, location: &str) -> bool {
    let project = project.trim().to_ascii_lowercase();
    let mut location = location.trim().to_ascii_lowercase();
    if project.is_empty() || !project_ok(&project) {
        return false;
    }
    if location.is_empty() {
        location = "global".to_owned();
    }
    location_ok(&location)
}

fn project_ok(project: &str) -> bool {
    let bytes = project.as_bytes();
    let [first, .., last] = bytes else {
        return false;
    };
    project_char(*first) && project_char(*last) && bytes.iter().all(|byte| project_byte(*byte))
}

fn project_char(byte: u8) -> bool {
    byte.is_ascii_lowercase() || byte.is_ascii_digit()
}

fn project_byte(byte: u8) -> bool {
    project_char(byte) || matches!(byte, b'.' | b':' | b'-')
}

fn location_ok(location: &str) -> bool {
    let mut parts = location.split('-');
    let Some(first) = parts.next() else {
        return false;
    };
    if first.is_empty() || !first.bytes().all(|byte| byte.is_ascii_lowercase()) {
        return false;
    }
    parts.all(|part| {
        !part.is_empty()
            && part
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    })
}

fn fill_account_row(_config: &Config, provider: &Provider, summary: &mut ProviderSummary) {
    let _ = provider;
    summary.account_label = String::new();
    summary.signed_out = false;
}
