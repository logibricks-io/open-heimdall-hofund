//! Parity checks for the data-only OpenAI-compatible preset batches.

use hofund_plugin_api::ProtocolId;
use hofund_plugin_registry::{builtin_protocols, find_preset, model_plan_of, select};
use hofund_provider::{Credential, ModelPlan, Resolve, Wire, catalog_ids, credential_for, resolve};

const OVERRIDE: &str = "https://override.example.test/v1";

struct Row {
    id: &'static str,
    name: &'static str,
    kind: &'static str,
    catalog: &'static str,
    note: &'static str,
    chat: &'static str,
    responses: &'static str,
    anthropic: &'static str,
    website: &'static str,
    keys_url: &'static str,
}

const ROWS: &[Row] = &[
    // reference: internal/provider/presets.go:525
    Row {
        id: "302ai",
        name: "302.AI",
        kind: "relay",
        catalog: "",
        note: "",
        chat: "https://api.302.ai/v1",
        responses: "",
        anthropic: "https://api.302.ai",
        website: "https://302.ai",
        keys_url: "https://302.ai/api-keys/list",
    },
    // reference: internal/provider/presets.go:515
    Row {
        id: "aihubmix",
        name: "AiHubMix",
        kind: "relay",
        catalog: "",
        note: "",
        chat: "https://aihubmix.com/v1",
        responses: "",
        anthropic: "https://aihubmix.com",
        website: "https://aihubmix.com",
        keys_url: "https://console.aihubmix.com/token",
    },
    // reference: internal/provider/presets.go:528
    Row {
        id: "cherryin",
        name: "CherryIN",
        kind: "relay",
        catalog: "",
        note: "",
        chat: "https://open.cherryin.ai/v1",
        responses: "https://open.cherryin.ai/v1",
        anthropic: "https://open.cherryin.ai",
        website: "https://open.cherryin.ai",
        keys_url: "https://open.cherryin.ai/console/token",
    },
    // reference: internal/provider/presets.go:486
    Row {
        id: "commandcode",
        name: "Command Code",
        kind: "relay",
        catalog: "",
        note: "",
        chat: "https://api.commandcode.ai/provider/v1",
        responses: "https://api.commandcode.ai/provider/v1",
        anthropic: "https://api.commandcode.ai/provider",
        website: "https://commandcode.ai/docs/provider",
        keys_url: "https://commandcode.ai/settings/keys",
    },
    // reference: internal/provider/presets.go:492
    Row {
        id: "fireworks",
        name: "Fireworks",
        kind: "relay",
        catalog: "fireworks-ai",
        note: "",
        chat: "https://api.fireworks.ai/inference/v1",
        responses: "",
        anthropic: "",
        website: "https://fireworks.ai",
        keys_url: "https://app.fireworks.ai/settings/users/api-keys",
    },
    // reference: internal/provider/presets.go:511
    Row {
        id: "modelscope",
        name: "ModelScope",
        kind: "relay",
        catalog: "modelscope",
        note: "API-Inference. Not the DashScope endpoints.",
        chat: "https://api-inference.modelscope.cn/v1",
        responses: "https://api-inference.modelscope.cn/v1",
        anthropic: "",
        website: "https://modelscope.cn/docs/model-service/API-Inference/intro",
        keys_url: "https://modelscope.cn/my/myaccesstoken",
    },
    // reference: internal/provider/presets.go:506
    Row {
        id: "nvidia",
        name: "NVIDIA NIM",
        kind: "relay",
        catalog: "nvidia",
        note: "",
        chat: "https://integrate.api.nvidia.com/v1",
        responses: "",
        anthropic: "",
        website: "https://build.nvidia.com",
        keys_url: "https://build.nvidia.com/settings/api-keys",
    },
    // reference: internal/provider/presets.go:449
    Row {
        id: "opencode-go",
        name: "OpenCode Go",
        kind: "relay",
        catalog: "opencode-go",
        note: "open coding models, $10/month",
        chat: "https://opencode.ai/zen/go/v1",
        responses: "https://opencode.ai/zen/go/v1",
        anthropic: "https://opencode.ai/zen/go",
        website: "https://opencode.ai/docs/go",
        keys_url: "https://opencode.ai/auth",
    },
    // reference: internal/provider/presets.go:522
    Row {
        id: "pipellm",
        name: "PipeLLM",
        kind: "relay",
        catalog: "",
        note: "",
        chat: "https://api.pipellm.ai/openai/v1",
        responses: "https://api.pipellm.ai/v1",
        anthropic: "https://api.pipellm.ai",
        website: "https://www.pipellm.ai",
        keys_url: "https://console.pipellm.ai",
    },
    // reference: internal/provider/presets.go:489
    Row {
        id: "together",
        name: "Together AI",
        kind: "relay",
        catalog: "togetherai",
        note: "",
        chat: "https://api.together.xyz/v1",
        responses: "",
        anthropic: "",
        website: "https://api.together.ai",
        keys_url: "https://api.together.ai/settings/api-keys",
    },
    // reference: internal/provider/presets.go:147
    Row {
        id: "xai",
        name: "xAI",
        kind: "vendor",
        catalog: "xai",
        note: "",
        chat: "https://api.x.ai/v1",
        responses: "https://api.x.ai/v1",
        anthropic: "https://api.x.ai",
        website: "https://console.x.ai",
        keys_url: "https://console.x.ai",
    },
    // reference: internal/provider/presets.go:150
    Row {
        id: "moonshot",
        name: "Kimi",
        kind: "vendor",
        catalog: "moonshotai",
        note: "",
        chat: "https://api.moonshot.ai/v1",
        responses: "",
        anthropic: "https://api.moonshot.ai/anthropic",
        website: "https://platform.moonshot.ai",
        keys_url: "https://platform.moonshot.ai/console/api-keys",
    },
    // reference: internal/provider/presets.go:153
    Row {
        id: "moonshot-cn",
        name: "Kimi (China)",
        kind: "vendor",
        catalog: "moonshotai",
        note: "",
        chat: "https://api.moonshot.cn/v1",
        responses: "",
        anthropic: "https://api.moonshot.cn/anthropic",
        website: "https://platform.moonshot.cn",
        keys_url: "https://platform.moonshot.cn/console/api-keys",
    },
    // reference: internal/provider/presets.go:158
    Row {
        id: "kimi-code",
        name: "Kimi Code",
        kind: "vendor",
        catalog: "kimi-code-plan-global",
        note: "Membership",
        chat: "https://api.kimi.ai/coding/v1",
        responses: "",
        anthropic: "https://api.kimi.ai/coding",
        website: "https://www.kimi.com/code",
        keys_url: "https://www.kimi.com/code/console",
    },
    // reference: internal/provider/presets.go:162
    Row {
        id: "kimi-code-cn",
        name: "Kimi Code (China)",
        kind: "vendor",
        catalog: "kimi-code-plan-cn",
        note: "Membership",
        chat: "https://api.kimi.com/coding/v1",
        responses: "",
        anthropic: "https://api.kimi.com/coding",
        website: "https://www.kimi.com/code",
        keys_url: "https://www.kimi.com/code/console",
    },
    // reference: internal/provider/presets.go:184
    Row {
        id: "minimax",
        name: "MiniMax",
        kind: "vendor",
        catalog: "minimax",
        note: "",
        chat: "https://api.minimax.io/v1",
        responses: "",
        anthropic: "https://api.minimax.io/anthropic",
        website: "https://platform.minimax.io",
        keys_url: "https://platform.minimax.io/user-center/basic-information/interface-key",
    },
    // reference: internal/provider/presets.go:187
    Row {
        id: "minimax-cn",
        name: "MiniMax (China)",
        kind: "vendor",
        catalog: "minimax",
        note: "",
        chat: "https://api.minimaxi.com/v1",
        responses: "",
        anthropic: "https://api.minimaxi.com/anthropic",
        website: "https://platform.minimaxi.com",
        keys_url: "https://platform.minimaxi.com/user-center/basic-information/interface-key",
    },
    // reference: internal/provider/presets.go:333
    Row {
        id: "qwen",
        name: "Qwen",
        kind: "vendor",
        catalog: "alibaba",
        note: "DashScope - intl",
        chat: "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
        responses: "",
        anthropic: "https://dashscope-intl.aliyuncs.com/apps/anthropic",
        website: "https://modelstudio.console.alibabacloud.com",
        keys_url: "https://modelstudio.console.alibabacloud.com/?tab=playground#/api-key",
    },
    // reference: internal/provider/presets.go:337
    Row {
        id: "qwen-cn",
        name: "Qwen (China)",
        kind: "vendor",
        catalog: "alibaba",
        note: "DashScope - China",
        chat: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        responses: "",
        anthropic: "https://dashscope.aliyuncs.com/apps/anthropic",
        website: "https://bailian.console.aliyun.com",
        keys_url: "https://bailian.console.aliyun.com/?tab=model#/api-key",
    },
    // reference: internal/provider/presets.go:364
    Row {
        id: "mistral",
        name: "Mistral",
        kind: "vendor",
        catalog: "mistral",
        note: "",
        chat: "https://api.mistral.ai/v1",
        responses: "",
        anthropic: "",
        website: "https://console.mistral.ai",
        keys_url: "https://console.mistral.ai/api-keys",
    },
];

