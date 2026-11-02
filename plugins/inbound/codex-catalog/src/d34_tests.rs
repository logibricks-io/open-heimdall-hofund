//! D-34: official Codex instructions in catalog entries.

use hofund_plugin_sdk::json::{self, Value, json};

use super::entry;
use super::official::{
    Lookup, ModelInstructions, is_routing_group, model_slug, reads_whole_catalog,
    set_test_codex_home, unknown_model_prompt,
};
use crate::tests::{Gateway, ask};

fn bundled_template(slug: &str) -> String {
    let bundled: Value =
        json::from_str(include_str!("official/codex_instructions.json")).expect("json");
    for row in bundled["entries"].as_array().expect("entries") {
        if row["slug"] == slug {
            return row["instructions_template"]
                .as_str()
                .expect("template")
                .to_owned();
        }
    }
    panic!("missing bundled slug {slug}");
}

#[test]
fn d34_1_gpt6_family_id_gets_bundled_template_with_or_without_prefix() {
    let lookup = Lookup::without_cache();
    let want = bundled_template("gpt-6-astra");
    for id in ["gpt-6-astra", "provider/gpt-6-astra"] {
        match lookup.for_id(id) {
            ModelInstructions::Bundled { base_instructions } => assert_eq!(base_instructions, want),
            other => panic!("{id}: {other:?}"),
        }
    }
}

#[test]
fn d34_2_longest_prefix_slug_match() {
    let lookup = Lookup::without_cache();
    let parent = bundled_template("gpt-6.1-sol");
    match lookup.for_id("gpt-6.1-sol-mini") {
        ModelInstructions::Bundled { base_instructions } => assert_eq!(base_instructions, parent),
        other => panic!("{other:?}"),
    }
    let wrong = bundled_template("gpt-6-sol");
    match lookup.for_id("gpt-6.1-sol-mini") {
        ModelInstructions::Bundled { base_instructions } => assert_ne!(base_instructions, wrong),
        other => panic!("{other:?}"),
    }
}

#[test]
fn d34_3_unknown_model_and_routing_group_get_prompt_md() {
    let lookup = Lookup::without_cache();
    assert!(matches!(
        lookup.for_id("seer/unknown-widget"),
        ModelInstructions::UnknownPrompt
    ));
    assert!(is_routing_group("group/smart"));
    assert!(matches!(
        lookup.for_id("group/smart"),
        ModelInstructions::UnknownPrompt
    ));
    let mut value = json!({"slug": "group/smart"});
    lookup.for_id("group/smart").apply(&mut value);
    assert_eq!(value["base_instructions"], unknown_model_prompt());
}

#[test]
fn d34_4_cache_entry_wins_and_bad_cache_falls_back() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache_path = dir.path().join("models_cache.json");
    let cached = json!({"models": [{
        "slug": "gpt-6.1-sol",
        "base_instructions": "cache base",
        "model_messages": {"instructions_template": "cache template", "extra": true}
    }]});
    std::fs::write(&cache_path, cached.to_string()).expect("write cache");
    set_test_codex_home(Some(dir.path().to_path_buf()));
    let lookup = Lookup::with_codex_home();
    match lookup.for_id("gpt-6.1-sol-mini") {
        ModelInstructions::Cached {
            base_instructions,
            model_messages,
        } => {
            assert_eq!(base_instructions, "cache base");
            assert_eq!(model_messages.expect("model_messages")["extra"], true);
        }
        other => panic!("{other:?}"),
    }
    std::fs::write(&cache_path, b"not json").expect("bad cache");
    let lookup = Lookup::with_codex_home();
    match lookup.for_id("gpt-6.1-sol-mini") {
        ModelInstructions::Bundled { .. } => {}
        other => panic!("{other:?}"),
    }
    set_test_codex_home(None);
}

#[test]
fn d34_5_catalog_cap_follows_client_version() {
    assert!(!reads_whole_catalog(None));
    assert!(!reads_whole_catalog(Some("0.150.0")));
    assert!(reads_whole_catalog(Some("0.166.0")));
    assert!(reads_whole_catalog(Some("0.167.0-alpha.1")));
    assert!(reads_whole_catalog(Some("1.0.0")));
}

