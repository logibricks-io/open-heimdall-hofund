//! Provider create, patch, and delete control handlers.

use std::collections::HashSet;
use std::sync::Arc;

use hofund_agent::{GatewayTarget, SidecarCheckpoint, catalog, reseat_after_provider_change};
use hofund_config::{
    Config, Route, UpstreamTarget, VisibleCatalog, edit_provider_create, edit_provider_delete,
    edit_provider_patch, load, validate_fallback_list, validate_provider_base_url,
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
    let pre_config = load(&current.text).map_err(config_failure)?;
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
    let applied = if reseat {
        let provider_id = id.to_owned();
        let pre_for_reseat = pre_config;
        ops.apply_with_sidecar_work(
            &request.expected_revision,
            edited,
            backup.clone(),
            move |ops, text| {
                let post_config = load(text).map_err(config_failure)?;
                let matched =
                    routes_newly_losing_sendable_hops(&pre_for_reseat, &post_config, &provider_id);
                run_reseat_locked(
                    ops,
                    descriptions,
                    agent_session,
                    gateway_target,
                    &provider_id,
                    false,
                    Some(&matched),
                    None,
                    text,
                )
            },
        )
        .await?
    } else {
        ops.apply(&request.expected_revision, edited).await?
    };
    let config = load(&applied.text).map_err(config_failure)?;
    let Some(provider) = config.providers.iter().find(|provider| provider.id == id) else {
        let current = ops.get().await?;
        ops.apply(&current.disk_revision, backup).await?;
        return Err(not_found(id));
    };
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
    let pre_config = load(&current.text).map_err(config_failure)?;
    let edited = edit_provider_delete(&current.text, id).map_err(config_failure)?;
    let provider_id = id.to_owned();
    let pre_for_reseat = pre_config;
    let applied = ops
        .apply_with_sidecar_work(
            &request.expected_revision,
            edited,
            backup,
            move |ops, text| {
                let post_config = load(text).map_err(config_failure)?;
                let matched =
                    routes_newly_losing_sendable_hops(&pre_for_reseat, &post_config, &provider_id);
                run_reseat_locked(
                    ops,
                    descriptions,
                    agent_session,
                    gateway_target,
                    &provider_id,
                    false,
                    Some(&matched),
                    None,
                    text,
                )
            },
        )
        .await?;
    let mut response = ProviderDeleteResponse::default();
    response.disk_revision = applied.disk_revision;
    response.removed = id.to_owned();
    Ok(response)
}

#[allow(clippy::too_many_arguments)]
fn run_reseat_locked(
    ops: &ConfigOps,
    descriptions: &'static [&'static AgentDescription],
    configured_session: Option<hofund_agent::Session>,
    gateway_target: Option<GatewayTarget>,
    provider_id: &str,
    provider_still_present: bool,
    match_route_models: Option<&[String]>,
    fallback_route_models: Option<&[String]>,
    config_text: &str,
) -> Result<(), ControlFailure> {
    let config = Arc::new(load(config_text).map_err(config_failure)?);
    let route_ids = ops.snapshot_route_models();
    let visible_catalog = VisibleCatalog::build(Arc::clone(&config), &route_ids);
    let sendable: HashSet<String> = crate::routes_with_sendable_hops(config.as_ref())
        .into_iter()
        .collect();
    let target = resolve_reseat_target(gateway_target, configured_session.as_ref())?;
    let session = configured_session.unwrap_or_else(crate::agent_session);
    let matched = match_route_models
        .map(<[String]>::to_vec)
        .unwrap_or_else(|| crate::routes_without_sendable_hops(config.as_ref()));
    let fallback = fallback_route_models
        .map(<[String]>::to_vec)
        .unwrap_or_else(|| sendable_fallback_routes(config.as_ref(), provider_id));
    let catalog = catalog(descriptions).map_err(|error| internal(error.to_string()))?;
    let mut checkpoint = SidecarCheckpoint::new();
    let route_ids_for_closure = route_ids.clone();
    let agent_visible_sendable = move |agent_id: &str| {
        let (shown, _) =
            visible_catalog.models_for_agent(&route_ids_for_closure, Some(agent_id), None);
        shown
            .into_iter()
            .filter(|model| sendable.contains(model))
            .collect::<Vec<_>>()
    };
    if let Err(error) = reseat_after_provider_change(
        &catalog,
        &session,
        &target,
        provider_id,
        provider_still_present,
        &matched,
        &fallback,
        agent_visible_sendable,
        &mut checkpoint,
    ) {
        if let Err(restore) = checkpoint.restore() {
            return Err(internal(restore.to_string()));
        }
        return Err(agent_reseat_failure(error, &target, config_text));
    }
    Ok(())
}

