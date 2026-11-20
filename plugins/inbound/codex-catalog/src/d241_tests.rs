//! D-241: a remote `codex/<slug>` uses this computer's catalog entry, and a
//! handed catalog row says ` via hofund` once.

use hofund_plugin_sdk::json::{self, Value, json};

use super::official::set_test_codex_home;
use super::{ListedModel, catalog};

#[test]
fn d241_remote_codex_slug_uses_this_computers_entry() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cached = json!({"models": [{
        "slug": "gpt-6-astra",
        "description": "Latest frontier agentic coding model.",
        "base_instructions": "LOCAL",
        "model_messages": {"instructions_template": "T"},
        "supported_reasoning_levels": [{"effort": "high"}, {"effort": "low"}],
        "shell_type": "unified_exec",
        "default_reasoning_level": "low",
        "context_window": 200000,
        "experimental_supported_tools": [{"name": "local-shell"}]
    }]});
    std::fs::write(dir.path().join("models_cache.json"), cached.to_string()).expect("write");
    set_test_codex_home(Some(dir.path().to_path_buf()));
    let mut own_row = row("office/codex/gpt-6-astra");
    own_row.catalog_entry = Some(json!({
        "slug": "gpt-6-astra",
        "description": "Remote should not replace this computer",
        "base_instructions": "REMOTE",
        "experimental_supported_tools": [{"name": "remote-tool"}],
        "input_modalities": ["text", "image"]
    }));
    let listed = [own_row, row("relay/gpt-6-astra"), row("codex/gpt-6-sol")];
    let (body, _) = catalog(&listed, false);
    set_test_codex_home(None);
    let models: Value = json::from_str(&body).expect("json");
    let entry = |slug: &str| {
        models["models"]
            .as_array()
            .expect("models")
            .iter()
            .find(|row| row["slug"] == slug)
            .unwrap_or_else(|| panic!("missing {slug}"))
            .clone()
    };
    let own = entry("office/codex/gpt-6-astra");
    assert_eq!(own["description"], "Latest frontier agentic coding model.");
    assert_eq!(own["base_instructions"], "LOCAL");
    assert_eq!(own["model_messages"]["instructions_template"], "T");
    assert_eq!(own["supported_reasoning_levels"][0]["effort"], "high");
    assert_eq!(own["shell_type"], "unified_exec");
    assert_eq!(own["default_reasoning_level"], "low");
    assert_eq!(
        own["experimental_supported_tools"][0]["name"],
        "local-shell"
    );
    assert!(
        own["experimental_supported_tools"]
            .as_array()
            .expect("tools")
            .iter()
            .all(|tool| tool["name"] != "remote-tool")
    );
    let relay = entry("relay/gpt-6-astra");
    let relay_description = relay["description"].as_str().expect("description");
    assert!(
        relay_description.ends_with(" via hofund"),
        "{relay_description}"
    );
    assert_ne!(relay_description, "Latest frontier agentic coding model.");
    let sol = entry("codex/gpt-6-sol");
    assert_eq!(sol["description"], "codex/gpt-6-sol via hofund");
}

#[test]
fn d241_handed_entry_says_via_hofund_once() {
    set_test_codex_home(None);
    let handed = json!({
        "slug": "deepseek/deepseek-v4",
        "description": "DeepSeek V4 via hofund",
        "base_instructions": "REMOTE",
        "model_messages": {"instructions_template": "remote"},
        "shell_type": "unified_exec",
        "supported_reasoning_levels": [{"effort": "high"}],
        "service_tiers": [{"id": "priority"}],
        "experimental_supported_tools": [{"name": "remote-shell"}],
        "input_modalities": ["text", "image"]
    });
    let listed = [ListedModel {
        id: "deepseek/deepseek-v4".to_owned(),
        context_window: None,
        service_tiers: Vec::new(),
        catalog_entry: Some(handed),
    }];
    let (body, _) = catalog(&listed, false);
    let models: Value = json::from_str(&body).expect("json");
    let row = &models["models"][0];
    assert_eq!(row["slug"], "deepseek/deepseek-v4");
    assert_eq!(row["description"], "DeepSeek V4 via hofund");
    assert_eq!(row["base_instructions"], "REMOTE");
    assert_eq!(row["model_messages"]["instructions_template"], "remote");
    assert_eq!(row["service_tiers"][0]["id"], "priority");
    assert_eq!(
        row["experimental_supported_tools"][0]["name"],
        "remote-shell"
    );
    assert_eq!(row["input_modalities"][1], "image");
}

fn row(id: &str) -> ListedModel {
    ListedModel {
        id: id.to_owned(),
        context_window: None,
        service_tiers: Vec::new(),
        catalog_entry: None,
    }
}
