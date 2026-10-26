//! Built-in factory conformance against the shared local-mock kit.

use hofund_kit_upstream::check_list_and_probe;
use hofund_plugin_api::{ProtocolId, RawTable, RawValue};

fn raw(base_url: &str) -> RawTable {
    let mut raw = RawTable::new();
    raw.insert("id", RawValue::String("conformance".to_owned()));
    raw.insert("base_url", RawValue::String(base_url.to_owned()));
    raw.insert("api_key", RawValue::String("conformance-key".to_owned()));
    raw
}

fn openai(base_url: &str, protocol: &str) -> RawTable {
    let mut raw = raw(base_url);
    raw.insert("protocol", RawValue::String(protocol.to_owned()));
    raw
}

#[tokio::test]
async fn openai_chat_factory_passes_list_and_probe_conformance() {
    let factory = hofund_plugin_registry::factory("openai-compat").expect("factory");
    check_list_and_probe(
        factory.as_ref(),
        |base| openai(base, "chat"),
        ProtocolId::OPENAI_CHAT,
        "conformance-key",
    )
    .await;
}

#[tokio::test]
async fn openai_responses_factory_passes_list_and_probe_conformance() {
    let factory = hofund_plugin_registry::factory("openai-compat").expect("factory");
    check_list_and_probe(
        factory.as_ref(),
        |base| openai(base, "responses"),
        ProtocolId::OPENAI_RESPONSES,
        "conformance-key",
    )
    .await;
}

#[tokio::test]
async fn anthropic_factory_passes_list_and_probe_conformance() {
    let factory = hofund_plugin_registry::factory("anthropic").expect("factory");
    check_list_and_probe(
        factory.as_ref(),
        raw,
        ProtocolId::ANTHROPIC_MESSAGES,
        "conformance-key",
    )
    .await;
}

#[tokio::test]
async fn generative_factory_passes_list_and_probe_conformance() {
    let factory = hofund_plugin_registry::factory("generative").expect("factory");
    check_list_and_probe(
        factory.as_ref(),
        raw,
        ProtocolId::GENERATIVE_LANGUAGE,
        "conformance-key",
    )
    .await;
}

#[tokio::test]
async fn vertex_factory_passes_static_catalog_conformance() {
    let factory = hofund_plugin_registry::factory("google-vertex").expect("factory");
    check_list_and_probe(
        factory.as_ref(),
        |base| {
            let mut raw = RawTable::new();
            raw.insert("id", RawValue::String("conformance".to_owned()));
            raw.insert("base_url", RawValue::String(base.to_owned()));
            raw.insert("location", RawValue::String("global".to_owned()));
            raw
        },
        ProtocolId::GENERATIVE_LANGUAGE,
        "",
    )
    .await;
}

#[tokio::test]
async fn bedrock_factory_passes_static_catalog_conformance() {
    let factory = hofund_plugin_registry::factory("bedrock").expect("factory");
    check_list_and_probe(
        factory.as_ref(),
        |base| {
            let mut raw = RawTable::new();
            raw.insert("id", RawValue::String("conformance".to_owned()));
            raw.insert("base_url", RawValue::String(base.to_owned()));
            raw.insert("region", RawValue::String("us-east-1".to_owned()));
            raw.insert("access_key_id", RawValue::String("access".to_owned()));
            raw.insert("secret_access_key", RawValue::String("secret".to_owned()));
            raw
        },
        ProtocolId::BEDROCK_CONVERSE,
        "",
    )
    .await;
}
