//! Compiled data-only agent descriptions.

#![cfg(feature = "test-fixtures")]

use hofund_plugin_api::{AgentRoute, FileKind, Slot, Visible};
use hofund_plugin_registry::{agent, agents, fixture_agents};

#[test]
fn production_agent_list_is_currently_empty() {
    assert!(agents().is_empty());
    assert!(agent("missing").is_none());
}

#[test]
fn fixture_agents_preserve_files_sets_slots_and_key_files() {
    let fixtures = fixture_agents();
    assert_eq!(fixtures.len(), 2);
    assert_eq!(fixtures[0].id().as_str(), "fixture-json-agent");
    assert_eq!(fixtures[1].id().as_str(), "fixture-toml-agent");
    assert_eq!(agent("fixture-json-agent"), None);

    let json = fixtures[0];
    assert_eq!(json.files().len(), 5);
    assert_eq!(json.sets().len(), 12);
    assert_eq!(json.model_file(), Some("settings"));
    assert_eq!(json.model_key(), Some("model.name"));
    assert_eq!(json.effort_file(), None);
    assert_eq!(json.route(), AgentRoute::Gateway);
    assert_eq!(json.visible(), Visible::All);
    assert_eq!(json.files()[0].kind(), FileKind::Json);
    assert_eq!(json.files()[1].kind(), FileKind::Env);
    assert_eq!(json.files()[2].id(), "origin");
    assert_eq!(json.files()[3].id(), "config");
    assert!(!json.files()[3].create());
    assert!(json.files()[4].create());
    assert!(matches!(json.sets()[0].slot(), Slot::Origin));
    assert!(matches!(
        json.sets()[3].slot(),
        Slot::ModelIfUnset { from: None }
    ));
    assert!(matches!(
        json.sets()[4].slot(),
        Slot::ModelIfUnset {
            from: Some("legacy_model")
        }
    ));
    assert!(matches!(
        json.sets()[6].slot(),
        Slot::ModelTier {
            when_model_matches: Some(needle),
            ..
        } if needle.as_str() == "fixture-fable-"
    ));
    assert!(matches!(json.sets()[11].slot(), Slot::CatalogModels));

    let toml = fixtures[1];
    assert_eq!(toml.files().len(), 1);
    assert_eq!(toml.sets().len(), 5);
    assert_eq!(toml.files()[0].kind(), FileKind::Toml);
    assert_eq!(toml.files()[0].env_override(), Some("FIXTURE_TOML_HOME"));
    assert_eq!(toml.model_file(), Some("config"));
    assert_eq!(toml.model_key(), Some("model"));
    assert_eq!(toml.effort_file(), Some("config"));
    assert_eq!(toml.effort_key(), Some("model_reasoning_effort"));
    assert!(matches!(toml.sets()[2].slot(), Slot::V1));
    assert!(matches!(toml.sets()[4].slot(), Slot::Token));
}