fn agent_reseat_failure(
    error: hofund_agent::AgentError,
    target: &GatewayTarget,
    config_text: &str,
) -> ControlFailure {
    let token = target.token();
    let message = crate::control_agents::scrub(&error.to_string(), &[token]);
    let message = crate::config_ops::scrub_config_message(&message, config_text);
    ControlFailure::from(ControlError::new(
        ControlErrorKind::Internal,
        message,
        "control",
    ))
}

pub(crate) fn routes_newly_losing_sendable_hops(
    before: &Config,
    after: &Config,
    provider_id: &str,
) -> Vec<String> {
    let before_sendable = crate::routes_with_sendable_hops(before);
    let after_sendable = crate::routes_with_sendable_hops(after);
    before
        .routes
        .iter()
        .filter(|route| route_uses_provider(before, route, provider_id))
        .filter(|route| before_sendable.contains(&route.model))
        .filter(|route| !after_sendable.contains(&route.model))
        .map(|route| route.model.clone())
        .collect()
}

fn sendable_fallback_routes(config: &Config, provider_id: &str) -> Vec<String> {
    let sendable = crate::routes_with_sendable_hops(config);
    config
        .routes
        .iter()
        .filter(|route| !route_uses_provider(config, route, provider_id))
        .filter(|route| sendable.contains(&route.model))
        .map(|route| route.model.clone())
        .collect()
}

fn resolve_reseat_target(
    gateway_target: Option<GatewayTarget>,
    configured_session: Option<&hofund_agent::Session>,
) -> Result<GatewayTarget, ControlFailure> {
    match gateway_target {
        Some(target) => Ok(target),
        None if configured_session.is_some() => Err(unprocessable(
            "agent",
            "agent reseat requires a configured gateway target",
        )),
        None => Err(unprocessable(
            "agent",
            "agent reseat requires a configured gateway target",
        )),
    }
}

fn route_uses_provider(config: &Config, route: &Route, provider_id: &str) -> bool {
    let mut seen = HashSet::new();
    route_model_uses_provider(config, &route.model, provider_id, &mut seen)
}

