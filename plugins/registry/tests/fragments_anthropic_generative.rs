//! Parity checks for the data-only Anthropic and Generative preset batch.

use hofund_plugin_api::ProtocolId;
use hofund_plugin_registry::{builtin_protocols, find_preset, model_plan_of, select};
use hofund_provider::{Credential, ModelPlan, Resolve, Wire, catalog_ids, credential_for, resolve};

const OVERRIDE: &str = "https://override.example.test/v1";

struct Success {
    protocol: Option<ProtocolId>,
    wire: Wire,
    base_url: &'static str,
    credential: Credential,
}

struct Row {
    id: &'static str,
    plugin: &'static str,
    name: &'static str,
    kind: &'static str,
    catalog: &'static str,
    note: &'static str,
    chat: &'static str,
    responses: &'static str,
    anthropic: &'static str,
    generative: &'static str,
    website: &'static str,
    keys_url: &'static str,
    header_hints: &'static [&'static str],
    successes: &'static [Success],
    errors: &'static [(ProtocolId, &'static str)],
}

const ROWS: &[Row] = &[
    // reference: internal/provider/presets.go:126
    // generative is a hofund extension; the reference row has Chat only.
    Row {
        id: "google",
        plugin: "generative",
        name: "Google Gemini",
        kind: "vendor",
        catalog: "google",
        note: "Gemini Developer API",
        chat: "https://generativelanguage.googleapis.com/v1beta/openai",
        responses: "",
        anthropic: "",
        generative: "https://generativelanguage.googleapis.com/v1beta",
        website: "https://aistudio.google.com",
        keys_url: "https://aistudio.google.com/apikey",
        header_hints: &[],
        successes: &[
            Success {
                protocol: None,
                wire: Wire::Generative,
                base_url: "https://generativelanguage.googleapis.com/v1beta",
                credential: Credential::GoogApiKey,
            },
            Success {
                protocol: Some(ProtocolId::OPENAI_CHAT),
                wire: Wire::Chat,
                base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
                credential: Credential::Bearer,
            },
            Success {
                protocol: Some(ProtocolId::GENERATIVE_LANGUAGE),
                wire: Wire::Generative,
                base_url: "https://generativelanguage.googleapis.com/v1beta",
                credential: Credential::GoogApiKey,
            },
        ],
        errors: &[
            (
                ProtocolId::OPENAI_RESPONSES,
                "preset `google` has no responses endpoint",
            ),
            (
                ProtocolId::ANTHROPIC_MESSAGES,
                "preset `google` has no messages endpoint",
            ),
            (
                ProtocolId::BEDROCK_CONVERSE,
                "preset `google` is served by plugin `generative`, which does not speak `bedrock-converse`; expected openai-chat, openai-responses, anthropic-messages, or generative-language",
            ),
        ],
    },
    // reference: internal/provider/presets.go:118
    Row {
        id: "anthropic",
        plugin: "anthropic",
        name: "Anthropic",
        kind: "vendor",
        catalog: "anthropic",
        note: "",
        chat: "",
        responses: "",
        anthropic: "https://api.anthropic.com",
        generative: "",
        website: "https://console.anthropic.com",
        keys_url: "https://console.anthropic.com/settings/keys",
        header_hints: &["anthropic-workspace-id"],
        successes: &[
            Success {
                protocol: None,
                wire: Wire::Messages,
                base_url: "https://api.anthropic.com",
                credential: Credential::XApiKey,
            },
            Success {
                protocol: Some(ProtocolId::ANTHROPIC_MESSAGES),
                wire: Wire::Messages,
                base_url: "https://api.anthropic.com",
                credential: Credential::XApiKey,
            },
        ],
        errors: &[
            (
                ProtocolId::OPENAI_CHAT,
                "preset `anthropic` has no chat endpoint",
            ),
            (
                ProtocolId::OPENAI_RESPONSES,
                "preset `anthropic` has no responses endpoint",
            ),
            (
                ProtocolId::GENERATIVE_LANGUAGE,
                "preset `anthropic` has no generative endpoint",
            ),
            (
                ProtocolId::BEDROCK_CONVERSE,
                "preset `anthropic` is served by plugin `anthropic`, which does not speak `bedrock-converse`; expected openai-chat, openai-responses, anthropic-messages, or generative-language",
            ),
        ],
    },
];

#[test]
fn batch7_rows_live_with_default_wire_factories_and_left_catalog() {
    let catalog = catalog_ids();
    for row in ROWS {
        let (preset, region) = find_preset(row.id).expect("compiled batch 7 row");
        assert_eq!(preset.plugin, row.plugin);
        assert_eq!(region, None);
        assert!(!catalog.contains(&row.id), "{}", row.id);
        let error = resolve(&Resolve {
            id: row.id,
            region: None,
            wire: None,
            base_url: "",
            project: "",
            location: "",
        })
        .expect_err("row left the built-in catalog");
        assert_eq!(error.to_string(), format!("unknown preset `{}`", row.id));
    }
}

