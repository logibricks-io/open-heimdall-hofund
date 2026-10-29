use hofund_plugin_api::ProtocolId;

use super::{
    LEARNED_ROUTE_CAPACITY, LearnedRoutes, closed_model, host_of, is_mantle_base, mantle_base,
    mantle_host, normalize, v1_root, wrong_route, wrong_route_message,
};

const V1: &str = "https://bedrock-mantle.us-east-1.api.aws/v1";
const OPENAI: &str = "https://bedrock-mantle.us-east-1.api.aws/openai/v1";

#[test]
fn learned_routes_evict_the_oldest_entry_at_the_capacity() {
    let routes = LearnedRoutes::default();
    for index in 0..=LEARNED_ROUTE_CAPACITY {
        routes.learn_other(&format!("model-{index}"));
    }
    assert!(!routes.other("model-0"));
    assert!(routes.other("model-1"));
    assert!(routes.other(&format!("model-{LEARNED_ROUTE_CAPACITY}")));
    assert_eq!(routes.len(), LEARNED_ROUTE_CAPACITY);
}

#[test]
fn learning_twice_keeps_one_entry_and_forget_drops_it() {
    let routes = LearnedRoutes::default();
    routes.learn_other("a");
    routes.learn_other("a");
    assert_eq!(routes.len(), 1);
    routes.forget("b");
    assert!(routes.other("a"));
    routes.forget("a");
    assert!(!routes.other("a"));
    assert_eq!(routes.len(), 0);
    // A forgotten entry leaves the eviction order too: filling to the
    // capacity again evicts nothing that is still learned.
    for index in 0..LEARNED_ROUTE_CAPACITY {
        routes.learn_other(&format!("model-{index}"));
    }
    routes.learn_other("a");
    routes.forget("a");
    routes.learn_other("last");
    assert!(!routes.other("model-0"));
    assert!(routes.other("model-1"));
    assert!(routes.other("last"));
    assert_eq!(routes.len(), LEARNED_ROUTE_CAPACITY);
}

#[test]
fn a_closed_model_uses_the_openai_prefix() {
    let cases = [
        (V1, "openai.gpt-oss-120b", false, V1),
        (V1, "google.gemma-3-27b-it", false, V1),
        (V1, "openai.gpt-5.5", false, OPENAI),
        (V1, "openai.gpt-6-sol", false, OPENAI),
        (V1, "xai.grok-4.3", false, OPENAI),
        (V1, "XAI.grok-4.3", false, OPENAI),
        (OPENAI, "openai.gpt-oss-120b", false, V1),
        (
            "https://bedrock-mantle.us-east-1.api.aws/openai/v1/",
            "xai.grok-4.3",
            false,
            OPENAI,
        ),
        (V1, "newmaker.model-1", true, OPENAI),
        (V1, "openai.gpt-5.5", true, V1),
        (
            "https://proxy.example.com/mantle",
            "openai.gpt-5.5",
            false,
            "https://proxy.example.com/mantle",
        ),
    ];
    for (base, model, other, want) in cases {
        assert_eq!(
            mantle_base(base, model, other),
            want,
            "{model} other={other}"
        );
    }
    assert!(closed_model("global.openai.gpt-6-luna"));
    assert!(!closed_model("openai.gpt-oss-20b-1:0"));
    assert!(!closed_model("openai.o3"));
}

