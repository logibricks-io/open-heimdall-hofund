//! Catalog merge cases ported from the reference offline suite.

use hofund_kit_subscription::{TestHost, advance_time, block_on, block_on_paused};
use hofund_plugin_api::{PluginId, PluginKind, SecretString};
use hofund_plugin_sdk::json::{Value, json};

use crate::endpoints::Endpoints;
use crate::listing::{self, from_catalog, request, slug_for, tokens_of};
use crate::picker_model::{self, parse_one};

fn host(dir: &tempfile::TempDir) -> TestHost {
    TestHost::open_for(
        PluginKind::Subscription,
        PluginId::from_static("cursor"),
        dir.path(),
    )
}

fn paused_host(dir: &tempfile::TempDir) -> TestHost {
    TestHost::open_for_with_tokio_monotonic(
        PluginKind::Subscription,
        PluginId::from_static("cursor"),
        dir.path(),
    )
}

fn endpoints() -> Endpoints {
    Endpoints {
        website: "https://cursor.test".to_owned(),
        api: "https://api2.cursor.test".to_owned(),
        agent: "https://agent.cursor.test".to_owned(),
        client_version: "cli-test".to_owned(),
    }
}

fn enqueue_listing(host: &TestHost, picker: &Value, catalog: Option<&Value>, catalog_fail: bool) {
    let usable = hofund_plugin_sdk::json::to_vec(&usable_fixture()).expect("usable");
    host.http()
        .enqueue_response(200, &[("content-type", "application/json")], usable);
    if catalog_fail {
        host.http().enqueue_response(503, &[], b"down");
    } else {
        let catalog = catalog.unwrap_or(picker);
        let body = hofund_plugin_sdk::json::to_vec(catalog).expect("catalog");
        host.http()
            .enqueue_response(200, &[("content-type", "application/json")], body);
    }
    let picker_body = hofund_plugin_sdk::json::to_vec(&json!({"models": picker})).expect("picker");
    host.http()
        .enqueue_response(200, &[("content-type", "application/json")], picker_body);
}

fn tok(n: u32) -> SecretString {
    SecretString::from(format!("tok-catalog-{n}"))
}

fn list_at(host: &TestHost, n: u32, catalog: Option<&Value>) -> Vec<listing::ListEntry> {
    listing::clear_listing_cache_for_tests();
    enqueue_listing(host, &picker_fixture(), catalog, catalog.is_none());
    block_on(listing::usable(host.context(), &endpoints(), &tok(n))).expect("listing")
}

fn picker_fixture() -> Value {
    json!([{
        "name": "composer-2.5",
        "clientDisplayName": "Composer 2.5",
        "contextTokenLimit": 200000,
        "supportsNonMaxMode": true,
        "variants": [{"parameterValues": [], "isDefaultNonMaxConfig": true, "legacySlug": "composer-2.5"}],
    }])
}

fn usable_fixture() -> Value {
    json!({
        "models": [
            {"modelId": "composer-2.5", "displayModelId": "composer-2.5", "displayName": "Composer 2.5"},
            {"modelId": "claude-opus-5-5-medium", "displayName": "Claude Opus 5.5 1M Medium"},
            {"modelId": "claude-opus-5-5-high", "displayName": "Claude Opus 5.5 1M High"},
            {"modelId": "claude-3-opus", "displayName": "Claude 3 Opus"},
        ]
    })
}

fn opus55_catalog() -> Value {
    json!({"models": [{
        "name": "claude-opus-5-5",
        "clientDisplayName": "Claude Opus 5.5",
        "legacySlugs": ["claude-opus-5-5-low", "claude-opus-5-5-medium", "claude-opus-5-5-high"],
        "contextTokenLimit": 300000,
        "contextTokenLimitForMaxMode": 1000000,
        "supportsNonMaxMode": true,
        "vendor": {"id": "MODEL_VENDOR_ID_ANTHROPIC"},
        "parameterDefinitions": [
            {"id": "context", "parameterType": {"enumParameter": {"values": [{"value": "300k"}, {"value": "1m"}]}}},
            {"id": "effort", "parameterType": {"enumParameter": {"values": [{"value": "low"}, {"value": "medium"}, {"value": "high"}]}}},
            {"id": "fast", "parameterType": {"booleanParameter": {"values": [{"value": "false"}, {"value": "true"}]}}},
        ],
        "variants": [
            {"parameterValues": [{"id": "context", "value": "300k"}, {"id": "effort", "value": "medium"}, {"id": "fast", "value": "false"}], "isDefaultNonMaxConfig": true, "legacySlug": "claude-opus-5-5-medium"},
            {"parameterValues": [{"id": "context", "value": "300k"}, {"id": "effort", "value": "high"}, {"id": "fast", "value": "false"}]},
            {"parameterValues": [{"id": "context", "value": "1m"}, {"id": "effort", "value": "medium"}, {"id": "fast", "value": "false"}], "isMaxMode": true},
        ],
    }]})
}

#[test]
fn catalog_lists_live_claude_once_per_context_size() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let mut catalog = picker_fixture();
    let opus = opus55_catalog()["models"].as_array().unwrap()[0].clone();
    catalog.as_array_mut().unwrap().push(opus);
    let catalog = json!({"models": catalog});
    let raw = list_at(&host, 1, Some(&catalog));
    let claude: Vec<_> = raw
        .iter()
        .filter(|entry| entry.run.starts_with("claude") && !entry.usable_only)
        .collect();
    assert_eq!(claude.len(), 2);
    assert!(
        claude
            .iter()
            .any(|entry| entry.id == "claude-opus-5-5@300k")
    );
    assert!(claude.iter().any(|entry| entry.id == "claude-opus-5-5@1m"));
    assert!(
        raw.iter()
            .any(|entry| entry.usable_only && entry.id == "claude-3-opus")
    );
}

