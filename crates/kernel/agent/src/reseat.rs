//! Moves agents off route models a disabled or removed provider no longer serves.

use crate::{AgentError, Catalog, GatewayTarget, Session};

/// Rewrites agent model settings that pointed at route models served through
/// `provider_id`.
///
/// Agents store routed model ids (optionally prefixed; see `model_prefix`).
/// `affected_route_models` lists those route ids whose upstreams still use the
/// provider. `fallback_route_models` lists route ids that do not.
///
/// Returns agent ids that were touched.
pub fn reseat_after_provider_change(
    catalog: &Catalog,
    session: &Session,
    target: &GatewayTarget,
    provider_id: &str,
    provider_still_present: bool,
    affected_route_models: &[String],
    fallback_route_models: &[String],
) -> Result<Vec<String>, AgentError> {
    let provider_id = provider_id.trim();
    let prefix = format!("{provider_id}/");
    let same_provider: Vec<String> = if provider_still_present {
        affected_route_models.to_vec()
    } else {
        Vec::new()
    };
    let fallback = fallback_route_models.first().cloned();
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
                .is_some_and(|model| affected_route_models.iter().any(|route| route == model));
        if !on_provider {
            continue;
        }
        let current_route = route_model.as_deref().unwrap_or(current.as_str());
        let replacement = same_provider
            .iter()
            .find(|model| model.as_str() != current_route)
            .or(same_provider.first())
            .or(fallback.as_ref())
            .map(String::as_str);
        adapter.set_model(session, target, replacement)?;
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
