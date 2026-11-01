//! Effective list prices for usage rows.

use std::sync::Arc;

use hofund_config::{ModelPriceOverride, UsageSettings};

use crate::spec::Price;
use crate::{ModelCatalog};

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
        }
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
            return Some(enrich_from_catalog(self.catalog.as_ref(), provider_id, model, price));
        }
        if let Some(served) = served.filter(|served| *served != model)
            && let Some(price) = self.list.list_price(provider_id, served)
        {
            return Some(enrich_from_catalog(
                self.catalog.as_ref(),
                provider_id,
                served,
                price,
            ));
        }
        if let Some(price) = self.catalog_price(provider_id, model) {
            return Some(price);
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
                return Some(price_from_override(model, custom));
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

fn price_from_override(model_id: &str, custom: &ModelPriceOverride) -> Price {
    let mut price = Price {
        input: custom.input,
        output: custom.output,
        cache_read: custom.cache_read,
        cache_write: custom.cache_write,
        cache_write_1h: custom.cache_write_1h.unwrap_or(0.0),
    };
    one_hour_for(model_id, &mut price);
    price
}

fn one_hour_for(model_id: &str, price: &mut Price) {
    if price.cache_write <= 0.0 || price.cache_write_1h > 0.0 {
        return;
    }
    if model_id.to_ascii_lowercase().contains("claude") {
        price.cache_write_1h = 2.0 * price.input;
    }
}

fn enrich_from_catalog(
    catalog: &ModelCatalog,
    provider_id: &str,
    model: &str,
    mut price: Price,
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
    one_hour_for(model, &mut price);
    price
}
