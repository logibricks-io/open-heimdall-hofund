//! Gateway routing trace agent label coverage.

use hofund_codec::{STREAM_CLIENT_PREFIXES, SUBAGENT_HEADER};
use hofund_secrets::Identity;

#[test]
fn r1_14_gateway_agent_label_prefers_subagent_header() {
    let identity = Identity {
        key_name: "Named key".to_owned(),
        ..Identity::default()
    };
    let headers = [
        (SUBAGENT_HEADER, "sub-agent-7"),
        ("user-agent", "other/1.0"),
    ];
    assert_eq!(trace_agent_label(&headers, &identity), "sub-agent-7");
}

#[test]
fn r1_14_gateway_agent_label_falls_back_to_stream_client() {
    let identity = Identity::default();
    let prefix = STREAM_CLIENT_PREFIXES[0];
    let user_agent = format!("{prefix}_cli/1.0");
    let headers = [("user-agent", user_agent.as_str())];
    assert_eq!(trace_agent_label(&headers, &identity), prefix);
}

#[test]
fn r1_14_gateway_agent_label_uses_gateway_key_name() {
    let identity = Identity {
        key_name: "Ops key".to_owned(),
        ..Identity::default()
    };
    assert_eq!(trace_agent_label(&[], &identity), "Ops key");
}

fn trace_agent_label(headers: &[(&str, &str)], identity: &Identity) -> String {
    hofund_gateway::trace_agent_label_for_test(headers, identity)
}

#[test]
fn r1_29_gateway_agent_label_is_cut_to_128_characters() {
    let long = "a".repeat(200);
    let headers = [(SUBAGENT_HEADER, long.as_str())];
    let label = trace_agent_label(&headers, &Identity::default());
    assert_eq!(label.chars().count(), 128);
    let short = "b".repeat(128);
    let headers = [(SUBAGENT_HEADER, short.as_str())];
    assert_eq!(trace_agent_label(&headers, &Identity::default()), short);
}