fn route_model_uses_provider(
    config: &Config,
    model: &str,
    provider_id: &str,
    seen: &mut HashSet<String>,
) -> bool {
    if !seen.insert(model.to_owned()) {
        return false;
    }
    let Some(route) = config.routes.iter().find(|route| route.model == model) else {
        return false;
    };
    for upstream in &route.upstreams {
        match &upstream.target {
            UpstreamTarget::Provider(id) if id == provider_id => return true,
            UpstreamTarget::Group(group_id) => {
                let group_model = format!("group/{group_id}");
                if route_model_uses_provider(config, &group_model, provider_id, seen) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
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

#[cfg(test)]
mod tests {
    use hofund_config::{Config, load};

    use hofund_agent::{AgentError, GatewayTarget};

    use super::{
        agent_reseat_failure, route_uses_provider, routes_newly_losing_sendable_hops,
        sendable_fallback_routes,
    };

    fn route_models_using_provider(config: &Config, provider_id: &str) -> Vec<String> {
        config
            .routes
            .iter()
            .filter(|route| route_uses_provider(config, route, provider_id))
            .map(|route| route.model.clone())
            .collect()
    }

    const RESEAT_FIXTURE: &str = "\
[[providers]]\nid = \"dead-off\"\nenabled = false\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1/v1\"\napi_key = \"k\"\n\
[[providers]]\nid = \"live\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1/v1\"\napi_key = \"k\"\n\
[[providers]]\nid = \"spare\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1/v1\"\napi_key = \"k\"\n\
[[routes]]\nmodel = \"dead-only\"\nupstreams = [\"dead-off\"]\n\
[[routes]]\nmodel = \"on-spare\"\nupstreams = [\"spare\"]\n\
[[routes]]\nmodel = \"safe\"\nupstreams = [\"live\"]\n";

    #[test]
    fn r7a_disable_reseat_matches_only_routes_newly_losing_sendable_hops() {
        let before = load(RESEAT_FIXTURE).expect("before");
        let mut after = before.clone();
        let spare = after
            .providers
            .iter_mut()
            .find(|provider| provider.id == "spare")
            .expect("spare");
        spare.enabled = false;
        let matched = routes_newly_losing_sendable_hops(&before, &after, "spare");
        assert_eq!(matched, ["on-spare"]);
        let fallback = sendable_fallback_routes(&after, "spare");
        assert_eq!(fallback, ["safe"]);
    }

    #[test]
    fn r7a_fallback_routes_exclude_routes_whose_upstream_still_uses_provider() {
        let text = "\
[[providers]]\nid = \"a\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1/v1\"\napi_key = \"k\"\n\
[[providers]]\nid = \"b\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1/v1\"\napi_key = \"k\"\n\
[[routes]]\nmodel = \"on-a\"\nupstreams = [\"a\"]\n\
[[routes]]\nmodel = \"alias-a\"\nupstreams = [{ provider = \"a\", model = \"m\" }]\n\
[[routes]]\nmodel = \"safe\"\nupstreams = [\"b\"]\n";
        let config = load(text).expect("config");
        let affected = route_models_using_provider(&config, "a");
        assert_eq!(affected, ["on-a", "alias-a"]);
        let fallback = sendable_fallback_routes(&config, "a");
        assert_eq!(fallback, ["safe"]);
    }

    #[test]
    fn r7a_delete_reseat_matches_only_routes_newly_losing_sendable_hops() {
        use hofund_config::edit_provider_delete;

        let before_text = "\
[[providers]]\nid = \"a\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1/v1\"\napi_key = \"k\"\n\
[[providers]]\nid = \"b\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1/v1\"\napi_key = \"k\"\n\
[[routes]]\nmodel = \"group/pair\"\nrouting = \"order\"\n\
upstreams = [{ provider = \"a\", model = \"m\" }, { provider = \"b\", model = \"m\" }]\n\
[[routes]]\nmodel = \"on-b\"\nupstreams = [\"b\"]\n\
[[routes]]\nmodel = \"safe\"\nupstreams = [\"a\"]\n";
        let before = load(before_text).expect("before");
        let after = load(&edit_provider_delete(before_text, "b").expect("delete")).expect("after");
        let matched = routes_newly_losing_sendable_hops(&before, &after, "b");
        assert_eq!(matched, ["on-b"]);
        assert!(
            crate::routes_with_sendable_hops(&after).contains("group/pair"),
            "group must remain sendable after one member is deleted"
        );
    }

    #[test]
    fn r7a_reseat_failure_scrubs_bearer_token_from_response() {
        let token = "sk-secret-bearer-token-value";
        let error = AgentError::Rejected(format!("connect failed with bearer {token}"));
        let target = GatewayTarget::new("http://127.0.0.1:3425/", token, Vec::new());
        let failure = agent_reseat_failure(error, &target, "");
        let body = failure.error.message;
        assert!(!body.contains(token), "response leaked token: {body}");
    }
}