#[test]
fn only_a_mantle_host_is_a_mantle_base() {
    for base in [
        V1,
        OPENAI,
        "https://bedrock-mantle.eu-west-1.api.aws/openai/v1/",
        "https://user@BEDROCK-MANTLE.eu-west-1.api.aws:443/v1",
    ] {
        assert!(is_mantle_base(base), "{base}");
        assert!(v1_root(base).is_some(), "{base}");
    }
    for base in [
        "https://bedrock-runtime.us-east-1.amazonaws.com/openai/v1",
        "https://bedrock-mantle.us-east-1.amazonaws.com/v1",
        "https://bedrock-mantle.example.com/v1",
        "https://api.aws/v1",
        "https://relay.example.com/bedrock-mantle.us-east-1.api.aws/v1",
        "https://api.openai.com/v1",
        "bedrock-mantle.us-east-1.api.aws/v1",
        "https:///v1",
    ] {
        assert!(!is_mantle_base(base), "{base}");
    }
    assert!(mantle_host("bedrock-mantle.us-east-1.api.aws"));
    assert!(mantle_host("Bedrock-Mantle.us-east-1.API.aws:8443"));
    assert!(!mantle_host("bedrock-runtime.us-east-1.amazonaws.com"));
    assert!(!mantle_host("bedrock-mantle.us-east-1.api.aws.example"));
    assert!(!mantle_host("x.bedrock-mantle.us-east-1.api.aws"));
    assert_eq!(
        host_of("https://u:p@Host.Example:8080/x?y#z").as_deref(),
        Some("host.example")
    );
    assert_eq!(host_of("https://h?x").as_deref(), Some("h"));
    assert_eq!(host_of("https://h#x").as_deref(), Some("h"));
    assert_eq!(host_of("no-scheme"), None);
}

#[test]
fn a_mantle_host_normalizes_each_protocol() {
    let root = "https://bedrock-mantle.eu-west-1.api.aws";
    let v1 = "https://bedrock-mantle.eu-west-1.api.aws/v1";
    let anthropic = "https://bedrock-mantle.eu-west-1.api.aws/anthropic";
    let openai = "https://bedrock-mantle.eu-west-1.api.aws/openai/v1";
    for (protocol, input, expected) in [
        (ProtocolId::OPENAI_CHAT, v1, v1),
        (
            ProtocolId::OPENAI_CHAT,
            "https://bedrock-mantle.eu-west-1.api.aws/openai/v1/",
            v1,
        ),
        (ProtocolId::OPENAI_CHAT, root, v1),
        (ProtocolId::OPENAI_CHAT, anthropic, v1),
        (ProtocolId::OPENAI_RESPONSES, root, v1),
        (ProtocolId::OPENAI_RESPONSES, anthropic, v1),
        (ProtocolId::OPENAI_RESPONSES, openai, v1),
        (ProtocolId::ANTHROPIC_MESSAGES, root, anthropic),
        (ProtocolId::ANTHROPIC_MESSAGES, v1, anthropic),
        (ProtocolId::ANTHROPIC_MESSAGES, openai, anthropic),
        (ProtocolId::ANTHROPIC_MESSAGES, anthropic, anthropic),
        (ProtocolId::BEDROCK_CONVERSE, root, root),
    ] {
        assert_eq!(normalize(input, protocol), expected, "{protocol} {input}");
    }
}

#[test]
fn another_host_or_an_unrelated_path_keeps_its_base() {
    assert_eq!(
        normalize("https://relay.example/openai/v1", ProtocolId::OPENAI_CHAT),
        "https://relay.example/openai/v1"
    );
    assert_eq!(
        normalize("https://relay.example", ProtocolId::ANTHROPIC_MESSAGES),
        "https://relay.example"
    );
    assert_eq!(
        normalize(
            "https://bedrock-mantle.us-east-1.api.aws/custom",
            ProtocolId::OPENAI_CHAT
        ),
        "https://bedrock-mantle.us-east-1.api.aws/custom"
    );
    assert_eq!(
        normalize(
            "https://bedrock-mantle.us-east-1.api.aws/custom",
            ProtocolId::ANTHROPIC_MESSAGES
        ),
        "https://bedrock-mantle.us-east-1.api.aws/custom"
    );
}

#[test]
fn the_route_refusal_is_a_client_error_that_names_the_route() {
    assert!(wrong_route(
        Some(400),
        "model `x` isn't supported on this route"
    ));
    assert!(wrong_route(
        Some(404),
        "Model is not supported on this route"
    ));
    assert!(wrong_route(Some(499), "not supported on this route"));
    assert!(!wrong_route(Some(399), "not supported on this route"));
    assert!(!wrong_route(Some(500), "isn't supported on this route"));
    assert!(!wrong_route(Some(400), "invalid model id"));
    assert!(!wrong_route(None, "isn't supported on this route"));
    assert!(wrong_route_message("IS NOT SUPPORTED ON THIS ROUTE"));
    assert!(!wrong_route_message("not supported"));
}
