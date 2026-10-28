//! Compiled preset fragments and catalog-equivalent selection.

use std::collections::BTreeSet;

use hofund_plugin_api::{DynUpstreamFactory, ProtocolId};
use hofund_plugin_registry::{
    CompiledAlias, CompiledPreset, compiled_aliases, compiled_presets, factory,
};
#[cfg(feature = "test-fixtures")]
use hofund_plugin_registry::{
    find_preset, fixture_aliases, fixture_presets, model_plan_of, resolve_region, select,
};
#[cfg(feature = "test-fixtures")]
use hofund_provider::{Resolve, Wire};

#[cfg(feature = "test-fixtures")]
const OPENAI: &[ProtocolId] = &[ProtocolId::OPENAI_CHAT, ProtocolId::OPENAI_RESPONSES];
#[cfg(feature = "test-fixtures")]
const ALL: &[ProtocolId] = &[
    ProtocolId::GENERATIVE_LANGUAGE,
    ProtocolId::OPENAI_CHAT,
    ProtocolId::OPENAI_RESPONSES,
    ProtocolId::ANTHROPIC_MESSAGES,
];

#[cfg(feature = "test-fixtures")]
fn fixture(id: &str) -> &'static CompiledPreset {
    fixture_presets()
        .iter()
        .find(|preset| preset.id == id)
        .expect("fixture preset")
}

#[cfg(feature = "test-fixtures")]
fn protocol(wire: Wire) -> ProtocolId {
    match wire {
        Wire::Chat => ProtocolId::OPENAI_CHAT,
        Wire::Responses => ProtocolId::OPENAI_RESPONSES,
        Wire::Messages => ProtocolId::ANTHROPIC_MESSAGES,
        Wire::Generative => ProtocolId::GENERATIVE_LANGUAGE,
        Wire::Vertex | Wire::Converse => panic!("not a fragment wire"),
    }
}

#[cfg(feature = "test-fixtures")]
#[test]
fn fixtures_are_separate_and_aliases_resolve_their_region() {
    assert_eq!(compiled_presets().len(), 1);
    assert!(compiled_aliases().is_empty());
    assert!(fixture_presets().len() >= 5);
    assert_eq!(fixture_aliases().len(), 1);
    let alias = fixture_aliases()[0];
    assert_eq!(alias.id, "fixture-alias");
    assert_eq!(alias.preset, "fixture-multi");
    assert_eq!(alias.region, Some("second"));
}

#[cfg(feature = "test-fixtures")]
#[test]
fn default_selection_follows_wire_order() {
    for (index, wire) in [
        Wire::Generative,
        Wire::Chat,
        Wire::Responses,
        Wire::Messages,
    ]
    .into_iter()
    .enumerate()
    {
        let mut copy = CompiledPreset {
            generative: "https://generative.example.test",
            anthropic: "https://messages.example.test",
            ..*fixture("fixture-multi")
        };
        if index > 0 {
            copy.generative = "";
        }
        if index > 1 {
            copy.chat = "";
        }
        if index > 2 {
            copy.responses = "";
        }
        let selected = select(Box::leak(Box::new(copy)), None, ALL, None, "").expect("selection");
        assert_eq!(selected.wire, wire);
    }
}

#[cfg(feature = "test-fixtures")]
#[test]
fn selected_protocol_must_be_served() {
    let error = select(
        fixture("fixture-multi"),
        None,
        OPENAI,
        Some(ProtocolId::ANTHROPIC_MESSAGES),
        "",
    )
    .expect_err("unserved protocol");
    let text = error.to_string();
    assert!(text.contains("preset `fixture-multi`"), "{text}");
    assert!(text.contains("plugin `openai-compat`"), "{text}");
    assert!(
        text.contains("does not speak `anthropic-messages`"),
        "{text}"
    );
    assert!(text.contains("openai-chat, or openai-responses"), "{text}");
}

