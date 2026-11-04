//! Factory model catalog from the reference Droid registry.

use hofund_plugin_api::{ModelList, ModelSpec, PluginError, ProtocolId};

struct CatalogModel {
    id: &'static str,
    name: &'static str,
    wire: ProtocolId,
    upstream: &'static str,
}

const MODELS: &[CatalogModel] = &[
    CatalogModel {
        id: "claude-fable-5.1",
        name: "Fable 5.1",
        wire: ProtocolId::ANTHROPIC_MESSAGES,
        upstream: "anthropic",
    },
    CatalogModel {
        id: "claude-opus-5-5",
        name: "Opus 5.5",
        wire: ProtocolId::ANTHROPIC_MESSAGES,
        upstream: "anthropic",
    },
    CatalogModel {
        id: "claude-sonnet-5-5",
        name: "Sonnet 5.5",
        wire: ProtocolId::ANTHROPIC_MESSAGES,
        upstream: "anthropic",
    },
    CatalogModel {
        id: "claude-haiku-5-5",
        name: "Haiku 5.5",
        wire: ProtocolId::ANTHROPIC_MESSAGES,
        upstream: "anthropic",
    },
    CatalogModel {
        id: "gpt-6-sol",
        name: "GPT-6 Sol",
        wire: ProtocolId::OPENAI_RESPONSES,
        upstream: "openai",
    },
    CatalogModel {
        id: "gpt-6-astra",
        name: "GPT-6 Astra",
        wire: ProtocolId::OPENAI_RESPONSES,
        upstream: "openai",
    },
    CatalogModel {
        id: "grok-4.7",
        name: "Grok 4.7",
        wire: ProtocolId::OPENAI_RESPONSES,
        upstream: "xai",
    },
    CatalogModel {
        id: "gemini-4-argon",
        name: "Gemini 4 Argon",
        wire: ProtocolId::GENERATIVE_LANGUAGE,
        upstream: "google",
    },
    CatalogModel {
        id: "glm-5.3",
        name: "GLM-5.3",
        wire: ProtocolId::OPENAI_CHAT,
        upstream: "fireworks",
    },
    CatalogModel {
        id: "kimi-k3",
        name: "Kimi K3",
        wire: ProtocolId::OPENAI_CHAT,
        upstream: "fireworks",
    },
    CatalogModel {
        id: "mistral-large-4",
        name: "Mistral Large 4",
        wire: ProtocolId::OPENAI_CHAT,
        upstream: "mistral",
    },
];

fn catalog_entry(id: &str) -> Option<&'static CatalogModel> {
    MODELS.iter().find(|model| model.id == id)
}

/// Static Factory model list.
pub(crate) fn catalog() -> Result<ModelList, PluginError> {
    let specs: Result<Vec<_>, _> = MODELS
        .iter()
        .map(|model| ModelSpec::new(model.id, Some(model.name.to_owned())))
        .collect();
    Ok(ModelList::new(specs?, None))
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
