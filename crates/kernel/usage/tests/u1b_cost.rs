//! U-1b effective prices, stored costs, and summary totals.

use std::collections::BTreeMap;
use std::sync::Arc;

use hofund_config::{ModelPriceOverride, UsageSettings};
use hofund_model_catalog::{
    CacheFile, CatalogFetcher, EffectivePricer, FetchResponse, ListPriceLookup, ModelCatalog,
    Price, parse_catalog,
};
use hofund_usage::{Ledger, Row, RowPricing, TokenPrice, UsageDetails};

const DEV_FIXTURE: &str = r#"{
  "acme": {
    "id": "acme",
    "models": {
      "gpt-demo": {
        "id": "gpt-demo",
        "cost": {"input": 2.5, "output": 10.0}
      },
      "beta": {
        "id": "beta",
        "cost": {"input": 9.0, "output": 9.0, "cache_read": 0.5, "cache_write": 1.25}
      }
    }
  }
}"#;

struct StubList;

impl ListPriceLookup for StubList {
    fn list_price(&self, provider_id: &str, model: &str) -> Option<Price> {
        (provider_id == "listed" && model == "alpha").then_some(Price {
            input: 1.0,
            output: 2.0,
            cache_read: 0.1,
            cache_write: 0.0,
            cache_write_1h: 0.0,
        })
    }
}

struct EmptyList;

impl ListPriceLookup for EmptyList {
    fn list_price(&self, _provider_id: &str, _model: &str) -> Option<Price> {
        None
    }
}

struct OfflineFetcher;

impl CatalogFetcher for OfflineFetcher {
    fn fetch(
        &self,
        _url: &str,
        _if_none_match: Option<&str>,
    ) -> Result<FetchResponse, std::io::Error> {
        Err(std::io::Error::other("offline"))
    }
}

fn seeded_catalog(dir: &std::path::Path) -> Arc<ModelCatalog> {
    let dev = parse_catalog(DEV_FIXTURE.as_bytes()).expect("fixture");
    CacheFile::beside_data_root(dir)
        .write(None, &dev)
        .expect("seed cache");
    ModelCatalog::open(
        dir,
        hofund_config::CatalogSettings::default(),
        Arc::new(OfflineFetcher),
    )
    .expect("catalog")
}

fn empty_catalog() -> Arc<ModelCatalog> {
    let dir = tempfile::tempdir().expect("dir");
    ModelCatalog::open(
        dir.path(),
        hofund_config::CatalogSettings::default(),
        Arc::new(OfflineFetcher),
    )
    .expect("catalog")
}

fn pricer(settings: UsageSettings) -> EffectivePricer {
    EffectivePricer::new(settings, empty_catalog(), Arc::new(StubList))
}

#[test]
fn u1b_1_custom_provider_model_price_wins() {
    let mut prices = BTreeMap::new();
    prices.insert(
        "relay/demo".to_owned(),
        ModelPriceOverride {
            input: 3.0,
            output: 4.0,
            cache_read: 0.0,
            cache_write: 0.0,
            cache_write_1h: None,
        },
    );
    let price = pricer(UsageSettings {
        prices,
        ..UsageSettings::default()
    })
    .resolve("relay", "demo", None, false)
    .expect("price");
    assert!((price.input - 3.0).abs() < f64::EPSILON);
}

#[test]
fn u1b_2_provider_wildcard_price_before_list() {
    let mut prices = BTreeMap::new();
    prices.insert(
        "listed/*".to_owned(),
        ModelPriceOverride {
            input: 9.0,
            output: 9.0,
            cache_read: 0.0,
            cache_write: 0.0,
            cache_write_1h: None,
        },
    );
    let price = pricer(UsageSettings {
        prices,
        ..UsageSettings::default()
    })
    .resolve("listed", "alpha", None, false)
    .expect("price");
    assert!((price.input - 9.0).abs() < f64::EPSILON);
}

#[test]
fn u1b_3_list_price_when_no_custom_entry() {
    let price = pricer(UsageSettings::default())
        .resolve("listed", "alpha", None, false)
        .expect("price");
    assert!((price.input - 1.0).abs() < f64::EPSILON);
}

#[test]
fn u1b_12_models_dev_catalog_when_provider_list_missing() {
    let dir = tempfile::tempdir().expect("dir");
    let catalog = seeded_catalog(dir.path());
    let pricer = EffectivePricer::new(UsageSettings::default(), catalog, Arc::new(EmptyList));
    let price = pricer
        .resolve("relay", "gpt-demo", None, false)
        .expect("catalog price");
    assert!((price.input - 2.5).abs() < f64::EPSILON);
}

