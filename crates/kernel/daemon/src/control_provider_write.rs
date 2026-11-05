//! Provider create, patch, and delete control handlers.

use std::sync::Arc;

use hofund_agent::{GatewayTarget, catalog, reseat_after_provider_change};
use hofund_config::{
    edit_provider_create, edit_provider_delete, edit_provider_patch, load, validate_fallback_list,
    validate_provider_base_url,
};
use hofund_control_proto::{
    ControlError, ControlErrorKind, ProviderCreateRequest, ProviderDeleteRequest,
    ProviderDeleteResponse, ProviderPatchRequest, ProviderWriteResponse,
};
use hofund_gateway::control::ControlFailure;
use hofund_plugin_api::AgentDescription;

use crate::DaemonRegistry;
use crate::config_ops::ConfigOps;
use crate::control_presets::option_keys_for;
use crate::control_provider_summary::summarize_provider;
use crate::models::ModelsHost;

pub(crate) async fn create_provider(
    ops: &Arc<ConfigOps>,
    models: &Arc<ModelsHost>,
    registry: &Arc<dyn DaemonRegistry>,
    cooldowns: &Arc<hofund_router::Cooldowns>,
    request: ProviderCreateRequest,
) -> Result<ProviderWriteResponse, ControlFailure> {
    request.validate().map_err(ControlFailure::from)?;
    let (preset, _) = registry
        .find_preset(&request.preset)
        .ok_or_else(|| unprocessable("preset", "unknown preset"))?;
    if let Some(url) = request.base_url.as_deref() {
        validate_provider_base_url(url).map_err(config_failure)?;
    }
    let id = request
        .id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            request
                .name
                .as_deref()
                .map(hofund_config::group_id_from_name)
                .filter(|id| hofund_config::provider_id_valid(id))
                .unwrap_or_else(|| format!("provider-{}", request.preset))
        });
    let allowed = option_keys_for(preset);
    let options: Vec<(String, String)> = request
        .options
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let current = ops.get().await?;
    let edited = edit_provider_create(
        &current.text,
        &id,
        &request.preset,
        request.name.as_deref(),
        request.base_url.as_deref(),
        request.api_key.as_deref(),
        request.family.as_deref(),
        &allowed,
        &options,
    )
    .map_err(config_failure)?;
    let applied = ops.apply(&request.expected_revision, edited).await?;
    let config = load(&applied.text).map_err(config_failure)?;
    let Some(provider) = config.providers.iter().find(|provider| provider.id == id) else {
        return Err(internal("provider create did not persist".to_owned()));
    };
    Ok(write_response(
        provider, &config, models, registry, cooldowns, &applied,
    ))
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn patch_provider(
    ops: &Arc<ConfigOps>,
    models: &Arc<ModelsHost>,
    registry: &Arc<dyn DaemonRegistry>,
    cooldowns: &Arc<hofund_router::Cooldowns>,
    descriptions: &'static [&'static AgentDescription],
    agent_session: Option<hofund_agent::Session>,
    gateway_target: Option<GatewayTarget>,
    id: &str,
    request: ProviderPatchRequest,
) -> Result<ProviderWriteResponse, ControlFailure> {
    request.validate().map_err(ControlFailure::from)?;
    if let Some(fallback) = &request.fallback {
        validate_fallback_list(id, fallback).map_err(config_failure)?;
    }
    if let Some(url) = request.base_url.as_deref() {
        validate_provider_base_url(url).map_err(config_failure)?;
    }
    let current = ops.get().await?;
    if !load(&current.text)
        .map_err(config_failure)?
        .providers
        .iter()
        .any(|provider| provider.id == id)
    {
        return Err(not_found(id));
    }
    let reseat = request.enabled == Some(false);
    let backup = current.text.clone();
    let backup_revision = current.disk_revision.clone();
    let edited = edit_provider_patch(
        &current.text,
        id,
        request.enabled,
        request.listed,
        request.family.as_ref().map(|family| family.as_deref()),
        request.base_url.as_deref(),
        request.fallback.as_deref(),
    )
    .map_err(config_failure)?;
    let applied = ops.apply(&request.expected_revision, edited).await?;
    if reseat
        && let Err(error) = reseat_provider(
            descriptions,
            agent_session,
            gateway_target,
            id,
            true,
            &applied,
        )
        .await
    {
        let current = ops.get().await?;
        let _ = ops.apply(&current.disk_revision, backup.clone()).await;
        return Err(error);
    }
    let config = load(&applied.text).map_err(config_failure)?;
    let Some(provider) = config.providers.iter().find(|provider| provider.id == id) else {
        let current = ops.get().await?;
        let _ = ops.apply(&current.disk_revision, backup).await;
        return Err(not_found(id));
    };
    let _ = backup_revision;
    Ok(write_response(
        provider, &config, models, registry, cooldowns, &applied,
    ))
}

