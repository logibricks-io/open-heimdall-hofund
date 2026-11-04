//! Usage row client-agent resolution on the gateway.

use std::sync::Arc;

use axum::http::HeaderMap;
use hofund_agent::request_agent_id;
use hofund_plugin_api::{AgentDescription, AgentRoute, PluginId, Visible};
use hofund_router::{FailoverPolicy, Router, Snapshot};
use hofund_secrets::Identity;
use hofund_usage::{Ledger, UNKNOWN_CLIENT_AGENT};

const fn description(id: &'static str, binary: &'static str) -> AgentDescription {
    AgentDescription::from_static(
        PluginId::from_static(id),
        "Fixture",
        binary,
        &[],
        &[],
        None,
        None,
        Visible::All,
        AgentRoute::Gateway,
        &[],
    )
}

static HOST: AgentDescription =
    description("fixture-host", "host").with_request_agent_header("x-fixture-agent-id");

static AGENT: AgentDescription = description("fixture-agent", "fixture")
    .with_client_prefixes(&["fixture", "fixture-extension"])
    .with_nested_turn_headers(&["x-fixture-window-id", "x-fixture-turn-metadata"]);

fn gateway_with_hook() -> hofund_gateway::Gateway {
    let dir = tempfile::tempdir().expect("temp dir");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let hook: hofund_gateway::RequestAgentHook = Arc::new(|headers, identity| {
        request_agent_id(
            headers
                .iter()
                .filter_map(|(name, value)| Some((name.as_str(), value.to_str().ok()?))),
            &identity.key_name,
            &[&HOST, &AGENT],
        )
    });
    hofund_gateway::Gateway::new(
        Snapshot::new(Router::new(
            std::collections::BTreeMap::new(),
            FailoverPolicy::default(),
        )),
        "secret".to_owned(),
        Default::default(),
        usage,
    )
    .with_request_agent(hook)
}

fn resolve(gateway: &hofund_gateway::Gateway, headers: HeaderMap, identity: Identity) -> String {
    hofund_gateway::resolve_client_agent_for_test(gateway, &headers, &identity)
}

#[test]
fn u1c_1_client_agent_from_request_agent_header() {
    let gateway = gateway_with_hook();
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-fixture-agent-id",
        "fixture-host".parse().expect("header"),
    );
    assert_eq!(
        resolve(&gateway, headers, Identity::default()),
        "fixture-host"
    );
}

#[test]
fn u1c_2_client_agent_from_user_agent_prefix() {
    let gateway = gateway_with_hook();
    let mut headers = HeaderMap::new();
    headers.insert("user-agent", "fixture/1.0 (test)".parse().expect("header"));
    assert_eq!(
        resolve(&gateway, headers, Identity::default()),
        "fixture-agent"
    );
}

#[test]
fn u1c_3_client_agent_from_originator_prefix() {
    let gateway = gateway_with_hook();
    let mut headers = HeaderMap::new();
    headers.insert("originator", "fixture-extension/2".parse().expect("header"));
    assert_eq!(
        resolve(&gateway, headers, Identity::default()),
        "fixture-agent"
    );
}

#[test]
fn u1c_4_client_agent_from_nested_turn_header() {
    let gateway = gateway_with_hook();
    let mut headers = HeaderMap::new();
    headers.insert("user-agent", "host/1.0".parse().expect("header"));
    headers.insert("x-fixture-window-id", "w".parse().expect("header"));
    assert_eq!(
        resolve(&gateway, headers, Identity::default()),
        "fixture-agent"
    );
}

#[test]
fn u1c_5_client_agent_from_gateway_key_name() {
    let gateway = gateway_with_hook();
    let identity = Identity {
        key_name: "Fixture-Agent".to_owned(),
        ..Identity::default()
    };
    assert_eq!(
        resolve(&gateway, HeaderMap::new(), identity),
        "fixture-agent"
    );
}

#[test]
fn u1c_6_client_agent_unknown_when_unmatched() {
    let dir = tempfile::tempdir().expect("temp dir");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let gateway = hofund_gateway::Gateway::new(
        Snapshot::new(Router::new(
            std::collections::BTreeMap::new(),
            FailoverPolicy::default(),
        )),
        "secret".to_owned(),
        Default::default(),
        usage,
    );
    let mut headers = HeaderMap::new();
    headers.insert("user-agent", "unmatched/1.0".parse().expect("header"));
    assert_eq!(
        resolve(&gateway, headers, Identity::default()),
        UNKNOWN_CLIENT_AGENT
    );
}
