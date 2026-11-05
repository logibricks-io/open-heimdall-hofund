//! Moves agents off route models a disabled or removed provider no longer serves.

use std::collections::HashSet;

use crate::{AgentError, Catalog, GatewayTarget, Session, SidecarCheckpoint};

/// Rewrites agent model settings that pointed at route models served through
/// `provider_id`.
#[allow(clippy::too_many_arguments)]
pub fn reseat_after_provider_change(
    catalog: &Catalog,
    session: &Session,
    gateway_target: &GatewayTarget,
    provider_id: &str,
    provider_still_present: bool,
    match_route_models: &[String],
    sendable_fallback_routes: &[String],
    agent_visible_sendable: impl Fn(&str) -> Vec<String>,
    checkpoint: &mut SidecarCheckpoint,
) -> Result<Vec<String>, AgentError> {
    let provider_id = provider_id.trim();
    let prefix = format!("{provider_id}/");
    let same_provider: Vec<String> = if provider_still_present {
        match_route_models.to_vec()
    } else {
        Vec::new()
    };
    let mut moved = Vec::new();
    for adapter in catalog.adapters() {
        if !adapter.detect(session) {
            continue;
        }
        let Some(current) = adapter.configured_model(session)? else {
            continue;
        };
        let route_model = configured_route_model(adapter.model_prefix(), &current);
        let on_provider = route_model.as_deref() == Some(provider_id)
            || route_model
                .as_deref()
                .is_some_and(|model| model.starts_with(&prefix))
            || route_model
                .as_deref()
                .is_some_and(|model| match_route_models.iter().any(|route| route == model));
        if !on_provider {
            continue;
        }
        let agent_models = agent_visible_sendable(adapter.id());
        let target = gateway_target.clone().with_models(agent_models.clone());
        adapter.record_sidecar_paths(session, checkpoint)?;
        if agent_models.is_empty() {
            adapter.set_model(session, &target, None)?;
            moved.push(adapter.id().to_owned());
            continue;
        }
        let visible: HashSet<String> = agent_models.iter().cloned().collect();
        let fallback = sendable_fallback_routes
            .iter()
            .find(|model| visible.contains(*model))
            .map(String::as_str);
        let current_route = route_model.as_deref().unwrap_or(current.as_str());
        let replacement: Option<&str> = same_provider
            .iter()
            .find(|model| model.as_str() != current_route)
            .map(String::as_str)
            .or_else(|| same_provider.first().map(String::as_str))
            .or(fallback);
        adapter.set_model(session, &target, replacement)?;
        moved.push(adapter.id().to_owned());
    }
    Ok(moved)
}

fn configured_route_model(model_prefix: Option<&str>, configured: &str) -> Option<String> {
    if let Some(prefix) = model_prefix {
        configured
            .strip_prefix(prefix)
            .map(|model| model.to_owned())
    } else {
        Some(configured.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::configured_route_model;

    #[test]
    fn r7a_configured_route_model_strips_adapter_prefix() {
        assert_eq!(
            configured_route_model(Some("hofund/"), "hofund/on-added").as_deref(),
            Some("on-added")
        );
        assert_eq!(
            configured_route_model(None, "on-added").as_deref(),
            Some("on-added")
        );
    }
}