#[test]
fn catalog_failure_keeps_usable_only_list() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let raw = list_at(&host, 2, None);
    assert!(
        raw.iter()
            .all(|entry| !entry.run.starts_with("claude") || entry.usable_only)
    );
    assert!(
        raw.iter()
            .any(|entry| entry.usable_only && entry.id == "claude-opus-5-5")
    );
}

#[test]
fn slug_for_matches_variant_effort_thinking_and_fast() {
    let opus = parse_one(&opus55_catalog()["models"][0]).expect("opus");
    let variant = picker_model::parse_variant(&json!({
        "parameterValues": [
            {"id": "context", "value": "300k"},
            {"id": "effort", "value": "high"},
            {"id": "fast", "value": "false"},
        ]
    }))
    .expect("variant");
    assert_eq!(slug_for(&opus, &variant), "claude-opus-5-5-high");
}

#[test]
fn list_models_cache_and_stale_while_revalidate() {
    let dir = tempfile::tempdir().expect("dir");
    let host = paused_host(&dir);
    listing::clear_listing_cache_for_tests();
    let picker = json!({"models": picker_fixture()});
    enqueue_listing(&host, &picker_fixture(), Some(&picker), false);
    let token = tok(3);
    let first = block_on(listing::usable(host.context(), &endpoints(), &token)).expect("first");
    assert!(!first.is_empty());
    block_on_paused(async {
        advance_time(std::time::Duration::from_secs(11 * 60)).await;
    });
    enqueue_listing(&host, &picker_fixture(), Some(&picker), false);
    let stale = block_on(listing::list_for(host.context(), &endpoints(), &token));
    assert_eq!(stale.len(), first.len());
    block_on_paused(async {
        advance_time(std::time::Duration::from_millis(50)).await;
    });
}

#[test]
fn failed_picker_within_retry_returns_empty_list_for() {
    let dir = tempfile::tempdir().expect("dir");
    let host = paused_host(&dir);
    listing::clear_listing_cache_for_tests();
    let token = tok(4);
    for _ in 0..3 {
        host.http().enqueue_response(503, &[], b"down");
    }
    assert!(block_on(listing::usable(host.context(), &endpoints(), &token)).is_err());
    let during = block_on(listing::list_for(host.context(), &endpoints(), &token));
    assert!(during.is_empty());
    block_on_paused(async {
        advance_time(std::time::Duration::from_secs(6 * 60)).await;
    });
    listing::clear_listing_cache_for_tests();
    let picker = json!({"models": picker_fixture()});
    enqueue_listing(&host, &picker_fixture(), Some(&picker), false);
    let after =
        block_on(listing::usable(host.context(), &endpoints(), &token)).expect("after retry");
    assert!(!after.is_empty());
}

#[test]
fn request_keeps_sized_legacy_id_on_account() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let mut catalog = picker_fixture();
    catalog
        .as_array_mut()
        .unwrap()
        .push(opus55_catalog()["models"][0].clone());
    let raw = list_at(&host, 5, Some(&json!({"models": catalog})));
    let choice = request(&raw, "claude-opus-5-5@1m", "medium", false);
    assert_eq!(choice.id, "claude-opus-5-5-medium");
    assert!(choice.max_mode);
    assert_eq!(
        choice
            .parameters
            .iter()
            .find(|parameter| parameter.id == "context")
            .map(|parameter| parameter.value.as_str()),
        Some("1m")
    );
}

#[test]
fn from_catalog_skips_models_the_picker_already_has() {
    let picker = vec![
        parse_one(&json!({
            "name": "claude-opus-5-5",
            "variants": [{"parameterValues": [], "legacySlug": "claude-opus-5-5-medium"}],
            "vendor": {"id": "MODEL_VENDOR_ID_ANTHROPIC"},
        }))
        .expect("picker"),
    ];
    let catalog = vec![parse_one(&opus55_catalog()["models"][0]).expect("catalog")];
    let ids = ["claude-opus-5-5-medium"]
        .iter()
        .map(|id| (*id).to_owned())
        .collect();
    assert!(from_catalog(&picker, &catalog, &ids).is_empty());
}

#[test]
fn cyber_parameter_is_not_forwarded() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let mut opus = opus55_catalog()["models"][0].clone();
    let variants = opus["variants"].as_array_mut().expect("variants");
    for variant in variants.iter_mut() {
        let values = variant["parameterValues"].as_array_mut().expect("values");
        values.insert(0, json!({"id": "cyber", "value": "false"}));
    }
    let raw = list_at(&host, 6, Some(&json!({"models": [opus]})));
    let choice = request(&raw, "claude-opus-5-5@300k", "low", false);
    assert!(
        choice
            .parameters
            .iter()
            .all(|parameter| parameter.id != "cyber")
    );
}

#[test]
fn tokens_of_parses_k_and_m_suffixes() {
    assert_eq!(tokens_of("300k"), 300_000);
    assert_eq!(tokens_of("1m"), 1_000_000);
}
