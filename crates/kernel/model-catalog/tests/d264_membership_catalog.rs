//! D-264 ordered catalog lookup and membership-price fixtures.

use hofund_config::CatalogSettings;
use hofund_model_catalog::{build_index, parse_catalog};

fn ids(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

#[test]
fn d264_ordered_catalogs_supply_specs_and_skip_only_named_membership_prices() {
    let catalog = parse_catalog(include_bytes!("fixtures/d264_membership_catalog.json"))
        .expect("fixture catalog");
    let index = build_index(&catalog, &CatalogSettings::default());
    let ordered = ids(&["a-plan-coding-plan", "acme"]);
    let membership = ids(&["a-plan-coding-plan"]);
    let no_membership = Vec::new();

    let latest = index.resolve_in(&ordered, "latest").expect("plan model");
    assert!(latest.spec.vision);
    assert_eq!(latest.spec.context_window, Some(1_000_000));
    assert_eq!(latest.spec.max_output, Some(131_072));
    assert_eq!(latest.spec.reasoning_levels, ["low", "high", "max"]);
    assert_eq!(
        latest.spec.input_modalities,
        ["image", "pdf", "text", "video"]
    );
    assert_eq!(latest.price.as_ref().expect("published cost").input, 0.0);
    assert!(index.price_in(&ordered, &membership, "latest").is_none());

    let established = index
        .resolve_in(&ordered, "established")
        .expect("established model");
    assert!(!established.spec.vision);
    assert_eq!(
        established.price.as_ref().expect("published cost").input,
        0.0
    );
    let bare = index.resolve("established").expect("bare row");
    assert_eq!(bare.provider_id, "a-plan-coding-plan");
    assert_eq!(bare.price.as_ref().expect("bare published cost").input, 0.0);
    let metered = index
        .resolve("acme/established")
        .and_then(|entry| entry.price.as_ref())
        .expect("metered row remains indexed");
    assert_eq!(metered.input, 1.19516);
    let price = index
        .price_in(&ordered, &membership, "established")
        .expect("metered fallback price");
    assert_eq!(price.input, 1.19516);
    assert_eq!(price.output, 4.18306);
    assert_eq!(price.cache_read, 0.29879);

    assert!(index.resolve_in(&ids(&["acme"]), "latest").is_none());
    assert!(index.resolve_in(&ids(&["relay"]), "latest").is_none());
    let relay_price = index
        .price_in(&ids(&["relay"]), &no_membership, "established")
        .expect("relay price");
    assert_eq!(relay_price.input, 0.0699);

    let free = index
        .price_in(&ids(&["free"]), &no_membership, "free-model")
        .expect("a non-membership zero price");
    assert_eq!(free.input, 0.0);
    assert_eq!(free.output, 0.0);
    let other_plan = index
        .price_in(
            &ids(&["other-coding-plan"]),
            &membership,
            "other-plan-model",
        )
        .expect("an unnamed coding-plan catalog keeps its price");
    assert_eq!(other_plan.input, 0.0);
}
