//! D-264 ordered catalog lookup and membership-price fixtures.

use hofund_config::CatalogSettings;
use hofund_model_catalog::{build_index, parse_catalog};

fn ids(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

#[test]
fn d264_ordered_catalogs_supply_specs_and_skip_membership_prices() {
    let catalog = parse_catalog(include_bytes!("fixtures/d264_membership_catalog.json"))
        .expect("fixture catalog");
    let index = build_index(&catalog, &CatalogSettings::default());
    let ordered = ids(&["acme-coding-plan", "acme"]);

    let latest = index.resolve_in(&ordered, "latest").expect("plan model");
    assert!(latest.spec.vision);
    assert_eq!(latest.spec.context_window, Some(1_000_000));
    assert_eq!(latest.spec.max_output, Some(131_072));
    assert_eq!(latest.spec.reasoning_levels, ["low", "high", "max"]);
    assert_eq!(
        latest.spec.input_modalities,
        ["image", "pdf", "text", "video"]
    );
    assert!(index.price_in(&ordered, "latest").is_none());

    let established = index
        .resolve_in(&ordered, "established")
        .expect("established model");
    assert!(!established.spec.vision);
    assert!(established.price.is_none());
    let price = index
        .price_in(&ordered, "established")
        .expect("metered fallback price");
    assert_eq!(price.input, 1.19516);
    assert_eq!(price.output, 4.18306);
    assert_eq!(price.cache_read, 0.29879);

    assert!(index.resolve_in(&ids(&["acme"]), "latest").is_none());
    assert!(index.resolve_in(&ids(&["relay"]), "latest").is_none());
    let relay_price = index
        .price_in(&ids(&["relay"]), "established")
        .expect("relay price");
    assert_eq!(relay_price.input, 0.0699);

    let free = index
        .price_in(&ids(&["free"]), "free-model")
        .expect("a non-membership zero price");
    assert_eq!(free.input, 0.0);
    assert_eq!(free.output, 0.0);
}
