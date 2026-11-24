//! Effective list price lookup from provider model lists.

use std::collections::HashMap;
use std::sync::Arc;

use hofund_model_catalog::{
    EffectivePricer, ListPriceLookup, ModelCatalog, Price, ProviderCatalogs,
};

use crate::ModelsHost;

/// Builds the gateway pricer from config, catalog, and cached model lists.
/// Model families whose one-hour cache write costs twice the input price.
const DOUBLED_ONE_HOUR: &[&str] = &["claude"];

pub(crate) fn effective_pricer(
    settings: hofund_config::UsageSettings,
    catalog: Arc<ModelCatalog>,
    models: Arc<ModelsHost>,
    config: &hofund_config::Config,
) -> EffectivePricer {
    let provider_catalogs = config
        .providers
        .iter()
        .filter_map(|provider| {
            models
                .price_catalogs(provider)
                .map(|catalogs| (provider.id.clone(), catalogs))
        })
        .collect();
    EffectivePricer::new(
        settings,
        catalog,
        Arc::new(ModelsList {
            models,
            provider_catalogs,
        }),
    )
    .with_doubled_one_hour_models(DOUBLED_ONE_HOUR.iter().map(|id| (*id).to_owned()).collect())
    .with_id_aliases(role_aliases())
}

fn role_aliases() -> Vec<(String, String)> {
    hofund_plugin_sdk::orchestrator_price_ids()
        .iter()
        .map(|(from, to)| ((*from).to_owned(), (*to).to_owned()))
        .collect()
}

struct ModelsList {
    models: Arc<ModelsHost>,
    provider_catalogs: HashMap<String, ProviderCatalogs>,
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

    fn provider_catalogs(&self, provider_id: &str) -> Option<ProviderCatalogs> {
        self.provider_catalogs.get(provider_id).cloned()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn d222_orchestrator_aliases_are_the_named_models() {
        let aliases = super::role_aliases();
        assert!(
            aliases
                .iter()
                .any(|(from, to)| { from == "gemini-pro-orchestrator" && to == "gemini-4-carbon" })
        );
        assert!(aliases.iter().any(|(from, to)| {
            from == "gemini-pro-orchestrator-carbon" && to == "gemini-4-carbon"
        }));
        assert!(aliases.iter().any(|(from, to)| {
            from == "gemini-pro-orchestrator-argon" && to == "gemini-4-argon"
        }));
        assert!(
            aliases.iter().any(|(from, to)| {
                from == "gemini-flash-worker-agy" && to == "gemini-4-hydrogen"
            })
        );
        assert!(
            aliases
                .iter()
                .all(|(from, _)| *from != "gemini-pro-orchestrator-neon")
        );
    }
}
