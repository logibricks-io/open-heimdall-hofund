//! Agent description delivery through the daemon.

#[test]
fn compiled_agent_ids_compose_with_the_builtin_catalog() {
    hofund_agent::catalog(hofund_daemon::agent_descriptions())
        .expect("compiled agent ids must not collide with built-ins");
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
