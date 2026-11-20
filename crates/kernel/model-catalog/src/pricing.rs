//! Effective list prices for usage rows.

use std::sync::Arc;

use hofund_config::{ModelPriceOverride, UsageSettings};

use crate::ModelCatalog;
use crate::spec::Price;

const ANY_PROVIDER: &str = "*/";

/// Looks up a provider's live model list price.
pub trait ListPriceLookup: Send + Sync {
    /// Returns the provider catalog price for `model`, when listed.
    fn list_price(&self, provider_id: &str, model: &str) -> Option<Price>;
}

/// Resolves the effective USD list price for one answered request.
pub struct EffectivePricer {
    settings: UsageSettings,
    catalog: Arc<ModelCatalog>,
    list: Arc<dyn ListPriceLookup>,
    doubled_one_hour: Vec<String>,
    aliases: Vec<(String, String)>,
}

impl EffectivePricer {
    /// Builds a pricer from config, the models.dev cache, and provider lists.
    #[must_use]
    pub fn new(
        settings: UsageSettings,
        catalog: Arc<ModelCatalog>,
        list: Arc<dyn ListPriceLookup>,
    ) -> Self {
        Self {
            settings,
            catalog,
            list,
            doubled_one_hour: Vec::new(),
            aliases: Vec::new(),
        }
    }

    /// Model ids priced as another id. Keys and values are matched without
    /// case, after the last `/`, and an effort suffix
    /// (`-minimal`, `-low`, `-medium`, `-high`, `-xhigh`, with an optional
    /// `-extra` before it) uses the same price. Vendor names stay with the
    /// caller.
    #[must_use]
    pub fn with_id_aliases(mut self, aliases: Vec<(String, String)>) -> Self {
        self.aliases = aliases
            .into_iter()
            .map(|(from, to)| (from.to_ascii_lowercase(), to))
            .filter(|(from, to)| !from.is_empty() && !to.is_empty())
            .collect();
        self
    }

    /// Model id fragments (case-insensitive) whose one-hour cache write
    /// defaults to twice the input price when only a five-minute write price
    /// is known. Vendor names stay with the caller.
    #[must_use]
    pub fn with_doubled_one_hour_models(mut self, fragments: Vec<String>) -> Self {
        self.doubled_one_hour = fragments
            .into_iter()
            .map(|fragment| fragment.to_ascii_lowercase())
            .filter(|fragment| !fragment.is_empty())
            .collect();
        self
    }

    /// Resolves a list price for `provider_id` and `model`, optionally falling
    /// back to `served` when the requested model has no price.
    #[must_use]
    pub fn resolve(
        &self,
        provider_id: &str,
        model: &str,
        served: Option<&str>,
        provider_free: bool,
    ) -> Option<Price> {
        if provider_free {
            return Some(Price {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
                cache_write_1h: 0.0,
            });
        }
        if let Some(price) = self.custom_price(provider_id, model) {
            return Some(price);
        }
        if let Some(price) = self.list.list_price(provider_id, model) {
            return Some(enrich_from_catalog(
                self.catalog.as_ref(),
                provider_id,
                model,
                price,
                &self.doubled_one_hour,
            ));
        }
        if let Some(served) = served.filter(|served| *served != model)
            && let Some(price) = self.list.list_price(provider_id, served)
        {
            return Some(enrich_from_catalog(
                self.catalog.as_ref(),
                provider_id,
                served,
                price,
                &self.doubled_one_hour,
            ));
        }
        if let Some(price) = self.catalog_price(provider_id, model) {
            return Some(price);
        }
        if let Some(alias) = priced_alias(model, &self.aliases) {
            if let Some(price) = self.custom_price(provider_id, alias) {
                return Some(price);
            }
            if let Some(price) = self.list.list_price(provider_id, alias) {
                return Some(enrich_from_catalog(
                    self.catalog.as_ref(),
                    provider_id,
                    alias,
                    price,
                    &self.doubled_one_hour,
                ));
            }
            if let Some(price) = self.catalog_price(provider_id, alias) {
                return Some(price);
            }
        }
        if let Some(served) = served.filter(|served| *served != model)
            && let Some(price) = self.catalog_price(provider_id, served)
        {
            return Some(price);
        }
        None
    }

    fn custom_price(&self, provider_id: &str, model: &str) -> Option<Price> {
        for key in [
            format!("{provider_id}/{model}"),
            format!("{provider_id}/*"),
            format!("{ANY_PROVIDER}{model}"),
        ] {
            if let Some(custom) = self.settings.prices.get(&key) {
                return Some(price_from_override(model, custom, &self.doubled_one_hour));
            }
        }
        None
    }

    fn catalog_price(&self, provider_id: &str, model: &str) -> Option<Price> {
        self.catalog
            .resolve(&format!("{provider_id}/{model}"))
            .or_else(|| self.catalog.resolve(model))
            .and_then(|entry| entry.price.clone())
    }
}

