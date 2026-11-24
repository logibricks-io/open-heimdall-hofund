//! Effective list price lookup from provider model lists.

use std::sync::Arc;

use hofund_model_catalog::{
    EffectivePricer, ListPriceLookup, ModelCatalog, Price, ProviderCatalogs,
};

use crate::ModelsHost;
use crate::runtime::Runtime;

/// Builds the gateway pricer from config, catalog, and cached model lists.
/// Model families whose one-hour cache write costs twice the input price.
const DOUBLED_ONE_HOUR: &[&str] = &["claude"];

pub(crate) fn effective_pricer(
    settings: hofund_config::UsageSettings,
    catalog: Arc<ModelCatalog>,
    models: Arc<ModelsHost>,
    runtime: Arc<Runtime>,
) -> EffectivePricer {
    EffectivePricer::new(settings, catalog, Arc::new(ModelsList { models, runtime }))
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
    runtime: Arc<Runtime>,
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
        let provider = self.runtime.published_provider(provider_id)?;
        self.models.price_catalogs(&provider)
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::Arc;
    use std::time::Duration;

    use hofund_config::{CatalogSettings, UsageSettings};
    use hofund_model_catalog::{
        CacheFile, CatalogFetcher, FetchResponse, ModelCatalog, parse_catalog,
    };
    use hofund_provider::ModelPlan;

    use crate::ModelsHost;
    use crate::registry::RegistryAdapter;
    use crate::runtime::Runtime;

    struct Offline;

    impl CatalogFetcher for Offline {
        fn fetch(&self, _url: &str, _if_none_match: Option<&str>) -> io::Result<FetchResponse> {
            Err(io::Error::other("offline"))
        }
    }

    #[test]
    fn d264_pricer_follows_a_provider_added_after_startup() {
        let directory = tempfile::tempdir().expect("catalog directory");
        let parsed = parse_catalog(include_bytes!(
            "../../model-catalog/tests/fixtures/d264_membership_catalog.json"
        ))
        .expect("fixture catalog");
        CacheFile::beside_data_root(directory.path())
            .write(None, &parsed)
            .expect("seed catalog");
        let catalog = ModelCatalog::open(
            directory.path(),
            CatalogSettings::default(),
            Arc::new(Offline),
        )
        .expect("open catalog");
        let runtime = Runtime::new(
            Arc::new(RegistryAdapter),
            &directory.path().join("hofund.toml"),
            Some(0),
        );
        let mut models = ModelsHost::new(Duration::ZERO, Duration::ZERO).with_model_plan(
            "added",
            ModelPlan {
                catalogs: vec!["a-plan-coding-plan".to_owned(), "acme".to_owned()],
                membership_catalogs: vec!["a-plan-coding-plan".to_owned()],
                ..ModelPlan::default()
            },
        );
        runtime
            .initial_snapshot(
                &hofund_config::load("").expect("initial config"),
                Some(&mut models),
            )
            .expect("initial snapshot");
        let models = Arc::new(models);
        runtime.attach_models(Arc::clone(&models));
        let pricer = super::effective_pricer(
            UsageSettings::default(),
            catalog,
            Arc::clone(&models),
            Arc::clone(&runtime),
        );

        let updated = hofund_config::load(
            "[[providers]]\nid = \"added\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:3425/v1\"\ncatalog = \"a-plan-coding-plan, acme\"\n",
        )
        .expect("updated config");
        runtime.publish_candidate(&updated).expect("publish added");

        assert!(models.cached("added").models.is_empty());
        let established = pricer
            .resolve("added", "established", None, false)
            .expect("metered fallback after publication");
        assert_eq!(established.input, 1.19516);
        assert_eq!(established.output, 4.18306);
        assert_eq!(established.cache_read, 0.29879);
        assert!(pricer.resolve("added", "latest", None, false).is_none());
    }

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
