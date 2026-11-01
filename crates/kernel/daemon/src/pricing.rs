//! Effective list price lookup from provider model lists.

use std::sync::Arc;

use hofund_model_catalog::{EffectivePricer, ListPriceLookup, ModelCatalog, Price};

use crate::ModelsHost;

/// Builds the gateway pricer from config, catalog, and cached model lists.
pub(crate) fn effective_pricer(
    settings: hofund_config::UsageSettings,
    catalog: Arc<ModelCatalog>,
    models: Arc<ModelsHost>,
) -> EffectivePricer {
    EffectivePricer::new(settings, catalog, Arc::new(ModelsList { models }))
}

struct ModelsList {
    models: Arc<ModelsHost>,
}

impl ListPriceLookup for ModelsList {
    fn list_price(&self, provider_id: &str, model: &str) -> Option<Price> {
        let listed = super::listed_model(self.models.cached(provider_id).models, model)?;
        let (input, output, cache_read) = listed.prices();
        let (input, output) = (input?, output?);
        Some(Price {
            input: input as f64 / 1_000_000.0,
            output: output as f64 / 1_000_000.0,
            cache_read: cache_read.unwrap_or(0) as f64 / 1_000_000.0,
            cache_write: 0.0,
            cache_write_1h: 0.0,
        })
    }
}
