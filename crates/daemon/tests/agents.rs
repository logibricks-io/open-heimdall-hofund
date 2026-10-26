//! Agent description delivery through the daemon.

#[test]
fn compiled_agent_ids_compose_with_the_builtin_catalog() {
    hofund_agent::catalog(hofund_daemon::agent_descriptions())
        .expect("compiled agent ids must not collide with built-ins");
}
