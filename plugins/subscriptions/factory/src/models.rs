//! Factory model catalog from the reference Droid registry.

use hofund_plugin_api::{ModelList, ModelSpec, PluginError, ProtocolId};

const MODELS: &[(&str, &str)] = &[
    ("claude-fable-5.1", "Fable 5.1"),
    ("claude-opus-5-5", "Opus 5.5"),
    ("claude-sonnet-5-5", "Sonnet 5.5"),
    ("claude-haiku-5-5", "Haiku 5.5"),
    ("gpt-6-sol", "GPT-6 Sol"),
    ("gpt-6-astra", "GPT-6 Astra"),
    ("grok-4.7", "Grok 4.7"),
    ("gemini-4-argon", "Gemini 4 Argon"),
    ("glm-5.3", "GLM-5.3"),
    ("kimi-k3", "Kimi K3"),
    ("mistral-large-4", "Mistral Large 4"),
];

/// Static Factory model list.
pub(crate) fn catalog() -> Result<ModelList, PluginError> {
    let specs: Result<Vec<_>, _> = MODELS
        .iter()
        .map(|(id, name)| ModelSpec::new(*id, Some((*name).to_owned())))
        .collect();
    Ok(ModelList::new(specs?, None))
}

/// Wire protocol and LLM base path suffix for one catalog model.
#[must_use]
pub(crate) fn wire_target(model: &str) -> (ProtocolId, &'static str) {
    if model.starts_with("claude-") {
        return (ProtocolId::ANTHROPIC_MESSAGES, "/api/llm/a/v1");
    }
    if model.starts_with("gpt-") || model.starts_with("grok-") {
        return (ProtocolId::OPENAI_RESPONSES, "/api/llm/r/v1");
    }
    if model.starts_with("gemini-") {
        return (ProtocolId::GENERATIVE_LANGUAGE, "/api/llm/g/v1");
    }
    (ProtocolId::OPENAI_CHAT, "/api/llm/c/v1")
}
