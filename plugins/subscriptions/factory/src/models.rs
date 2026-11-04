//! Factory model catalog from the reference Droid registry.

use hofund_plugin_api::{ModelList, ModelSpec, PluginError, ProtocolId};

struct CatalogModel {
    id: &'static str,
    name: &'static str,
    wire: ProtocolId,
    upstream: &'static str,
    context: u64,
    max_output: u64,
}

const MODELS: &[CatalogModel] = &[
    CatalogModel {
        id: "claude-fable-5.1",
        name: "Fable 5.1",
        wire: ProtocolId::ANTHROPIC_MESSAGES,
        upstream: "anthropic",
        context: 867_000,
        max_output: 128_000,
    },
    CatalogModel {
        id: "claude-fable-5",
        name: "Fable 5",
        wire: ProtocolId::ANTHROPIC_MESSAGES,
        upstream: "anthropic",
        context: 867_000,
        max_output: 128_000,
    },
    CatalogModel {
        id: "claude-opus-5-5",
        name: "Opus 5.5",
        wire: ProtocolId::ANTHROPIC_MESSAGES,
        upstream: "anthropic",
        context: 872_000,
        max_output: 128_000,
    },
    CatalogModel {
        id: "claude-opus-5",
        name: "Opus 5",
        wire: ProtocolId::ANTHROPIC_MESSAGES,
        upstream: "anthropic",
        context: 867_000,
        max_output: 128_000,
    },
    CatalogModel {
        id: "claude-opus-4-8",
        name: "Opus 4.8",
        wire: ProtocolId::ANTHROPIC_MESSAGES,
        upstream: "anthropic",
        context: 867_000,
        max_output: 128_000,
    },
    CatalogModel {
        id: "claude-sonnet-5-5",
        name: "Sonnet 5.5",
        wire: ProtocolId::ANTHROPIC_MESSAGES,
        upstream: "anthropic",
        context: 872_000,
        max_output: 128_000,
    },
    CatalogModel {
        id: "claude-sonnet-5",
        name: "Sonnet 5",
        wire: ProtocolId::ANTHROPIC_MESSAGES,
        upstream: "anthropic",
        context: 872_000,
        max_output: 128_000,
    },
    CatalogModel {
        id: "claude-sonnet-4-6",
        name: "Sonnet 4.6",
        wire: ProtocolId::ANTHROPIC_MESSAGES,
        upstream: "anthropic",
        context: 931_000,
        max_output: 64_000,
    },
    CatalogModel {
        id: "claude-haiku-5-5",
        name: "Haiku 5.5",
        wire: ProtocolId::ANTHROPIC_MESSAGES,
        upstream: "anthropic",
        context: 872_000,
        max_output: 128_000,
    },
    CatalogModel {
        id: "claude-haiku-4-5-20251001",
        name: "Haiku 4.5",
        wire: ProtocolId::ANTHROPIC_MESSAGES,
        upstream: "anthropic",
        context: 0,
        max_output: 0,
    },
    CatalogModel {
        id: "gpt-6-sol",
        name: "GPT-6 Sol",
        wire: ProtocolId::OPENAI_RESPONSES,
        upstream: "openai",
        context: 1_050_000,
        max_output: 128_000,
    },
    CatalogModel {
        id: "gpt-6-astra",
        name: "GPT-6 Astra",
        wire: ProtocolId::OPENAI_RESPONSES,
        upstream: "openai",
        context: 1_050_000,
        max_output: 128_000,
    },
    CatalogModel {
        id: "gpt-6-luna",
        name: "GPT-6 Luna",
        wire: ProtocolId::OPENAI_RESPONSES,
        upstream: "openai",
        context: 1_050_000,
        max_output: 128_000,
    },
    CatalogModel {
        id: "gpt-5.6-sol",
        name: "GPT-5.6 Sol",
        wire: ProtocolId::OPENAI_RESPONSES,
        upstream: "openai",
        context: 1_050_000,
        max_output: 128_000,
    },
    CatalogModel {
        id: "gpt-5.6-terra",
        name: "GPT-5.6 Terra",
        wire: ProtocolId::OPENAI_RESPONSES,
        upstream: "openai",
        context: 1_050_000,
        max_output: 128_000,
    },
    CatalogModel {
        id: "gpt-5.6-luna",
        name: "GPT-5.6 Luna",
        wire: ProtocolId::OPENAI_RESPONSES,
        upstream: "openai",
        context: 1_050_000,
        max_output: 128_000,
    },
    CatalogModel {
        id: "gpt-5.5",
        name: "GPT-5.5",
        wire: ProtocolId::OPENAI_RESPONSES,
        upstream: "openai",
        context: 1_050_000,
        max_output: 128_000,
    },
    CatalogModel {
        id: "gpt-5.4",
        name: "GPT-5.4",
        wire: ProtocolId::OPENAI_RESPONSES,
        upstream: "openai",
        context: 1_050_000,
        max_output: 128_000,
    },
    CatalogModel {
        id: "gpt-5.3-codex",
        name: "GPT-5.3-Codex",
        wire: ProtocolId::OPENAI_RESPONSES,
        upstream: "openai",
        context: 400_000,
        max_output: 128_000,
    },
    CatalogModel {
        id: "grok-4.7",
        name: "Grok 4.7",
        wire: ProtocolId::OPENAI_RESPONSES,
        upstream: "xai",
        context: 500_000,
        max_output: 63_356,
    },
    CatalogModel {
        id: "grok-4.6",
        name: "Grok 4.6",
        wire: ProtocolId::OPENAI_RESPONSES,
        upstream: "xai",
        context: 200_000,
        max_output: 63_356,
    },
    CatalogModel {
        id: "gemini-4-argon",
        name: "Gemini 4 Argon",
        wire: ProtocolId::GENERATIVE_LANGUAGE,
        upstream: "google",
        context: 1_000_000,
        max_output: 65_536,
    },
    CatalogModel {
        id: "gemini-3.1-pro-preview",
        name: "Gemini 3.1 Pro",
        wire: ProtocolId::GENERATIVE_LANGUAGE,
        upstream: "google",
        context: 1_000_000,
        max_output: 65_536,
    },
    CatalogModel {
        id: "gemini-3.8-flash",
        name: "Gemini 3.8 Flash",
        wire: ProtocolId::GENERATIVE_LANGUAGE,
        upstream: "google",
        context: 1_000_000,
        max_output: 65_536,
    },
    CatalogModel {
        id: "gemini-3.7-flash",
        name: "Gemini 3.7 Flash",
        wire: ProtocolId::GENERATIVE_LANGUAGE,
        upstream: "google",
        context: 1_000_000,
        max_output: 65_536,
    },
    CatalogModel {
        id: "gemini-3.6-flash",
        name: "Gemini 3.6 Flash",
        wire: ProtocolId::GENERATIVE_LANGUAGE,
        upstream: "google",
        context: 1_000_000,
        max_output: 65_536,
    },
    CatalogModel {
        id: "gemini-3.5-flash",
        name: "Gemini 3.5 Flash",
        wire: ProtocolId::GENERATIVE_LANGUAGE,
        upstream: "google",
        context: 1_000_000,
        max_output: 65_536,
    },
    CatalogModel {
        id: "gemini-3-flash-preview",
        name: "Gemini 3 Flash",
        wire: ProtocolId::GENERATIVE_LANGUAGE,
        upstream: "google",
        context: 1_000_000,
        max_output: 65_536,
    },
    CatalogModel {
        id: "glm-5.3",
        name: "GLM-5.3",
        wire: ProtocolId::OPENAI_CHAT,
        upstream: "fireworks",
        context: 1_040_000,
        max_output: 131_072,
    },
    CatalogModel {
        id: "glm-5.3-flash",
        name: "GLM-5.3-Flash",
        wire: ProtocolId::OPENAI_CHAT,
        upstream: "fireworks",
        context: 1_048_576,
        max_output: 131_072,
    },
    CatalogModel {
        id: "glm-5.2",
        name: "GLM-5.2",
        wire: ProtocolId::OPENAI_CHAT,
        upstream: "baseten",
        context: 1_040_000,
        max_output: 131_072,
    },
    CatalogModel {
        id: "kimi-k3",
        name: "Kimi K3",
        wire: ProtocolId::OPENAI_CHAT,
        upstream: "fireworks",
        context: 262_144,
        max_output: 65_536,
    },
    CatalogModel {
        id: "deepseek-v4.1-flash",
        name: "DeepSeek V4.1 Flash",
        wire: ProtocolId::OPENAI_CHAT,
        upstream: "fireworks",
        context: 1_040_000,
        max_output: 131_072,
    },
    CatalogModel {
        id: "qwen3.8-max",
        name: "Qwen3.8 Max",
        wire: ProtocolId::OPENAI_CHAT,
        upstream: "fireworks",
        context: 262_144,
        max_output: 131_072,
    },
    CatalogModel {
        id: "minimax-m3",
        name: "MiniMax M3",
        wire: ProtocolId::OPENAI_CHAT,
        upstream: "fireworks",
        context: 512_000,
        max_output: 64_000,
    },
    CatalogModel {
        id: "minimax-m2.7",
        name: "MiniMax M2.7",
        wire: ProtocolId::ANTHROPIC_MESSAGES,
        upstream: "fireworks",
        context: 196_600,
        max_output: 64_000,
    },
    CatalogModel {
        id: "mistral-large-4",
        name: "Mistral Large 4",
        wire: ProtocolId::OPENAI_CHAT,
        upstream: "mistral",
        context: 524_288,
        max_output: 64_000,
    },
    CatalogModel {
        id: "mistral-medium-3.5",
        name: "Mistral Medium 3.5",
        wire: ProtocolId::OPENAI_CHAT,
        upstream: "mistral",
        context: 256_000,
        max_output: 64_000,
    },
    CatalogModel {
        id: "nemotron-3-ultra",
        name: "Nemotron 3 Ultra",
        wire: ProtocolId::OPENAI_CHAT,
        upstream: "baseten",
        context: 202_000,
        max_output: 65_536,
    },
];

