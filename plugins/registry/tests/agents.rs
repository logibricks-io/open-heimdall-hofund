//! Compiled data-only agent descriptions.

#![cfg(feature = "test-fixtures")]

use hofund_plugin_api::{AgentRoute, FileKind, ModelField, Slot, Visible};
use hofund_plugin_registry::{agent, agents, fixture_agents};

#[test]
fn production_agent_list_contains_migrated_agents() {
    let ids: Vec<&str> = agents().iter().map(|agent| agent.id().as_str()).collect();
    assert_eq!(
        ids,
        [
            "claude-code",
            "codex",
            "crush",
            "gemini-cli",
            "hermes",
            "opencode",
            "pi"
        ]
    );
    assert_eq!(
        agent("claude-code").expect("claude-code").id().as_str(),
        "claude-code"
    );
    assert_eq!(agent("codex").expect("codex").id().as_str(), "codex");
    assert_eq!(
        agent("gemini-cli").expect("gemini-cli").id().as_str(),
        "gemini-cli"
    );
    assert_eq!(
        agent("opencode").expect("opencode").id().as_str(),
        "opencode"
    );
    assert_eq!(
        agent("opencode").expect("opencode").model_prefix(),
        Some("hofund/")
    );
    assert_eq!(agent("codex").expect("codex").model_prefix(), None);
    assert!(agent("missing").is_none());
}

#[test]
fn fixture_agents_preserve_files_sets_slots_and_key_files() {
    let fixtures = fixture_agents();
    assert_eq!(fixtures.len(), 3);
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

    let yaml = fixtures[2];
    assert_eq!(yaml.id().as_str(), "fixture-yaml-agent");
    assert_eq!(yaml.files().len(), 1);
    assert_eq!(yaml.files()[0].kind(), FileKind::Yaml);
    assert_eq!(yaml.files()[0].env_override(), Some("FIXTURE_YAML_HOME"));
    assert_eq!(yaml.sets().len(), 6);
    assert_eq!(yaml.model_key(), Some("model.default"));
    assert!(matches!(yaml.sets()[1].slot(), Slot::V1));
    assert!(matches!(yaml.sets()[2].slot(), Slot::Token));
    static FIELDS: &[ModelField] = &[
        ModelField::id("id"),
        ModelField::literal("name", "Hofund"),
        ModelField::json("reasoning", "true"),
    ];
    assert_eq!(yaml.sets()[4].slot(), &Slot::ModelArray { fields: FIELDS });
    assert_eq!(yaml.sets()[5].slot(), &Slot::Json("false"));
}
