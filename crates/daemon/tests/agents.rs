//! Agent description delivery through the daemon.

#[test]
fn compiled_agent_ids_compose_in_cli_order_and_codex_is_compiled() {
    let catalog = hofund_agent::catalog(hofund_daemon::agent_descriptions())
        .expect("compiled agent ids must not collide with built-ins");
    let ids = catalog
        .adapters()
        .into_iter()
        .map(|adapter| adapter.id())
        .collect::<Vec<_>>();
    assert!(ids.starts_with(&["claude-code", "codex", "gemini-cli", "opencode"]));
    assert!(hofund_agent::find("codex").is_none());
    assert!(catalog.find("codex").is_some());
}

#[test]
fn reserved_agent_ids_match_the_builtin_catalog() {
    let builtins = hofund_agent::catalog(&[]).expect("built-in catalog");
    let ids = builtins
        .adapters()
        .into_iter()
        .map(|adapter| adapter.id())
        .collect::<Vec<_>>();
    assert_eq!(hofund_plugin_registry::RESERVED_AGENT_IDS, ids);
}
