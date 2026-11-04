//! D-53: Tencent Token Plan preset endpoints and saved-provider migration.

use hofund_plugin_registry::{find_preset, resolve_region, select};

const OPENAI: &[hofund_plugin_api::ProtocolId] = &[hofund_plugin_api::ProtocolId::OPENAI_CHAT];

#[test]
fn d53_1_plan_region_uses_tokenhub_plan_endpoints() {
    let (preset, region) = find_preset("tencent-token-plan").expect("preset");
    assert_eq!(preset.id, "tencent-cloud");
    assert_eq!(region, Some("plan"));
    let region = resolve_region(preset, Some("plan"))
        .expect("region")
        .expect("plan region");
    let chat = select(preset, Some(region.id), OPENAI, None, "").expect("chat");
    assert_eq!(chat.base_url, "https://tokenhub.tencentmaas.com/plan/v3");
    let responses = select(
        preset,
        Some(region.id),
        &[hofund_plugin_api::ProtocolId::OPENAI_RESPONSES][..],
        None,
        "",
    )
    .expect("responses");
    assert_eq!(
        responses.base_url,
        "https://tokenhub.tencentmaas.com/plan/v3"
    );
    let messages = select(
        preset,
        Some(region.id),
        &[hofund_plugin_api::ProtocolId::ANTHROPIC_MESSAGES][..],
        None,
        "",
    )
    .expect("messages");
    assert_eq!(
        messages.base_url,
        "https://tokenhub.tencentmaas.com/plan/anthropic"
    );
    let models = region.models.unwrap_or(&[]);
    assert!(models.contains(&"auto"));
    assert!(models.contains(&"mimo-v2.6-pro"));
}
