//! Gateway session identity (R-33).

use hofund_codec::{ECHO_SESSION_HEADER, SESSION_HEADERS, session_of};
use hofund_gateway::session_headers::gateway_session_header_names;
use hofund_plugin_api::{AgentDescription, AgentRoute, PluginId, Visible};

const fn fixture_description(id: &'static str) -> AgentDescription {
    AgentDescription::from_static(
        PluginId::from_static(id),
        "Fixture",
        "fixture",
        &[],
        &[],
        None,
        None,
        Visible::All,
        AgentRoute::Gateway,
        &[],
    )
}

#[test]
fn r33_session_of_orders_echo_before_native_headers() {
    let names: Vec<&str> = [ECHO_SESSION_HEADER, SESSION_HEADERS[0], SESSION_HEADERS[1]]
        .into_iter()
        .collect();
    let headers = [
        (ECHO_SESSION_HEADER, "ledger"),
        (SESSION_HEADERS[0], "native"),
    ];
    assert_eq!(session_of(&names, &headers), "ledger");
}

#[test]
fn r33_plugin_session_headers_merge_after_kernel_list() {
    let agent = fixture_description("fixture").with_session_headers(&["x-fixture-session"]);
    let names = gateway_session_header_names(&[&agent]);
    assert!(names.iter().any(|name| name == "x-fixture-session"));
    assert_eq!(names[0], ECHO_SESSION_HEADER);
}