fn endpoint(row: &Row, wire: Wire) -> &'static str {
    match wire {
        Wire::Chat => row.chat,
        Wire::Responses => row.responses,
        Wire::Messages => row.anthropic,
        Wire::Generative | Wire::Vertex | Wire::Converse => "",
    }
}

fn served(row: &Row) -> impl Iterator<Item = (ProtocolId, Wire)> {
    [
        (ProtocolId::OPENAI_CHAT, Wire::Chat),
        (ProtocolId::OPENAI_RESPONSES, Wire::Responses),
        (ProtocolId::ANTHROPIC_MESSAGES, Wire::Messages),
    ]
    .into_iter()
    .filter(|(_, wire)| !endpoint(row, *wire).is_empty())
}

fn expected_credential(wire: Wire) -> Credential {
    if wire == Wire::Messages {
        Credential::MessagesBoth
    } else {
        Credential::Bearer
    }
}

fn expected_model_plan(row: &Row) -> ModelPlan {
    ModelPlan {
        chat: row.chat.to_owned(),
        responses: row.responses.to_owned(),
        anthropic: row.anthropic.to_owned(),
        generative: String::new(),
        no_list: false,
        lists: false,
        only: String::new(),
        models: Vec::new(),
        quirks: None,
        extra_body: None,
    }
}