#[cfg(feature = "test-fixtures")]
#[test]
fn selected_wire_must_have_a_url() {
    let error = select(
        fixture("fixture-messages"),
        None,
        ALL,
        Some(ProtocolId::OPENAI_RESPONSES),
        "",
    )
    .expect_err("missing responses URL");
    assert_eq!(
        error.to_string(),
        "preset `fixture-messages` has no responses endpoint"
    );
}

#[cfg(feature = "test-fixtures")]
#[test]
fn region_urls_override_per_wire_and_fall_back() {
    let selected = select(
        fixture("fixture-multi"),
        Some("second"),
        OPENAI,
        Some(ProtocolId::OPENAI_RESPONSES),
        "",
    )
    .expect("responses fallback");
    assert_eq!(selected.base_url, "https://first.example.test/v1");
    let selected = select(
        fixture("fixture-multi"),
        Some("second"),
        OPENAI,
        Some(ProtocolId::OPENAI_CHAT),
        "",
    )
    .expect("chat override");
    assert_eq!(selected.base_url, "https://second.example.test/v1");
}

#[cfg(feature = "test-fixtures")]
#[test]
fn unknown_region_lists_ids_in_declared_order() {
    let error = select(fixture("fixture-multi"), Some("missing"), OPENAI, None, "")
        .expect_err("unknown region");
    assert_eq!(
        error.to_string(),
        "preset `fixture-multi` has no region `missing`; expected first, or second"
    );
    let error = select(
        fixture("fixture-messages"),
        Some("first"),
        &[ProtocolId::ANTHROPIC_MESSAGES],
        None,
        "",
    )
    .expect_err("regionless preset");
    assert_eq!(
        error.to_string(),
        "preset `fixture-messages` has no regions"
    );
}

#[cfg(feature = "test-fixtures")]
#[test]
fn caller_base_url_overrides_the_selected_url() {
    let selected = select(
        fixture("fixture-multi"),
        None,
        OPENAI,
        None,
        "https://override.example.test/v1",
    )
    .expect("override");
    assert_eq!(selected.base_url, "https://override.example.test/v1");
}

#[cfg(feature = "test-fixtures")]
#[test]
fn relay_no_url_preset_requires_a_base_url() {
    let relay = hofund_plugin_registry::preset("relay").expect("relay preset");
    let error = select(relay, None, OPENAI, None, "").expect_err("missing base URL");
    assert_eq!(error.to_string(), "preset `relay` needs base_url");
    let selected = select(
        relay,
        None,
        OPENAI,
        Some(ProtocolId::OPENAI_RESPONSES),
        "https://relay.example.test/v1",
    )
    .expect("explicit base URL");
    assert_eq!(selected.wire, Wire::Responses);
    assert_eq!(selected.base_url, "https://relay.example.test/v1");
}

#[cfg(feature = "test-fixtures")]
#[test]
fn builtin_no_url_presets_match_catalog_endpoint_rules() {
    let served = hofund_plugin_registry::builtin_protocols();
    let preset = fixture("fixture-no-url");
    let selected = select(preset, None, &served, None, "https://relay.example.test/v1")
        .expect("builtin default");
    assert_eq!(selected.wire, Wire::Chat);
    assert_eq!(selected.protocol, ProtocolId::OPENAI_CHAT);
    assert_eq!(selected.base_url, "https://relay.example.test/v1");

    let error = select(preset, None, &served, None, "").expect_err("missing base URL");
    assert_eq!(error.to_string(), "preset `fixture-no-url` needs base_url");

    let error = select(
        preset,
        None,
        &served,
        Some(ProtocolId::OPENAI_RESPONSES),
        "https://relay.example.test/v1",
    )
    .expect_err("missing responses endpoint");
    assert_eq!(
        error.to_string(),
        "preset `fixture-no-url` has no responses endpoint"
    );

    let without_endpoint = Box::leak(Box::new(CompiledPreset {
        endpoint: "",
        ..*preset
    }));
    let error = select(
        without_endpoint,
        None,
        &served,
        None,
        "https://relay.example.test/v1",
    )
    .expect_err("missing endpoint declaration");
    assert_eq!(error.to_string(), "preset `fixture-no-url` has no endpoint");
}

