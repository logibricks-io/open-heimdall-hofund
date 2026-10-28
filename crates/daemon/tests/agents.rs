//! Agent description delivery through the daemon.

#[test]
fn compiled_agent_ids_compose_in_cli_order_and_migrated_agents_are_compiled() {
    let catalog = hofund_agent::catalog(hofund_daemon::agent_descriptions())
        .expect("compiled agent ids must not collide with built-ins");
    let ids = catalog
        .adapters()
        .into_iter()
        .map(|adapter| adapter.id())
        .collect::<Vec<_>>();
    assert!(ids.starts_with(&["claude-code", "codex", "gemini-cli", "opencode"]));
    assert!(hofund_agent::find("claude-code").is_none());
    assert!(catalog.find("claude-code").is_some());
    assert!(hofund_agent::find("codex").is_none());
    assert!(catalog.find("codex").is_some());
    assert!(hofund_agent::find("gemini-cli").is_none());
    assert!(catalog.find("gemini-cli").is_some());
    assert!(hofund_agent::find("opencode").is_none());
    assert!(catalog.find("opencode").is_some());
    assert!(hofund_agent::adapters().is_empty());
}

#[test]
fn reserved_agent_ids_are_empty_because_every_builtin_is_compiled() {
    let compiled = hofund_daemon::agent_descriptions();
    for id in ["claude-code", "codex", "gemini-cli", "opencode"] {
        assert!(
            compiled
                .iter()
                .any(|description| description.id().as_str() == id),
            "built-in agent `{id}` must ship a compiled description"
        );
    }
    hofund_agent::catalog(compiled).expect("compiled catalog without const fallback");
    assert!(
        hofund_plugin_registry::RESERVED_AGENT_IDS.is_empty(),
        "no const-backed agent remains, so no agent id is reserved: {:?}",
        hofund_plugin_registry::RESERVED_AGENT_IDS
    );
}
