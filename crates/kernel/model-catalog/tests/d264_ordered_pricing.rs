//! D-264 ordered fallback pricing without a cached model-list price.

use std::io;
use std::path::Path;
use std::sync::Arc;

use hofund_config::{CatalogSettings, UsageSettings};
use hofund_model_catalog::{
    CacheFile, CatalogFetcher, EffectivePricer, FetchResponse, ListPriceLookup, ModelCatalog,
    Price, ProviderCatalogs, parse_catalog,
};

struct Offline;

impl CatalogFetcher for Offline {
    fn fetch(&self, _url: &str, _if_none_match: Option<&str>) -> io::Result<FetchResponse> {
        Err(io::Error::other("offline"))
    }
}

struct Lookup {
    listed: bool,
}

impl ListPriceLookup for Lookup {
    fn list_price(&self, provider_id: &str, model: &str) -> Option<Price> {
        (self.listed && provider_id == "plan" && model == "established").then_some(Price {
            input: 1.19516,
            output: 4.18306,
            cache_read: 0.0,
            cache_write: 0.0,
            cache_write_1h: 0.0,
        })
    }

    fn provider_catalogs(&self, provider_id: &str) -> Option<ProviderCatalogs> {
        let catalogs = match provider_id {
            "plan" => vec!["a-plan-coding-plan", "acme"],
            "free" => vec!["free"],
            "other" => vec!["other-coding-plan"],
            _ => return None,
        };
        let membership_catalogs = if provider_id == "plan" {
            vec!["a-plan-coding-plan".to_owned()]
        } else {
            Vec::new()
        };
        Some(ProviderCatalogs {
            catalogs: catalogs.into_iter().map(str::to_owned).collect(),
            membership_catalogs,
        })
    }
}

fn catalog(dir: &Path) -> Arc<ModelCatalog> {
    let parsed = parse_catalog(include_bytes!("fixtures/d264_membership_catalog.json"))
        .expect("fixture catalog");
    CacheFile::beside_data_root(dir)
        .write(None, &parsed)
        .expect("seed catalog");
    ModelCatalog::open(dir, CatalogSettings::default(), Arc::new(Offline)).expect("open catalog")
}

fn pricer(catalog: Arc<ModelCatalog>, listed: bool) -> EffectivePricer {
    EffectivePricer::new(
        UsageSettings::default(),
        catalog,
        Arc::new(Lookup { listed }),
    )
}

#[test]
fn d264_pricer_walks_ordered_catalogs_without_a_listed_price() {
    let directory = tempfile::tempdir().expect("catalog directory");
    let pricer = pricer(catalog(directory.path()), false);

    let established = pricer
        .resolve("plan", "established", None, false)
        .expect("metered fallback");
    assert_eq!(established.input, 1.19516);
    assert_eq!(established.output, 4.18306);
    assert_eq!(established.cache_read, 0.29879);
    assert!(pricer.resolve("plan", "latest", None, false).is_none());

    let free = pricer
        .resolve("free", "free-model", None, false)
        .expect("non-membership zero price");
    assert_eq!(free.input, 0.0);
    let other = pricer
        .resolve("other", "other-plan-model", None, false)
        .expect("other coding-plan price");
    assert_eq!(other.input, 0.0);
}

#[test]
fn d264_pricer_enriches_a_listed_price_from_the_metered_catalog() {
    let directory = tempfile::tempdir().expect("catalog directory");
    let pricer = pricer(catalog(directory.path()), true);
    let established = pricer
        .resolve("plan", "established", None, false)
        .expect("listed price");
    assert_eq!(established.input, 1.19516);
    assert_eq!(established.cache_read, 0.29879);
}
