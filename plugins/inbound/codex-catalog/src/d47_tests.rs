//! D-47: auto-compact limit for models with no known context window.

use std::path::PathBuf;

use hofund_plugin_sdk::json::{self, Value, json};

use super::official::Lookup;
use super::tests::{Gateway, ask};
use super::{DEFAULT_AUTO_COMPACT_TOKEN_LIMIT, ListedModel, catalog, entry};

#[test]
fn d47_1_no_window_entry_gets_auto_compact_limit_and_no_context_window() {
    let lookup = Lookup::without_cache();
    let value = entry("vendor/unknown", 1, &lookup, None);
    assert_eq!(
        value["auto_compact_token_limit"],
        DEFAULT_AUTO_COMPACT_TOKEN_LIMIT
    );
    assert!(value.get("context_window").is_none(), "{value}");
}

#[test]
fn d47_2_an_entry_with_a_window_is_unchanged() {
    let lookup = Lookup::without_cache();
    let mut without_limit = entry("vendor/windowed", 2, &lookup, None);
    without_limit
        .as_object_mut()
        .expect("object")
        .remove("auto_compact_token_limit");
    let with_window = entry("vendor/windowed", 2, &lookup, Some(200_000));
    assert_eq!(with_window, without_limit);
    assert!(with_window.get("auto_compact_token_limit").is_none());
}

#[test]
fn d47_3_catalog_json_matches_official_fixture_field_types() {
    let fixture_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/codex-rust-v0.166.0/models.json");
    let fixture: Value =
        json::from_slice(&std::fs::read(fixture_path).expect("models.json")).expect("json");
    let official = fixture["models"][0].clone();
    let lookup = Lookup::without_cache();
    let ours = entry("vendor/fixture-check", 1, &lookup, None);
    assert_eq!(ours["auto_compact_token_limit"].as_u64(), Some(272_000));
    for (key, value) in ours.as_object().expect("object") {
        let Some(sample) = official.get(key) else {
            continue;
        };
        if value.is_null() || sample.is_null() {
            continue;
        }
        assert_eq!(
            json_type(value),
            json_type(sample),
            "field {key} type mismatch"
        );
    }
    let gateway = Gateway::listing(&json!([
        {"id": "vendor/fixture-check", "object": "model"},
    ]));
    let (_, _, _, body) = ask(&gateway, "/v1/codex/models?client_version=0.166.0");
    let list: Value = json::from_str(&body).expect("catalog json");
    assert!(list["models"][0]["auto_compact_token_limit"].is_number());
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[test]
fn d47_4_served_catalog_uses_window_from_the_gateway_list() {
    let gateway = Gateway::listing(&json!([
        {"id": "vendor/windowed", "context_window": 200_000},
        {"id": "vendor/open"},
    ]));
    let (_, _, _, body) = ask(&gateway, "/v1/codex/models?client_version=0.166.0");
    let models = json::from_str::<Value>(&body).expect("json")["models"]
        .as_array()
        .expect("models")
        .clone();
    let windowed = models
        .iter()
        .find(|row| row["slug"] == "vendor/windowed")
        .expect("windowed");
    assert!(windowed.get("auto_compact_token_limit").is_none());
    let open = models
        .iter()
        .find(|row| row["slug"] == "vendor/open")
        .expect("open");
    assert_eq!(
        open["auto_compact_token_limit"],
        DEFAULT_AUTO_COMPACT_TOKEN_LIMIT
    );
    let listed = [
        ListedModel {
            id: "vendor/windowed".to_owned(),
            context_window: Some(200_000),
        },
        ListedModel {
            id: "vendor/open".to_owned(),
            context_window: None,
        },
    ];
    let (built, _) = catalog(&listed, false);
    assert!(built.contains("\"auto_compact_token_limit\":272000"));
}
