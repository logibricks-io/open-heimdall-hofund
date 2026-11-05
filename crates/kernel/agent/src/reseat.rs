//! Moves agents off route models a disabled or removed provider no longer serves.

use crate::{AgentError, Catalog, GatewayTarget, Session};

/// Rewrites agent model settings that pointed at `provider_id`.
///
/// When the provider still exists but is disabled, or was removed entirely,
/// each detected agent that used `provider_id/…` is moved to another model on
/// the same provider when one remains in `route_models`, otherwise to the first
/// listed route model, or cleared when none fit.
///
/// Returns agent ids that were touched.
pub fn reseat_after_provider_change(
    catalog: &Catalog,
    session: &Session,
    target: &GatewayTarget,
    provider_id: &str,
    provider_still_present: bool,
    route_models: &[String],
) -> Result<Vec<String>, AgentError> {
    let provider_id = provider_id.trim();
    let prefix = format!("{provider_id}/");
    let same_provider: Vec<String> = if provider_still_present {
        route_models
            .iter()
            .filter(|model| model.starts_with(&prefix))
            .cloned()
            .collect()
    } else {
        Vec::new()
    };
    let fallback = route_models
        .first()
        .cloned()
        .filter(|model| !model.starts_with(&prefix));
    let mut moved = Vec::new();
    for adapter in catalog.adapters() {
        if !adapter.detect(session) {
            continue;
        }
        let Some(current) = adapter.configured_model(session)? else {
            continue;
        };
        if current != provider_id && !current.starts_with(&prefix) {
            continue;
        }
        let replacement = same_provider
            .iter()
            .find(|model| **model != current)
            .or(same_provider.first())
            .or(fallback.as_ref())
            .map(String::as_str);
        adapter.set_model(session, target, replacement)?;
        moved.push(adapter.id().to_owned());
    }
    Ok(moved)
}