#[test]
fn u1b_4_any_provider_model_key() {
    let mut prices = BTreeMap::new();
    prices.insert(
        "*/shared".to_owned(),
        ModelPriceOverride {
            input: 5.0,
            output: 5.0,
            cache_read: 0.0,
            cache_write: 0.0,
            cache_write_1h: None,
        },
    );
    let price = pricer(UsageSettings {
        prices,
        ..UsageSettings::default()
    })
    .resolve("other", "shared", None, false)
    .expect("price");
    assert!((price.input - 5.0).abs() < f64::EPSILON);
}

#[test]
fn u1b_5_cache_write_5m_and_1h_split() {
    let price = TokenPrice {
        input: 1.0,
        output: 0.0,
        cache_read: 0.0,
        cache_write: 1.0,
        cache_write_1h: 2.0,
    };
    let cost = price.cost_split(0, 0, 0, 1_000_000, 500_000);
    assert!((cost - 1.5).abs() < 1e-9);
}

#[test]
fn u1b_6_reasoning_is_in_output_not_extra() {
    let price = TokenPrice {
        input: 0.0,
        output: 10.0,
        cache_read: 0.0,
        cache_write: 0.0,
        cache_write_1h: 0.0,
    };
    let details = UsageDetails {
        reasoning_tokens: Some(100),
        ..UsageDetails::default()
    };
    let cost = price.cost_for_row(None, Some(1_000_000), details);
    assert!((cost - 10.0).abs() < 1e-9);
}

#[test]
fn u1b_7_stored_price_survives_later_config_change() {
    let dir = tempfile::tempdir().expect("dir");
    let ledger = Ledger::open(&dir.path().join("usage.sqlite")).expect("open");
    ledger
        .record_with_details(
            &Row {
                created_unix: 1,
                model: "demo".to_owned(),
                upstream: "primary".to_owned(),
                input_tokens: Some(1_000_000),
                output_tokens: Some(0),
                latency_ms: 1,
                status: "200".to_owned(),
                ..Default::default()
            },
            None,
            UsageDetails::default(),
            Some(RowPricing {
                priced: true,
                cost_usd: 1.25,
                price: TokenPrice {
                    input: 1.0,
                    output: 1.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                    cache_write_1h: 0.0,
                },
            }),
        )
        .expect("record");
    let (_, totals) = ledger.summary(None, 10).expect("summary");
    assert!((totals.cost_usd - 1.25).abs() < f64::EPSILON);
}

#[test]
fn u1b_8_unpriced_rows_do_not_add_cost() {
    let dir = tempfile::tempdir().expect("dir");
    let ledger = Ledger::open(&dir.path().join("usage.sqlite")).expect("open");
    ledger
        .record_with_details(
            &Row {
                created_unix: 1,
                model: "demo".to_owned(),
                upstream: "primary".to_owned(),
                input_tokens: Some(10),
                output_tokens: Some(10),
                latency_ms: 1,
                status: "200".to_owned(),
                ..Default::default()
            },
            None,
            UsageDetails::default(),
            None,
        )
        .expect("record");
    let (_, totals) = ledger.summary(None, 10).expect("summary");
    assert_eq!(totals.cost_usd, 0.0);
    assert_eq!(totals.unpriced, 1);
}

#[test]
fn u1b_9_cny_conversion_uses_configured_rate() {
    let rate = hofund_usage::Fx::open(tempfile::tempdir().expect("dir").path(), Some(7.0), None);
    let (amount, _) = rate.convert_usd(hofund_usage::DisplayCurrency::Cny, 2.0);
    assert!((amount - 14.0).abs() < f64::EPSILON);
}

#[test]
fn u1b_10_old_database_gains_cost_columns() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("usage.sqlite");
    rusqlite::Connection::open(&path)
        .expect("old database")
        .execute_batch(
            "CREATE TABLE requests (
                id INTEGER PRIMARY KEY,
                created_unix INTEGER NOT NULL,
                model TEXT NOT NULL,
                upstream TEXT NOT NULL,
                input_tokens INTEGER,
                output_tokens INTEGER,
                latency_ms INTEGER NOT NULL,
                status TEXT NOT NULL
            );",
        )
        .expect("seed");
    let ledger = Ledger::open(&path).expect("migrate");
    ledger
        .record_with_details(
            &Row {
                created_unix: 1,
                model: "demo".to_owned(),
                upstream: "up".to_owned(),
                input_tokens: Some(1),
                output_tokens: Some(1),
                latency_ms: 1,
                status: "200".to_owned(),
                ..Default::default()
            },
            None,
            UsageDetails::default(),
            Some(RowPricing::free()),
        )
        .expect("insert");
    let rows = ledger.rows().expect("rows");
    assert!(rows[0].priced);
    assert_eq!(rows[0].cost_usd, Some(0.0));
}

