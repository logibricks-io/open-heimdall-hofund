//! LongCat vendor preset.

use hofund_plugin_registry::{find_preset, select};

const CHAT: &[hofund_plugin_api::ProtocolId] = &[hofund_plugin_api::ProtocolId::OPENAI_CHAT];
const RESPONSES: &[hofund_plugin_api::ProtocolId] =
    &[hofund_plugin_api::ProtocolId::OPENAI_RESPONSES];
const MESSAGES: &[hofund_plugin_api::ProtocolId] =
    &[hofund_plugin_api::ProtocolId::ANTHROPIC_MESSAGES];

#[test]
fn d60_1_preset_loads_with_both_bases() {
    let (preset, region) = find_preset("longcat").expect("preset");
    assert_eq!(preset.id, "longcat");
    assert_eq!(preset.name, "LongCat");
    assert!(region.is_none());
    let chat = select(preset, None, CHAT, None, "").expect("chat");
    assert_eq!(chat.base_url, "https://api.longcat.chat/openai/v1");
    let responses = select(preset, None, RESPONSES, None, "").expect("responses");
    assert_eq!(responses.base_url, "https://api.longcat.chat/openai/v1");
    let messages = select(preset, None, MESSAGES, None, "").expect("messages");
    assert_eq!(messages.base_url, "https://api.longcat.chat/anthropic");
}