#[test]
fn d34_6_catalog_cap_on_serve_and_off_for_new_codex() {
    let ids: Vec<String> = (0..600).map(|n| format!("many/model-{n:03}")).collect();
    let gateway = Gateway::listing(&Value::Array(
        ids.iter().map(|id| json!({"id": id})).collect(),
    ));
    let (_, _, header, _) = ask(&gateway, "/v1/codex/models?client_version=0.150.0");
    assert!(header.is_some());
    let (_, _, header, _) = ask(&gateway, "/v1/codex/models");
    assert!(header.is_some());
    let (_, _, header, body) = ask(&gateway, "/v1/codex/models?client_version=0.166.0");
    assert_eq!(header, None);
    let listed: Value = json::from_str(&body).expect("json");
    assert_eq!(
        listed["models"].as_array().expect("models").len(),
        ids.len()
    );
}

#[test]
fn d34_8_entry_puts_instructions_not_hofund_fallback() {
    let lookup = Lookup::without_cache();
    let value = entry("gpt-6-astra", 1, &lookup);
    assert_eq!(value["base_instructions"], bundled_template("gpt-6-astra"));
    assert_ne!(value["base_instructions"], unknown_model_prompt());
}

#[test]
fn d34_9_model_slug_takes_last_segment() {
    assert_eq!(model_slug("vendor/gpt-6-astra"), "gpt-6-astra");
}

#[test]
fn d34_10_client_version_found_after_a_bare_query_pair() {
    let ids: Vec<String> = (0..600).map(|n| format!("many/model-{n:03}")).collect();
    let gateway = Gateway::listing(&Value::Array(
        ids.iter().map(|id| json!({"id": id})).collect(),
    ));
    let (_, _, header, _) = ask(&gateway, "/v1/codex/models?flag&client_version=0.166.0");
    assert_eq!(header, None);
}

#[test]
fn d34_11_cache_row_without_instructions_falls_back_and_adds_no_messages() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cached = json!({"models": [
        {"slug": "gpt-6-astra", "base_instructions": ""},
        {"slug": "gpt-6.1-sol", "base_instructions": "cache base"}
    ]});
    std::fs::write(dir.path().join("models_cache.json"), cached.to_string()).expect("write");
    set_test_codex_home(Some(dir.path().to_path_buf()));
    let lookup = Lookup::with_codex_home();
    set_test_codex_home(None);
    let value = entry("gpt-6-astra", 1, &lookup);
    assert_eq!(value["base_instructions"], bundled_template("gpt-6-astra"));
    let value = entry("gpt-6.1-sol", 1, &lookup);
    assert_eq!(value["base_instructions"], "cache base");
    assert!(value.get("model_messages").is_none(), "{value}");
}

#[test]
fn d34_12_a_slug_only_matches_at_a_dash_boundary() {
    let lookup = Lookup::without_cache();
    assert_eq!(
        lookup.for_id("gpt-6.1-solar"),
        ModelInstructions::UnknownPrompt
    );
}

#[test]
fn d34_13_longest_cache_slug_wins_and_its_messages_are_applied() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cached = json!({"models": [
        {"slug": "gpt-6.1-sol-mini", "base_instructions": "mini base",
         "model_messages": {"instructions_template": "mini"}},
        {"slug": "gpt-6.1-sol", "base_instructions": "sol base"}
    ]});
    std::fs::write(dir.path().join("models_cache.json"), cached.to_string()).expect("write");
    set_test_codex_home(Some(dir.path().to_path_buf()));
    let lookup = Lookup::with_codex_home();
    set_test_codex_home(None);
    let value = entry("vendor/gpt-6.1-sol-mini-fast", 1, &lookup);
    assert_eq!(value["base_instructions"], "mini base");
    assert_eq!(value["model_messages"]["instructions_template"], "mini");
}