#[cfg(feature = "test-fixtures")]
#[test]
fn decide_only_preset_is_not_callable() {
    for protocol in [None, Some(ProtocolId::OPENAI_CHAT)] {
        let error =
            select(fixture("fixture-decide"), None, OPENAI, protocol, "").expect_err("decide only");
        assert_eq!(
            error.to_string(),
            "preset `fixture-decide` only classifies a route. It has no chat, responses, or messages endpoint."
        );
    }
}

#[cfg(feature = "test-fixtures")]
#[test]
fn alias_region_is_default_and_user_region_wins() {
    let alias = fixture_aliases()[0];
    let preset = fixture(alias.preset);
    let selected = select(preset, alias.region, OPENAI, None, "").expect("alias region");
    assert_eq!(selected.region.map(|region| region.id), Some("second"));
    assert_eq!(selected.base_url, "https://second.example.test/v1");
    let selected =
        select(preset, Some("first").or(alias.region), OPENAI, None, "").expect("user region");
    assert_eq!(selected.region.map(|region| region.id), Some("first"));
    assert_eq!(selected.base_url, "https://first.example.test/v1");
}

#[cfg(feature = "test-fixtures")]
#[test]
fn region_models_replace_preset_models() {
    let preset = fixture("fixture-multi");
    let region = resolve_region(preset, Some("second")).expect("region");
    let plan = model_plan_of(preset, region);
    assert_eq!(plan.models, ["fixture-region"]);
    assert_eq!(plan.chat, "https://second.example.test/v1");
    assert_eq!(plan.responses, "https://first.example.test/v1");
    assert!(plan.lists);
    let plan = model_plan_of(preset, None);
    assert_eq!(plan.models, ["fixture-one", "fixture-two"]);
    assert!(!plan.lists);
}

#[cfg(feature = "test-fixtures")]
#[test]
fn copied_catalog_row_resolves_identically() {
    let preset = fixture("fixture-equivalence");
    for region in [None, Some("plan"), Some("cn"), Some("intl")] {
        let compiled_region = resolve_region(preset, region).expect("compiled region");
        let expected_plan = hofund_provider::model_plan("tencent-cloud", region)
            .expect("catalog plan")
            .expect("catalog preset");
        assert_eq!(model_plan_of(preset, compiled_region), expected_plan);
        for wire in [Wire::Chat, Wire::Responses, Wire::Messages] {
            let expected = hofund_provider::resolve(&Resolve {
                id: "tencent-cloud",
                region,
                wire: Some(wire),
                base_url: "",
                project: "",
                location: "",
            });
            let actual = select(preset, region, ALL, Some(protocol(wire)), "");
            match expected {
                Ok(expected) => {
                    let actual = actual.expect("compiled selection");
                    assert_eq!(actual.wire, expected.wire);
                    assert_eq!(actual.base_url, expected.base_url);
                    assert_eq!(
                        hofund_provider::credential_for(preset.id, wire, &actual.base_url),
                        expected.credential
                    );
                }
                Err(error) => {
                    assert!(actual.is_err(), "{region:?} {wire:?}: {error}");
                }
            }
        }
    }
}