#[test]
fn batch_rows_live_in_openai_compat_and_left_the_catalog() {
    let catalog = catalog_ids();
    for row in ROWS {
        let (preset, region) = find_preset(row.id).expect("compiled batch row");
        assert_eq!(preset.plugin, "openai-compat");
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
fn batch_rows_match_the_reference_literal() {
    for row in ROWS {
        let (preset, _) = find_preset(row.id).expect("compiled batch row");
        assert_eq!(preset.id, row.id);
        assert_eq!(preset.name, row.name);
        assert_eq!(preset.kind, row.kind);
        assert_eq!(preset.catalog, row.catalog);
        assert_eq!(preset.note, row.note);
        assert_eq!(preset.chat, row.chat);
        assert_eq!(preset.responses, row.responses);
        assert_eq!(preset.anthropic, row.anthropic);
        assert_eq!(preset.website, row.website);
        assert_eq!(preset.keys_url, row.keys_url);
    }
}

#[test]
fn batch_rows_resolve_like_the_old_catalog() {
    let protocols = builtin_protocols();
    for row in ROWS {
        let (preset, _) = find_preset(row.id).expect("compiled batch row");
        let cases = std::iter::once((None, Wire::Chat))
            .chain(served(row).map(|(protocol, wire)| (Some(protocol), wire)));
        for (protocol, wire) in cases {
            for base_url in ["", OVERRIDE] {
                let selected = select(preset, None, &protocols, protocol, base_url)
                    .expect("old catalog resolution");
                let expected_base = if base_url.is_empty() {
                    endpoint(row, wire)
                } else {
                    OVERRIDE
                };
                assert_eq!(
                    selected.protocol,
                    protocol.unwrap_or(ProtocolId::OPENAI_CHAT)
                );
                assert_eq!(selected.wire, wire, "{}", row.id);
                assert_eq!(selected.base_url, expected_base, "{}", row.id);
                assert_eq!(
                    credential_for(row.id, selected.wire, &selected.base_url),
                    expected_credential(wire),
                    "{} {wire:?} {base_url:?}",
                    row.id
                );
            }
        }
    }
}

#[test]
fn batch_rows_model_plan_like_the_old_catalog() {
    for row in ROWS {
        let (preset, _) = find_preset(row.id).expect("compiled batch row");
        assert_eq!(model_plan_of(preset, None), expected_model_plan(row));
    }
}