pub(crate) async fn delete_provider(
    ops: &Arc<ConfigOps>,
    descriptions: &'static [&'static AgentDescription],
    agent_session: Option<hofund_agent::Session>,
    gateway_target: Option<GatewayTarget>,
    id: &str,
    request: ProviderDeleteRequest,
) -> Result<ProviderDeleteResponse, ControlFailure> {
    request.validate().map_err(ControlFailure::from)?;
    let current = ops.get().await?;
    if !load(&current.text)
        .map_err(config_failure)?
        .providers
        .iter()
        .any(|provider| provider.id == id)
    {
        return Err(not_found(id));
    }
    let backup = current.text.clone();
    let edited = edit_provider_delete(&current.text, id).map_err(config_failure)?;
    let applied = ops.apply(&request.expected_revision, edited).await?;
    if let Err(error) = reseat_provider(
        descriptions,
        agent_session,
        gateway_target,
        id,
        false,
        &applied,
    )
    .await
    {
        let current = ops.get().await?;
        let _ = ops.apply(&current.disk_revision, backup).await?;
        return Err(error);
    }
    let mut response = ProviderDeleteResponse::default();
    response.disk_revision = applied.disk_revision;
    response.removed = id.to_owned();
    Ok(response)
}

fn write_response(
    provider: &hofund_config::Provider,
    config: &hofund_config::Config,
    models: &Arc<ModelsHost>,
    registry: &Arc<dyn DaemonRegistry>,
    cooldowns: &Arc<hofund_router::Cooldowns>,
    applied: &hofund_control_proto::ConfigResponse,
) -> ProviderWriteResponse {
    let mut response = ProviderWriteResponse::default();
    response.provider = summarize_provider(provider, config, models, registry, cooldowns);
    response.disk_revision = applied.disk_revision.clone();
    response.runtime_revision = applied.runtime_revision.clone();
    response.pending_restart = applied.pending_restart.clone();
    response
}

async fn reseat_provider(
    descriptions: &'static [&'static AgentDescription],
    session: Option<hofund_agent::Session>,
    target: Option<GatewayTarget>,
    provider_id: &str,
    provider_still_present: bool,
    applied: &hofund_control_proto::ConfigResponse,
) -> Result<(), ControlFailure> {
    let (Some(session), Some(target)) = (session, target) else {
        return Err(unprocessable(
            "agent",
            "agent reseat requires a configured agent session",
        ));
    };
    let config = load(&applied.text).map_err(config_failure)?;
    let served_refs = provider_served_refs(&config, provider_id);
    let fallback_routes: Vec<String> = config
        .routes
        .iter()
        .map(|route| route.model.clone())
        .collect();
    let provider_id = provider_id.to_owned();
    tokio::task::spawn_blocking(move || {
        let catalog = catalog(descriptions).map_err(|error| internal(error.to_string()))?;
        reseat_after_provider_change(
            &catalog,
            &session,
            &target,
            &provider_id,
            provider_still_present,
            &served_refs,
            &fallback_routes,
        )
        .map_err(|error| internal(error.to_string()))
    })
    .await
    .map_err(|_| internal("reseat task failed".to_owned()))??;
    Ok(())
}

fn provider_served_refs(config: &hofund_config::Config, provider_id: &str) -> Vec<String> {
    let mut refs = vec![provider_id.to_owned()];
    for route in &config.routes {
        for upstream in &route.upstreams {
            let reference = upstream_reference(upstream, provider_id);
            if reference.starts_with(&format!("{provider_id}/")) || reference == provider_id {
                refs.push(reference);
            }
        }
    }
    refs.sort();
    refs.dedup();
    refs
}

fn upstream_reference(upstream: &hofund_config::Upstream, provider_id: &str) -> String {
    let _ = provider_id;
    match &upstream.target {
        hofund_config::UpstreamTarget::Provider(id) => upstream
            .model
            .as_ref()
            .map(|model| format!("{id}/{model}"))
            .unwrap_or_else(|| id.clone()),
        hofund_config::UpstreamTarget::Subscription(id) => upstream
            .model
            .as_ref()
            .map(|model| format!("{id}/{model}"))
            .unwrap_or_else(|| id.clone()),
        hofund_config::UpstreamTarget::Group(id) => format!("group/{id}"),
    }
}

fn not_found(id: &str) -> ControlFailure {
    ControlFailure::from(ControlError::new(
        ControlErrorKind::NotFound,
        format!("unknown provider `{id}`"),
        "control",
    ))
}

fn unprocessable(field: &str, reason: &str) -> ControlFailure {
    let mut error = ControlError::new(ControlErrorKind::Unprocessable, reason, "control");
    error
        .details
        .push(hofund_control_proto::FieldIssue::new(field, reason, reason));
    ControlFailure::from(error)
}

fn config_failure(error: hofund_config::ConfigError) -> ControlFailure {
    ControlFailure::from(ControlError::new(
        ControlErrorKind::Unprocessable,
        error.to_string(),
        "control",
    ))
}

fn internal(message: String) -> ControlFailure {
    ControlFailure::from(ControlError::new(
        ControlErrorKind::Internal,
        message,
        "control",
    ))
}
