//! Route model list filtering for catalog visibility (retired, deprecated, disabled).

use std::sync::Arc;

use axum::http::StatusCode;
use hofund_model_catalog::{ModelCatalog, listable_route_models};
use hofund_router::{RouteScope, Snapshot};

/// Route model ids visible on list surfaces for this caller scope.
#[must_use]
pub fn visible_route_models(
    snapshot: &Snapshot,
    scope: Option<&RouteScope>,
    catalog: &Option<Arc<ModelCatalog>>,
) -> Vec<String> {
    let routes = snapshot.load().models_scoped(scope);
    let visibility = catalog.as_ref().map(|catalog| catalog.visibility());
    listable_route_models(routes, catalog.as_deref(), visibility)
}

/// Whether a request for `model` should be refused as disabled.
#[must_use]
pub fn disabled_refusal(
    catalog: &Option<Arc<ModelCatalog>>,
    model: &str,
) -> Option<(StatusCode, String)> {
    let catalog = catalog.as_ref()?;
    let pattern = catalog.visibility().disabled_pattern(model)?;
    Some((
        StatusCode::FORBIDDEN,
        catalog.visibility().disabled_message(model, &pattern),
    ))
}
