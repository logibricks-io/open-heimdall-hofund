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
    for id in ["claude-code", "codex", "gemini-cli", "opencode"] {
        assert!(catalog.find(id).is_some(), "missing compiled agent `{id}`");
    }
}

#[test]
fn every_builtin_ships_a_compiled_description() {
    let compiled = hofund_daemon::agent_descriptions();
    for id in ["claude-code", "codex", "gemini-cli", "opencode"] {
        assert!(
            compiled
                .iter()
                .any(|description| description.id().as_str() == id),
            "built-in agent `{id}` must ship a compiled description"
        );
    }
    hofund_agent::catalog(compiled).expect("compiled catalog");
}
