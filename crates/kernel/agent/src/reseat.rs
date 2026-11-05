//! Moves agents off route models a disabled or removed provider no longer serves.

use crate::{AgentError, Catalog, GatewayTarget, Session};

/// Rewrites agent model settings that pointed at `provider_id`.
///
/// When the provider still exists but is disabled, or was removed entirely,
/// each detected agent that used `provider_id/…` or a served ref on that provider
/// is moved to another model on the same provider when one remains in
/// `served_refs`, otherwise to the first fallback route model, or cleared when
/// none fit.
///
/// Returns agent ids that were touched.
pub fn reseat_after_provider_change(
    catalog: &Catalog,
    session: &Session,
    target: &GatewayTarget,
    provider_id: &str,
    provider_still_present: bool,
    served_refs: &[String],
    fallback_routes: &[String],
) -> Result<Vec<String>, AgentError> {
    let provider_id = provider_id.trim();
    let prefix = format!("{provider_id}/");
    let same_provider: Vec<String> = if provider_still_present {
        served_refs
            .iter()
            .filter(|model| model.starts_with(&prefix) || model.as_str() == provider_id)
            .cloned()
            .collect()
    } else {
        Vec::new()
    };
    let fallback = fallback_routes
        .iter()
        .find(|model| !model.starts_with(&prefix) && model.as_str() != provider_id)
        .cloned();
    let mut moved = Vec::new();
    for adapter in catalog.adapters() {
        if !adapter.detect(session) {
            continue;
        }
        let Some(current) = adapter.configured_model(session)? else {
            continue;
        };
        let on_provider = current == provider_id
            || current.starts_with(&prefix)
            || served_refs.iter().any(|reference| reference == &current);
        if !on_provider {
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