#[test]
fn u1b_15_one_hour_write_doubles_input_only_for_named_families() {
    let mut prices = BTreeMap::new();
    for key in ["relay/famx-large", "relay/other-large"] {
        prices.insert(
            key.to_owned(),
            ModelPriceOverride {
                input: 3.0,
                output: 15.0,
                cache_read: 0.3,
                cache_write: 3.75,
                cache_write_1h: None,
            },
        );
    }
    let settings = UsageSettings {
        prices,
        ..UsageSettings::default()
    };
    let named = pricer(settings.clone()).with_doubled_one_hour_models(vec!["FamX".to_owned()]);
    let doubled = named
        .resolve("relay", "famx-large", None, false)
        .expect("named");
    assert!((doubled.cache_write_1h - 6.0).abs() < f64::EPSILON);
    let other = named
        .resolve("relay", "other-large", None, false)
        .expect("other");
    assert!(other.cache_write_1h.abs() < f64::EPSILON);
    let plain = pricer(settings)
        .resolve("relay", "famx-large", None, false)
        .expect("plain");
    assert!(plain.cache_write_1h.abs() < f64::EPSILON);
}

struct ZeroCacheList;

impl ListPriceLookup for ZeroCacheList {
    fn list_price(&self, provider_id: &str, model: &str) -> Option<Price> {
        (provider_id == "listed" && model == "beta").then_some(Price {
            input: 1.0,
            output: 2.0,
            cache_read: 0.0,
            cache_write: 0.0,
            cache_write_1h: 0.0,
        })
    }
}

#[test]
fn u1b_16_list_price_takes_missing_cache_rates_from_catalog() {
    let dir = tempfile::tempdir().expect("dir");
    let catalog = seeded_catalog(dir.path());
    let pricer = EffectivePricer::new(UsageSettings::default(), catalog, Arc::new(ZeroCacheList));
    let price = pricer
        .resolve("listed", "beta", None, false)
        .expect("list price");
    assert!((price.input - 1.0).abs() < f64::EPSILON);
    assert!((price.cache_read - 0.5).abs() < f64::EPSILON);
    assert!((price.cache_write - 1.25).abs() < f64::EPSILON);
}

#[test]
fn u1b_17_free_provider_costs_nothing_even_with_a_custom_price() {
    let mut prices = BTreeMap::new();
    prices.insert(
        "free/model".to_owned(),
        ModelPriceOverride {
            input: 5.0,
            output: 5.0,
            cache_read: 0.0,
            cache_write: 0.0,
            cache_write_1h: None,
        },
    );
    let price = pricer(UsageSettings {
        prices,
        ..UsageSettings::default()
    })
    .resolve("free", "model", None, true)
    .expect("free price");
    assert!(price.input.abs() < f64::EPSILON);
    assert!(price.output.abs() < f64::EPSILON);
}

#[test]
fn u1b_18_one_hour_writes_are_capped_at_the_write_total() {
    let price = TokenPrice {
        input: 0.0,
        output: 0.0,
        cache_read: 0.0,
        cache_write: 1.0,
        cache_write_1h: 2.0,
    };
    let cost = price.cost_split(0, 0, 0, 1_000_000, 3_000_000);
    assert!((cost - 2.0).abs() < 1e-9);
}

#[test]
fn u1b_19_uncached_input_is_billed_instead_of_total_input() {
    let price = TokenPrice {
        input: 1.0,
        output: 0.0,
        cache_read: 0.0,
        cache_write: 0.0,
        cache_write_1h: 0.0,
    };
    let details = UsageDetails {
        uncached_input_tokens: Some(400_000),
        ..UsageDetails::default()
    };
    let cost = price.cost_for_row(Some(1_000_000), None, details);
    assert!((cost - 0.4).abs() < 1e-9);
}

#[test]
fn u1b_20_zero_token_rows_are_not_token_bearing() {
    assert!(!hofund_usage::token_bearing(Some(0), Some(0)));
    assert!(!hofund_usage::token_bearing(None, None));
    assert!(hofund_usage::token_bearing(Some(1), None));
}
