//! Golden conformance for compiled agent descriptions.

use std::path::Path;

#[test]
fn shipped_agents_match_their_golden_cases() {
    for description in hofund_plugin_registry::agents() {
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../agents")
            .join(description.id().as_str())
            .join("fixtures");
        hofund_kit_agent::conformance(description, &fixtures);
    }
}

#[cfg(feature = "test-fixtures")]
#[test]
fn compiled_agents_match_their_golden_cases() {
    for description in hofund_plugin_registry::fixture_agents() {
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/agents")
            .join(description.id().as_str())
            .join("fixtures");
        hofund_kit_agent::conformance(description, &fixtures);
    }
}