#[test]
fn batch7_rows_match_the_reference_literal() {
    for row in ROWS {
        let (preset, _) = find_preset(row.id).expect("compiled batch 7 row");
        assert_eq!(preset.id, row.id);
        assert_eq!(preset.name, row.name);
        assert_eq!(preset.kind, row.kind);
        assert_eq!(preset.catalog, row.catalog);
        assert_eq!(preset.note, row.note);
        assert_eq!(preset.chat, row.chat);
        assert_eq!(preset.responses, row.responses);
        assert_eq!(preset.anthropic, row.anthropic);
        assert_eq!(preset.generative, row.generative);
        assert_eq!(preset.website, row.website);
        assert_eq!(preset.keys_url, row.keys_url);
        assert_eq!(preset.header_hints, row.header_hints);
        assert_eq!(preset.short, "");
        assert_eq!(preset.site_host, "");
        assert_eq!(preset.key_hint, "");
        assert!(!preset.no_key);
        assert!(!preset.no_list);
        assert!(!preset.hosts);
        assert_eq!(preset.only, "");
        assert!(preset.models.is_empty());
        assert_eq!(preset.endpoint, "");
        assert_eq!(preset.endpoint_hint, "");
        assert_eq!(preset.endpoint_needed, "");
        assert_eq!(preset.region_label, "");
        assert_eq!(preset.decide, "");
        assert!(preset.config.is_empty());
        assert!(preset.regions.is_empty());
    }
}

#[test]
fn batch7_rows_select_like_the_old_catalog() {
    let protocols = builtin_protocols();
    for row in ROWS {
        let (preset, _) = find_preset(row.id).expect("compiled batch 7 row");
        for success in row.successes {
            for base_url in ["", OVERRIDE] {
                let selected = select(preset, None, &protocols, success.protocol, base_url)
                    .expect("old catalog resolution");
                let expected_protocol = success.protocol.unwrap_or_else(|| match success.wire {
                    Wire::Generative => ProtocolId::GENERATIVE_LANGUAGE,
                    Wire::Messages => ProtocolId::ANTHROPIC_MESSAGES,
                    _ => panic!("batch 7 default wire"),
                });
                let expected_base = if base_url.is_empty() {
                    success.base_url
                } else {
                    OVERRIDE
                };
                let expected_credential = if row.id == "anthropic" && base_url == OVERRIDE {
                    Credential::MessagesBoth
                } else {
                    success.credential
                };
                assert_eq!(selected.protocol, expected_protocol, "{}", row.id);
                assert_eq!(selected.wire, success.wire, "{}", row.id);
                assert_eq!(selected.base_url, expected_base, "{}", row.id);
                assert_eq!(selected.region, None);
                assert_eq!(
                    credential_for(row.id, selected.wire, &selected.base_url),
                    expected_credential,
                    "{} {base_url}",
                    row.id
                );
            }
        }
        for (protocol, expected) in row.errors {
            for base_url in ["", OVERRIDE] {
                let error = select(preset, None, &protocols, Some(*protocol), base_url)
                    .expect_err("unserved wire must fail even with a base override");
                assert_eq!(error.to_string(), *expected, "{} {base_url}", row.id);
            }
        }
    }
}

#[test]
fn batch7_rows_reject_an_unknown_region_like_the_old_catalog() {
    for row in ROWS {
        let (preset, _) = find_preset(row.id).expect("compiled batch 7 row");
        let error = select(preset, Some("missing"), &builtin_protocols(), None, "")
            .expect_err("unknown region must fail");
        assert_eq!(
            error.to_string(),
            format!("preset `{}` has no regions", row.id)
        );
    }
}

#[test]
fn batch7_rows_model_plan_like_the_old_catalog() {
    for row in ROWS {
        let (preset, _) = find_preset(row.id).expect("compiled batch 7 row");
        assert_eq!(
            model_plan_of(preset, None),
            ModelPlan {
                chat: row.chat.to_owned(),
                responses: row.responses.to_owned(),
                anthropic: row.anthropic.to_owned(),
                generative: row.generative.to_owned(),
                no_list: false,
                lists: false,
                only: String::new(),
                models: Vec::new(),
                quirks: None,
                extra_body: None,
            }
        );
    }
}

#[test]
fn anthropic_workspace_host_keeps_x_api_key() {
    assert_eq!(
        credential_for(
            "anthropic",
            Wire::Messages,
            "https://workspace.anthropic.com"
        ),
        Credential::XApiKey
    );
}