fn price_from_override(model_id: &str, custom: &ModelPriceOverride, doubled: &[String]) -> Price {
    let mut price = Price {
        input: custom.input,
        output: custom.output,
        cache_read: custom.cache_read,
        cache_write: custom.cache_write,
        cache_write_1h: custom.cache_write_1h.unwrap_or(0.0),
    };
    one_hour_for(model_id, &mut price, doubled);
    price
}

fn priced_alias<'a>(model: &str, aliases: &'a [(String, String)]) -> Option<&'a str> {
    if aliases.is_empty() {
        return None;
    }
    let lower = model.to_ascii_lowercase();
    let bare = lower.rsplit('/').next().unwrap_or(lower.as_str());
    if let Some((_, priced)) = aliases.iter().find(|(id, _)| id == bare) {
        return Some(priced.as_str());
    }
    let (base, effort) = bare.rsplit_once('-')?;
    if !matches!(effort, "minimal" | "low" | "medium" | "high" | "xhigh") {
        return None;
    }
    let base = base.strip_suffix("-extra").unwrap_or(base);
    aliases
        .iter()
        .find(|(id, _)| id == base)
        .map(|(_, priced)| priced.as_str())
}

fn one_hour_for(model_id: &str, price: &mut Price, doubled: &[String]) {
    if price.cache_write <= 0.0 || price.cache_write_1h > 0.0 {
        return;
    }
    let model_id = model_id.to_ascii_lowercase();
    if doubled
        .iter()
        .any(|fragment| model_id.contains(fragment.as_str()))
    {
        price.cache_write_1h = 2.0 * price.input;
    }
}

fn enrich_from_catalog(
    catalog: &ModelCatalog,
    provider_id: &str,
    model: &str,
    mut price: Price,
    doubled: &[String],
) -> Price {
    if let Some(entry) = catalog
        .resolve(&format!("{provider_id}/{model}"))
        .or_else(|| catalog.resolve(model))
        && let Some(catalog_price) = &entry.price
    {
        if price.cache_write == 0.0 {
            price.cache_write = catalog_price.cache_write;
        }
        if price.cache_write_1h == 0.0 {
            price.cache_write_1h = catalog_price.cache_write_1h;
        }
        if price.cache_read == 0.0 {
            price.cache_read = catalog_price.cache_read;
        }
    }
    one_hour_for(model, &mut price, doubled);
    price
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::Arc;

    use hofund_config::{CatalogSettings, ModelPriceOverride, UsageSettings};

    use super::EffectivePricer;
    use crate::spec::Price;
    use crate::sync::{CatalogFetcher, FetchResponse};
    use crate::{ListPriceLookup, ModelCatalog};

    struct NamedList;

    impl ListPriceLookup for NamedList {
        fn list_price(&self, _provider_id: &str, model: &str) -> Option<Price> {
            (model == "price-a").then_some(Price {
                input: 4.0,
                output: 8.0,
                cache_read: 0.0,
                cache_write: 0.0,
                cache_write_1h: 0.0,
            })
        }
    }

    struct Offline;

    impl CatalogFetcher for Offline {
        fn fetch(&self, _url: &str, _if_none_match: Option<&str>) -> io::Result<FetchResponse> {
            Err(io::Error::other("offline"))
        }
    }

    fn pricer(dir: &std::path::Path, settings: UsageSettings) -> EffectivePricer {
        let catalog = ModelCatalog::open(dir, CatalogSettings::default(), Arc::new(Offline))
            .expect("catalog");
        EffectivePricer::new(settings, catalog, Arc::new(NamedList))
            .with_id_aliases(vec![("role-a".to_owned(), "price-a".to_owned())])
    }

    #[test]
    fn d222_alias_prices_the_named_model() {
        let dir = tempfile::tempdir().expect("dir");
        let priced = pricer(dir.path(), UsageSettings::default());
        let price = priced
            .resolve("prov", "Role-A", None, false)
            .expect("bare alias");
        assert_eq!(price.input, 4.0);
        let effort = priced
            .resolve("prov", "vendor/role-a-extra-high", None, false)
            .expect("effort alias");
        assert_eq!(effort.output, 8.0);
        assert!(
            priced
                .resolve("prov", "role-a-extra", None, false)
                .is_none()
        );
        assert!(
            priced
                .resolve("prov", "role-a-similar", None, false)
                .is_none()
        );

        let mut settings = UsageSettings::default();
        settings.prices.insert(
            "prov/role-a".to_owned(),
            ModelPriceOverride {
                input: 1.5,
                output: 2.5,
                cache_read: 0.0,
                cache_write: 0.0,
                cache_write_1h: None,
            },
        );
        let custom = pricer(dir.path(), settings)
            .resolve("prov", "role-a", None, false)
            .expect("custom");
        assert_eq!(custom.input, 1.5);
    }
}