fn catalog_entry(id: &str) -> Option<&'static CatalogModel> {
    MODELS.iter().find(|model| model.id == id)
}

/// Static Factory model list.
pub(crate) fn catalog() -> Result<ModelList, PluginError> {
    let mut specs = Vec::with_capacity(MODELS.len());
    for model in MODELS {
        specs.push(
            ModelSpec::new(model.id, Some(model.name.to_owned()))?
                .with_context(model.context)
                .with_max_output(model.max_output),
        );
    }
    Ok(ModelList::new(specs, None))
}

/// Vendor name Factory expects in `x-api-provider` for one catalog model.
#[must_use]
pub(crate) fn x_api_provider(model: &str) -> Option<&'static str> {
    catalog_entry(model).map(|entry| entry.upstream)
}

/// Extra headers Factory expects on LLM routes for one catalog model.
#[must_use]
pub(crate) fn outbound_llm_headers(model: &str) -> Vec<(String, String)> {
    x_api_provider(model)
        .map(|vendor| vec![("x-api-provider".to_owned(), vendor.to_owned())])
        .unwrap_or_default()
}

/// Wire protocol and LLM base path suffix for one catalog model.
#[must_use]
pub(crate) fn wire_target(model: &str) -> (ProtocolId, &'static str) {
    if let Some(entry) = catalog_entry(model) {
        return (entry.wire, path_for_wire(entry.wire));
    }
    let wire = if model.starts_with("claude-") {
        ProtocolId::ANTHROPIC_MESSAGES
    } else if model.starts_with("gpt-") || model.starts_with("grok-") {
        ProtocolId::OPENAI_RESPONSES
    } else if model.starts_with("gemini-") {
        ProtocolId::GENERATIVE_LANGUAGE
    } else {
        ProtocolId::OPENAI_CHAT
    };
    (wire, path_for_wire(wire))
}

fn path_for_wire(wire: ProtocolId) -> &'static str {
    match wire {
        ProtocolId::ANTHROPIC_MESSAGES => "/api/llm/a/v1",
        ProtocolId::OPENAI_RESPONSES => "/api/llm/r/v1",
        ProtocolId::GENERATIVE_LANGUAGE => "/api/llm/g/v1",
        ProtocolId::OPENAI_CHAT => "/api/llm/o/v1",
        _ => "/api/llm/o/v1",
    }
}