fn assert_wires_and_ids(presets: &[CompiledPreset], aliases: &[CompiledAlias]) {
    let catalog: BTreeSet<&str> = hofund_provider::catalog_ids().into_iter().collect();
    for preset in presets {
        assert!(!catalog.contains(preset.id), "duplicate {}", preset.id);
        let served = if hofund_plugin_registry::builtin_factory_ids().contains(&preset.plugin) {
            hofund_plugin_registry::builtin_protocols()
        } else {
            let upstream = factory(preset.plugin).expect("preset factory");
            DynUpstreamFactory::spec(upstream.as_ref())
                .protocols()
                .to_vec()
        };
        for (url, protocol) in [
            (preset.chat, ProtocolId::OPENAI_CHAT),
            (preset.responses, ProtocolId::OPENAI_RESPONSES),
            (preset.anthropic, ProtocolId::ANTHROPIC_MESSAGES),
            (preset.generative, ProtocolId::GENERATIVE_LANGUAGE),
        ] {
            assert!(
                url.is_empty() || served.contains(&protocol),
                "{}",
                preset.id
            );
        }
        for region in preset.regions {
            for (url, protocol) in [
                (region.chat, ProtocolId::OPENAI_CHAT),
                (region.responses, ProtocolId::OPENAI_RESPONSES),
                (region.anthropic, ProtocolId::ANTHROPIC_MESSAGES),
            ] {
                assert!(
                    url.is_empty() || served.contains(&protocol),
                    "{}",
                    preset.id
                );
            }
        }
    }
    for alias in aliases {
        assert!(!catalog.contains(alias.id), "duplicate {}", alias.id);
    }
}

#[test]
fn compiled_wires_are_served_and_ids_do_not_collide_with_the_catalog() {
    assert_wires_and_ids(compiled_presets(), compiled_aliases());
}

#[cfg(feature = "test-fixtures")]
#[test]
fn fixture_wires_are_served_and_ids_do_not_collide_with_the_catalog() {
    assert_wires_and_ids(fixture_presets(), fixture_aliases());
}

#[cfg(feature = "test-fixtures")]
#[test]
fn an_empty_region_wire_falls_back_to_the_preset() {
    let preset = fixture("fixture-fallback");
    let chat = select(
        preset,
        Some("only-responses"),
        OPENAI,
        Some(ProtocolId::OPENAI_CHAT),
        "",
    )
    .expect("fallback chat");
    assert_eq!(chat.base_url, "https://fallback.example.test/v1");
    let responses = select(
        preset,
        Some("only-responses"),
        OPENAI,
        Some(ProtocolId::OPENAI_RESPONSES),
        "",
    )
    .expect("region responses");
    assert_eq!(responses.base_url, "https://region.example.test/v1");
}

#[cfg(feature = "test-fixtures")]
#[test]
fn default_selection_skips_unserved_wires() {
    let selected = select(
        fixture("fixture-equivalence"),
        None,
        &[ProtocolId::ANTHROPIC_MESSAGES],
        None,
        "",
    )
    .expect("served messages");
    assert_eq!(selected.wire, Wire::Messages);
    assert_eq!(
        selected.base_url,
        "https://api.lkeap.cloud.tencent.com/plan/anthropic"
    );
}

#[cfg(feature = "test-fixtures")]
#[test]
fn explicit_protocol_on_a_relay_no_url_preset_requires_a_base_url() {
    let relay = hofund_plugin_registry::preset("relay").expect("relay preset");
    let error = select(relay, None, OPENAI, Some(ProtocolId::OPENAI_RESPONSES), "")
        .expect_err("missing base URL");
    assert_eq!(error.to_string(), "preset `relay` needs base_url");
}

#[cfg(feature = "test-fixtures")]
#[test]
fn fixture_lookup_is_separate_from_production_lookup() {
    assert!(find_preset("fixture-multi").is_none());
    for preset in fixture_presets() {
        assert!(find_preset(preset.id).is_none(), "{}", preset.id);
        let (found, region) =
            hofund_plugin_registry::find_fixture_preset(preset.id).expect("fixture preset lookup");
        assert_eq!(found.id, preset.id);
        assert_eq!(region, None);
    }
    for alias in fixture_aliases() {
        assert!(find_preset(alias.id).is_none(), "{}", alias.id);
        let (found, region) =
            hofund_plugin_registry::find_fixture_preset(alias.id).expect("fixture alias lookup");
        assert_eq!(found.id, alias.preset);
        assert_eq!(region, alias.region);
    }
}
