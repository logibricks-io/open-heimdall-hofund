//! Factory model catalog and request shaping from the reference Droid registry.

use std::sync::Arc;
use std::time::SystemTime;

use hofund_plugin_api::{ModelList, ModelSpec, PluginError, ProtocolId};
use hofund_plugin_sdk::json::{Map, Value, json};
use hofund_plugin_sdk::time::parse_rfc3339;
use hofund_plugin_sdk::wire::Rewrite;

const FAST_BETA: &str = "fast-mode-2026-02-01";
const DROID_LINE: &str = "You are Droid, an AI software engineering agent built by Factory.";

struct CatalogModel {
    id: &'static str,
    name: &'static str,
    wire: ProtocolId,
    upstream: &'static str,
    context: u64,
    max_output: u64,
    efforts: &'static [&'static str],
    images: bool,
    rate: u64,
}

const E5: &[&str] = &["low", "medium", "high", "xhigh", "max"];
const E6: &[&str] = &["none", "low", "medium", "high", "xhigh", "max"];
const E4: &[&str] = &["low", "medium", "high", "xhigh"];
const G3: &[&str] = &["low", "medium", "high"];
const G3_MINIMAL: &[&str] = &["minimal", "low", "medium", "high"];

const MODELS: &[CatalogModel] = &[
    CatalogModel {
        id: "claude-fable-5.1",
        name: "Fable 5.1",
        wire: ProtocolId::ANTHROPIC_MESSAGES,
        upstream: "anthropic",
        context: 867_000,
        max_output: 128_000,
        efforts: E5,
        images: true,
        rate: 40_000,
    },
    CatalogModel {
        id: "claude-fable-5",
        name: "Fable 5",
        wire: ProtocolId::ANTHROPIC_MESSAGES,
        upstream: "anthropic",
        context: 867_000,
        max_output: 128_000,
        efforts: E5,
        images: true,
        rate: 40_000,
    },
    CatalogModel {
        id: "claude-opus-5-5",
        name: "Opus 5.5",
        wire: ProtocolId::ANTHROPIC_MESSAGES,
        upstream: "anthropic",
        context: 872_000,
        max_output: 128_000,
        efforts: E5,
        images: true,
        rate: 16_000,
    },
    CatalogModel {
        id: "claude-opus-5",
        name: "Opus 5",
        wire: ProtocolId::ANTHROPIC_MESSAGES,
        upstream: "anthropic",
        context: 867_000,
        max_output: 128_000,
        efforts: E5,
        images: true,
        rate: 20_000,
    },
    CatalogModel {
        id: "claude-opus-4-8",
        name: "Opus 4.8",
        wire: ProtocolId::ANTHROPIC_MESSAGES,
        upstream: "anthropic",
        context: 867_000,
        max_output: 128_000,
        efforts: E5,
        images: true,
        rate: 20_000,
    },
    CatalogModel {
        id: "claude-sonnet-5-5",
        name: "Sonnet 5.5",
        wire: ProtocolId::ANTHROPIC_MESSAGES,
        upstream: "anthropic",
        context: 872_000,
        max_output: 128_000,
        efforts: E5,
        images: true,
        rate: 8_000,
    },
    CatalogModel {
        id: "claude-sonnet-5",
        name: "Sonnet 5",
        wire: ProtocolId::ANTHROPIC_MESSAGES,
        upstream: "anthropic",
        context: 872_000,
        max_output: 128_000,
        efforts: E5,
        images: true,
        rate: 8_000,
    },
    CatalogModel {
        id: "claude-sonnet-4-6",
        name: "Sonnet 4.6",
        wire: ProtocolId::ANTHROPIC_MESSAGES,
        upstream: "anthropic",
        context: 931_000,
        max_output: 64_000,
        efforts: &["low", "medium", "high", "max"],
        images: true,
        rate: 12_000,
    },
    CatalogModel {
        id: "claude-haiku-5-5",
        name: "Haiku 5.5",
        wire: ProtocolId::ANTHROPIC_MESSAGES,
        upstream: "anthropic",
        context: 872_000,
        max_output: 128_000,
        efforts: E5,
        images: true,
        rate: 400,
    },
    CatalogModel {
        id: "claude-haiku-4-5-20251001",
        name: "Haiku 4.5",
        wire: ProtocolId::ANTHROPIC_MESSAGES,
        upstream: "anthropic",
        context: 0,
        max_output: 0,
        efforts: G3,
        images: true,
        rate: 4_000,
    },
    CatalogModel {
        id: "gpt-6.1-sol",
        name: "GPT-6.1 Sol",
        wire: ProtocolId::OPENAI_RESPONSES,
        upstream: "openai",
        context: 1_050_000,
        max_output: 128_000,
        efforts: E5,
        images: true,
        rate: 8_000,
    },
    CatalogModel {
        id: "gpt-6-sol",
        name: "GPT-6 Sol",
        wire: ProtocolId::OPENAI_RESPONSES,
        upstream: "openai",
        context: 1_050_000,
        max_output: 128_000,
        efforts: E6,
        images: true,
        rate: 8_000,
    },
    CatalogModel {
        id: "gpt-6-astra",
        name: "GPT-6 Astra",
        wire: ProtocolId::OPENAI_RESPONSES,
        upstream: "openai",
        context: 1_050_000,
        max_output: 128_000,
        efforts: E5,
        images: true,
        rate: 40_000,
    },
    CatalogModel {
        id: "gpt-6-luna",
        name: "GPT-6 Luna",
        wire: ProtocolId::OPENAI_RESPONSES,
        upstream: "openai",
        context: 1_050_000,
        max_output: 128_000,
        efforts: E6,
        images: true,
        rate: 400,
    },
    CatalogModel {
        id: "gpt-5.6-sol",
        name: "GPT-5.6 Sol",
        wire: ProtocolId::OPENAI_RESPONSES,
        upstream: "openai",
        context: 1_050_000,
        max_output: 128_000,
        efforts: E6,
        images: true,
        rate: 20_000,
    },
    CatalogModel {
        id: "gpt-5.6-terra",
        name: "GPT-5.6 Terra",
        wire: ProtocolId::OPENAI_RESPONSES,
        upstream: "openai",
        context: 1_050_000,
        max_output: 128_000,
        efforts: E6,
        images: true,
        rate: 8_000,
    },
    CatalogModel {
        id: "gpt-5.6-luna",
        name: "GPT-5.6 Luna",
        wire: ProtocolId::OPENAI_RESPONSES,
        upstream: "openai",
        context: 1_050_000,
        max_output: 128_000,
        efforts: E6,
        images: true,
        rate: 800,
    },
    CatalogModel {
        id: "gpt-5.5",
        name: "GPT-5.5",
        wire: ProtocolId::OPENAI_RESPONSES,
        upstream: "openai",
        context: 1_050_000,
        max_output: 128_000,
        efforts: E4,
        images: true,
        rate: 20_000,
    },
    CatalogModel {
        id: "gpt-5.4",
        name: "GPT-5.4",
        wire: ProtocolId::OPENAI_RESPONSES,
        upstream: "openai",
        context: 1_050_000,
        max_output: 128_000,
        efforts: E4,
        images: true,
        rate: 10_000,
    },
    CatalogModel {
        id: "gpt-5.3-codex",
        name: "GPT-5.3-Codex",
        wire: ProtocolId::OPENAI_RESPONSES,
        upstream: "openai",
        context: 400_000,
        max_output: 128_000,
        efforts: E4,
        images: true,
        rate: 7_000,
    },
    CatalogModel {
        id: "grok-4.7",
        name: "Grok 4.7",
        wire: ProtocolId::OPENAI_RESPONSES,
        upstream: "xai",
        context: 500_000,
        max_output: 63_356,
        efforts: E4,
        images: true,
        rate: 8_000,
    },
    CatalogModel {
        id: "grok-4.6",
        name: "Grok 4.6",
        wire: ProtocolId::OPENAI_RESPONSES,
        upstream: "xai",
        context: 200_000,
        max_output: 63_356,
        efforts: E4,
        images: true,
        rate: 8_000,
    },
    CatalogModel {
        id: "glm-5.3",
        name: "GLM-5.3",
        wire: ProtocolId::OPENAI_CHAT,
        upstream: "fireworks",
        context: 1_040_000,
        max_output: 131_072,
        efforts: &["low", "high", "max"],
        images: false,
        rate: 5_600,
    },
    CatalogModel {
        id: "glm-5.3-flash",
        name: "GLM-5.3-Flash",
        wire: ProtocolId::OPENAI_CHAT,
        upstream: "fireworks",
        context: 1_048_576,
        max_output: 131_072,
        efforts: &["low", "high", "max"],
        images: true,
        rate: 600,
    },
    CatalogModel {
        id: "glm-5.2",
        name: "GLM-5.2",
        wire: ProtocolId::OPENAI_CHAT,
        upstream: "baseten",
        context: 1_040_000,
        max_output: 131_072,
        efforts: &["none", "high", "max"],
        images: false,
        rate: 5_600,
    },
    CatalogModel {
        id: "kimi-k3",
        name: "Kimi K3",
        wire: ProtocolId::OPENAI_CHAT,
        upstream: "fireworks",
        context: 262_144,
        max_output: 65_536,
        efforts: &["none", "low", "high", "max"],
        images: true,
        rate: 12_000,
    },
    CatalogModel {
        id: "deepseek-v4.1-flash",
        name: "DeepSeek V4.1 Flash",
        wire: ProtocolId::OPENAI_CHAT,
        upstream: "fireworks",
        context: 1_040_000,
        max_output: 131_072,
        efforts: &["none", "low", "high", "max"],
        images: true,
        rate: 1_200,
    },
    CatalogModel {
        id: "deepseek-v4.1-flash-1017",
        name: "DeepSeek V4.1 Flash 1017",
        wire: ProtocolId::OPENAI_CHAT,
        upstream: "fireworks",
        context: 1_040_000,
        max_output: 131_072,
        efforts: &["none", "low", "high", "max"],
        images: true,
        rate: 1_200,
    },
    CatalogModel {
        id: "qwen3.8-max",
        name: "Qwen3.8 Max",
        wire: ProtocolId::OPENAI_CHAT,
        upstream: "fireworks",
        context: 262_144,
        max_output: 131_072,
        efforts: &["low", "medium", "xhigh"],
        images: false,
        rate: 8_000,
    },
    CatalogModel {
        id: "minimax-m3",
        name: "MiniMax M3",
        wire: ProtocolId::OPENAI_CHAT,
        upstream: "fireworks",
        context: 512_000,
        max_output: 64_000,
        efforts: &["high"],
        images: true,
        rate: 1_200,
    },
    CatalogModel {
        id: "minimax-m2.7",
        name: "MiniMax M2.7",
        wire: ProtocolId::ANTHROPIC_MESSAGES,
        upstream: "fireworks",
        context: 196_600,
        max_output: 64_000,
        efforts: &["high"],
        images: false,
        rate: 1_200,
    },
    CatalogModel {
        id: "mistral-large-4",
        name: "Mistral Large 4",
        wire: ProtocolId::OPENAI_CHAT,
        upstream: "mistral",
        context: 524_288,
        max_output: 64_000,
        efforts: &["high"],
        images: true,
        rate: 5_440,
    },
    CatalogModel {
        id: "mistral-medium-3.5",
        name: "Mistral Medium 3.5",
        wire: ProtocolId::OPENAI_CHAT,
        upstream: "mistral",
        context: 256_000,
        max_output: 64_000,
        efforts: &["high"],
        images: true,
        rate: 6_000,
    },
    CatalogModel {
        id: "nemotron-3-ultra",
        name: "Nemotron 3 Ultra",
        wire: ProtocolId::OPENAI_CHAT,
        upstream: "baseten",
        context: 202_000,
        max_output: 65_536,
        efforts: &["high"],
        images: false,
        rate: 2_400,
    },
    CatalogModel {
        id: "gemini-4-argon",
        name: "Gemini 4 Argon",
        wire: ProtocolId::GENERATIVE_LANGUAGE,
        upstream: "google",
        context: 1_000_000,
        max_output: 65_536,
        efforts: G3,
        images: true,
        rate: 16_000,
    },
    CatalogModel {
        id: "gemini-3.1-pro-preview",
        name: "Gemini 3.1 Pro",
        wire: ProtocolId::GENERATIVE_LANGUAGE,
        upstream: "google",
        context: 1_000_000,
        max_output: 65_536,
        efforts: G3,
        images: true,
        rate: 8_000,
    },
    CatalogModel {
        id: "gemini-3.8-flash",
        name: "Gemini 3.8 Flash",
        wire: ProtocolId::GENERATIVE_LANGUAGE,
        upstream: "google",
        context: 1_000_000,
        max_output: 65_536,
        efforts: G3,
        images: true,
        rate: 6_000,
    },
    CatalogModel {
        id: "gemini-3.7-flash",
        name: "Gemini 3.7 Flash",
        wire: ProtocolId::GENERATIVE_LANGUAGE,
        upstream: "google",
        context: 1_000_000,
        max_output: 65_536,
        efforts: G3,
        images: true,
        rate: 6_000,
    },
    CatalogModel {
        id: "gemini-3.6-flash",
        name: "Gemini 3.6 Flash",
        wire: ProtocolId::GENERATIVE_LANGUAGE,
        upstream: "google",
        context: 1_000_000,
        max_output: 65_536,
        efforts: G3,
        images: true,
        rate: 6_000,
    },
    CatalogModel {
        id: "gemini-3.5-flash",
        name: "Gemini 3.5 Flash",
        wire: ProtocolId::GENERATIVE_LANGUAGE,
        upstream: "google",
        context: 1_000_000,
        max_output: 65_536,
        efforts: G3_MINIMAL,
        images: true,
        rate: 6_000,
    },
    CatalogModel {
        id: "gemini-3-flash-preview",
        name: "Gemini 3 Flash",
        wire: ProtocolId::GENERATIVE_LANGUAGE,
        upstream: "google",
        context: 1_000_000,
        max_output: 65_536,
        efforts: G3_MINIMAL,
        images: true,
        rate: 2_000,
    },
    CatalogModel {
        id: "claude-opus-5-5-fast",
        name: "Opus 5.5 Fast Mode",
        wire: ProtocolId::ANTHROPIC_MESSAGES,
        upstream: "anthropic",
        context: 872_000,
        max_output: 128_000,
        efforts: E5,
        images: true,
        rate: 32_000,
    },
    CatalogModel {
        id: "claude-opus-5-fast",
        name: "Opus 5 Fast Mode",
        wire: ProtocolId::ANTHROPIC_MESSAGES,
        upstream: "anthropic",
        context: 867_000,
        max_output: 128_000,
        efforts: E5,
        images: true,
        rate: 40_000,
    },
    CatalogModel {
        id: "claude-opus-4-8-fast",
        name: "Opus 4.8 Fast Mode",
        wire: ProtocolId::ANTHROPIC_MESSAGES,
        upstream: "anthropic",
        context: 867_000,
        max_output: 128_000,
        efforts: E5,
        images: true,
        rate: 40_000,
    },
    CatalogModel {
        id: "gpt-5.5-fast",
        name: "GPT-5.5 Fast Mode",
        wire: ProtocolId::OPENAI_RESPONSES,
        upstream: "openai",
        context: 1_050_000,
        max_output: 128_000,
        efforts: E4,
        images: true,
        rate: 50_000,
    },
    CatalogModel {
        id: "gpt-5.4-fast",
        name: "GPT-5.4 Fast Mode",
        wire: ProtocolId::OPENAI_RESPONSES,
        upstream: "openai",
        context: 1_050_000,
        max_output: 128_000,
        efforts: E4,
        images: true,
        rate: 20_000,
    },
    CatalogModel {
        id: "gpt-5.3-codex-fast",
        name: "GPT-5.3-Codex Fast Mode",
        wire: ProtocolId::OPENAI_RESPONSES,
        upstream: "openai",
        context: 400_000,
        max_output: 128_000,
        efforts: E4,
        images: true,
        rate: 14_000,
    },
];

struct Promotion {
    id: &'static str,
    discount_percent: u64,
    from: &'static str,
    to: &'static str,
}

const PROMOTIONS: &[Promotion] = &[
    Promotion {
        id: "gpt-5.6-sol",
        discount_percent: 20,
        from: "2026-08-22T00:00:00Z",
        to: "2026-11-22T00:00:00Z",
    },
    Promotion {
        id: "mistral-large-4",
        discount_percent: 50,
        from: "2026-10-06T00:00:00Z",
        to: "2026-10-20T00:00:00Z",
    },
    Promotion {
        id: "gemini-4-argon",
        discount_percent: 50,
        from: "2026-10-13T16:00:00Z",
        to: "2026-12-01T08:00:00Z",
    },
    Promotion {
        id: "gemini-3.8-flash",
        discount_percent: 50,
        from: "2026-08-17T21:10:19Z",
        to: "2027-01-01T08:00:00Z",
    },
    Promotion {
        id: "gemini-3.7-flash",
        discount_percent: 50,
        from: "2026-08-17T21:10:19Z",
        to: "2027-01-01T08:00:00Z",
    },
];

fn catalog_entry(id: &str) -> Option<&'static CatalogModel> {
    MODELS.iter().find(|model| model.id == id)
}

fn rate_at(model: &CatalogModel, now: SystemTime) -> (u64, u64) {
    let active = PROMOTIONS.iter().find(|promotion| {
        promotion.id == model.id
            && parse_rfc3339(promotion.from).is_some_and(|from| now >= from)
            && parse_rfc3339(promotion.to).is_some_and(|to| now < to)
    });
    match active {
        Some(promotion) => (
            model.rate * (100 - promotion.discount_percent) / 100,
            model.rate,
        ),
        None => (model.rate, 0),
    }
}

/// Static Factory model list at one reading time.
pub(crate) fn catalog_at(now: SystemTime) -> Result<ModelList, PluginError> {
    let mut specs = Vec::with_capacity(MODELS.len());
    for model in MODELS {
        let (rate, rate_was) = rate_at(model, now);
        specs.push(
            ModelSpec::new(model.id, Some(model.name.to_owned()))?
                .with_context(model.context)
                .with_max_output(model.max_output)
                .with_image_input(Some(model.images))
                .with_input_modalities(if model.images {
                    vec!["text".to_owned(), "image".to_owned()]
                } else {
                    vec!["text".to_owned()]
                })
                .with_output_modalities(vec!["text".to_owned()])
                .with_efforts(
                    model
                        .efforts
                        .iter()
                        .map(|effort| (*effort).to_owned())
                        .collect(),
                )
                .with_rate(rate, rate_was),
        );
    }
    Ok(ModelList::new(specs, None).with_kind(hofund_plugin_api::ListKind::Catalog))
}

/// Models billed to Droid Core rather than Standard.
pub(crate) fn core_models() -> Vec<String> {
    MODELS
        .iter()
        .filter(|model| !matches!(model.upstream, "anthropic" | "openai" | "xai" | "google"))
        .map(|model| model.id.to_owned())
        .collect()
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

/// Request body and added headers for one upstream wire.
#[must_use]
pub(crate) fn request_rewrite(wire: ProtocolId) -> Rewrite {
    Arc::new(move |body| {
        let added = fast_body(body, wire);
        client_body(body, wire);
        added
    })
}

fn fast_body(body: &mut Map<String, Value>, wire: ProtocolId) -> Vec<(String, String)> {
    let anthropic = wire == ProtocolId::ANTHROPIC_MESSAGES;
    let responses = wire == ProtocolId::OPENAI_RESPONSES;
    if !anthropic && !responses {
        return Vec::new();
    }
    let Some(original) = body.get("model").and_then(Value::as_str).map(str::to_owned) else {
        return Vec::new();
    };
    let asked = if anthropic {
        body.get("speed").and_then(Value::as_str) == Some("fast")
    } else {
        body.get("service_tier").and_then(Value::as_str) == Some("priority")
    };
    let fast_id = asked
        .then(|| format!("{original}-fast"))
        .filter(|id| catalog_entry(id).is_some_and(|model| model.wire == wire));
    let model = fast_id.as_deref().unwrap_or(&original);
    let is_fast =
        model.ends_with("-fast") && catalog_entry(model).is_some_and(|entry| entry.wire == wire);
    if !is_fast {
        return Vec::new();
    }
    body.insert("model".to_owned(), Value::String(model.to_owned()));
    if anthropic {
        body.insert("speed".to_owned(), Value::String("fast".to_owned()));
        vec![("anthropic-beta".to_owned(), FAST_BETA.to_owned())]
    } else {
        body.insert(
            "service_tier".to_owned(),
            Value::String("priority".to_owned()),
        );
        Vec::new()
    }
}

fn client_body(body: &mut Map<String, Value>, wire: ProtocolId) {
    match wire {
        ProtocolId::OPENAI_RESPONSES => {
            if body
                .get("model")
                .and_then(Value::as_str)
                .and_then(x_api_provider)
                == Some("openai")
            {
                body.remove("temperature");
                body.remove("top_p");
            }
            let instructions = body
                .get("instructions")
                .and_then(Value::as_str)
                .unwrap_or_default();
            body.insert(
                "instructions".to_owned(),
                Value::String(with_droid_opening(instructions)),
            );
        }
        ProtocolId::OPENAI_CHAT => {
            let Some(Value::Array(messages)) = body.get_mut("messages") else {
                return;
            };
            match messages.first_mut() {
                Some(Value::Object(first))
                    if first.get("role").and_then(Value::as_str) == Some("system") =>
                {
                    match first.get_mut("content") {
                        Some(Value::String(text)) => {
                            *text = with_droid_opening(text);
                        }
                        Some(Value::Array(parts)) => {
                            if let Some(Value::Object(part)) = parts.first_mut()
                                && let Some(Value::String(text)) = part.get_mut("text")
                            {
                                *text = with_droid_opening(text);
                            }
                        }
                        _ => {}
                    }
                }
                _ => messages.insert(0, json!({"role": "system", "content": DROID_LINE})),
            }
        }
        ProtocolId::ANTHROPIC_MESSAGES => {
            let system = body
                .remove("system")
                .unwrap_or_else(|| Value::Array(Vec::new()));
            let mut blocks = match system {
                Value::String(text) if !text.trim().is_empty() => {
                    vec![json!({"type": "text", "text": text})]
                }
                Value::Array(blocks) => blocks,
                _ => Vec::new(),
            };
            let opened = blocks.first().is_some_and(|block| {
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.starts_with(DROID_LINE))
            });
            if !opened {
                blocks.insert(0, json!({"type": "text", "text": DROID_LINE}));
            }
            body.insert("system".to_owned(), Value::Array(blocks));
            if body.get("stream").and_then(Value::as_bool) == Some(true) {
                body.remove("context_management");
            }
        }
        ProtocolId::GENERATIVE_LANGUAGE => gemini_client_body(body),
        _ => {}
    }
}

fn gemini_client_body(body: &mut Map<String, Value>) {
    body.remove("stream");
    let instruction = body
        .remove("systemInstruction")
        .unwrap_or_else(|| json!({"parts": []}));
    let mut instruction = match instruction {
        Value::String(text) => json!({"parts": [{"text": text}]}),
        Value::Object(object) => Value::Object(object),
        _ => json!({"parts": []}),
    };
    if let Some(object) = instruction.as_object_mut() {
        object.remove("role");
        let parts = object
            .remove("parts")
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default();
        let mut texts = Vec::new();
        let mut rest = Vec::new();
        for part in parts {
            let plain_text = part.as_object().is_some_and(|object| {
                object.len() == 1 && object.get("text").and_then(Value::as_str).is_some()
            });
            if plain_text {
                texts.push(
                    part.get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                );
            } else {
                rest.push(part);
            }
        }
        if rest.is_empty()
            && texts
                .first()
                .is_some_and(|text| text.starts_with(DROID_LINE))
        {
            object.insert(
                "parts".to_owned(),
                Value::Array(
                    texts
                        .into_iter()
                        .map(|text| json!({"text": text}))
                        .collect(),
                ),
            );
        } else {
            let mut parts = vec![json!({"text": with_droid_opening(&texts.join("\n"))})];
            parts.extend(rest);
            object.insert("parts".to_owned(), Value::Array(parts));
        }
    }
    body.insert("systemInstruction".to_owned(), instruction);
}

fn with_droid_opening(text: &str) -> String {
    if text.starts_with(DROID_LINE) {
        text.to_owned()
    } else if text.trim().is_empty() {
        DROID_LINE.to_owned()
    } else {
        format!("{DROID_LINE}\n{text}")
    }
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
        ProtocolId::ANTHROPIC_MESSAGES => "/api/llm/a",
        ProtocolId::OPENAI_RESPONSES => "/api/llm/o/v1",
        ProtocolId::GENERATIVE_LANGUAGE => "/api/llm/g/v1",
        ProtocolId::OPENAI_CHAT => "/api/llm/o/v1",
        _ => "/api/llm/o/v1",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> SystemTime {
        parse_rfc3339(text).expect("time")
    }

    #[test]
    fn r52_promotions_expose_current_and_previous_rates() {
        let list = catalog_at(at("2026-10-14T21:30:00Z")).expect("catalog");
        let models: std::collections::HashMap<_, _> = list
            .models()
            .iter()
            .map(|model| (model.id(), model))
            .collect();
        let argon = models["gemini-4-argon"];
        assert_eq!(argon.rate(), Some(0.8));
        assert_eq!(argon.rate_was(), 1.6);
        let sol = models["gpt-5.6-sol"];
        assert_eq!(sol.rate(), Some(1.6));
        assert_eq!(sol.rate_was(), 2.0);
        assert_eq!(models["gpt-5.5"].rate_was(), 0.0);
    }

    #[test]
    fn r52_promotion_window_has_start_and_exclusive_end() {
        let before = catalog_at(at("2026-08-01T00:00:00Z")).expect("catalog");
        let sol = before
            .models()
            .iter()
            .find(|model| model.id() == "gpt-5.6-sol")
            .expect("sol");
        assert_eq!(sol.rate(), Some(2.0));
        assert_eq!(sol.rate_was(), 0.0);
        let after = catalog_at(at("2026-12-02T00:00:00Z")).expect("catalog");
        let argon = after
            .models()
            .iter()
            .find(|model| model.id() == "gemini-4-argon")
            .expect("argon");
        assert_eq!(argon.rate(), Some(1.6));
        assert_eq!(argon.rate_was(), 0.0);
    }

    #[test]
    fn r52_fast_models_are_last_with_base_limits_and_efforts() {
        let list = catalog_at(at("2026-10-14T21:30:00Z")).expect("catalog");
        assert_eq!(list.models().len(), 47);
        let first_fast = list
            .models()
            .iter()
            .position(|model| model.id().ends_with("-fast"))
            .expect("fast");
        assert!(
            list.models()[first_fast..]
                .iter()
                .all(|model| model.id().ends_with("-fast"))
        );
        for base in [
            "claude-opus-5-5",
            "claude-opus-5",
            "claude-opus-4-8",
            "gpt-5.5",
            "gpt-5.4",
            "gpt-5.3-codex",
        ] {
            let base_model = list
                .models()
                .iter()
                .find(|model| model.id() == base)
                .expect("base");
            let fast = list
                .models()
                .iter()
                .find(|model| model.id() == format!("{base}-fast"))
                .expect("fast");
            assert_eq!(fast.context(), base_model.context());
            assert_eq!(fast.max_output(), base_model.max_output());
            assert_eq!(fast.efforts(), base_model.efforts());
        }
    }

    #[test]
    fn r52_fast_body_rewrites_only_reference_pairs() {
        let mut anthropic = json!({"model": "claude-opus-5-5", "speed": "fast"})
            .as_object()
            .expect("object")
            .clone();
        let added = fast_body(&mut anthropic, ProtocolId::ANTHROPIC_MESSAGES);
        assert_eq!(anthropic["model"], "claude-opus-5-5-fast");
        assert_eq!(
            added,
            vec![("anthropic-beta".to_owned(), FAST_BETA.to_owned())]
        );

        let mut direct = json!({"model": "claude-opus-5-fast"})
            .as_object()
            .expect("object")
            .clone();
        fast_body(&mut direct, ProtocolId::ANTHROPIC_MESSAGES);
        assert_eq!(direct["speed"], "fast");

        let mut responses = json!({"model": "gpt-5.5", "service_tier": "priority"})
            .as_object()
            .expect("object")
            .clone();
        assert!(fast_body(&mut responses, ProtocolId::OPENAI_RESPONSES).is_empty());
        assert_eq!(responses["model"], "gpt-5.5-fast");

        let mut unavailable = json!({"model": "gpt-6-sol", "service_tier": "priority"})
            .as_object()
            .expect("object")
            .clone();
        fast_body(&mut unavailable, ProtocolId::OPENAI_RESPONSES);
        assert_eq!(unavailable["model"], "gpt-6-sol");
    }

    #[test]
    fn r52_gemini_body_uses_generate_shape_and_thinking_config() {
        let mut body = json!({
            "model": "gemini-4-argon",
            "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
            "systemInstruction": {"role": "user", "parts": [{"text": "Be brief."}, {"text": "Use tools."}]},
            "generationConfig": {"thinkingConfig": {"includeThoughts": true, "thinkingLevel": "high"}},
            "stream": true
        }).as_object().expect("object").clone();
        client_body(&mut body, ProtocolId::GENERATIVE_LANGUAGE);
        assert_eq!(body["model"], "gemini-4-argon");
        assert!(body.get("stream").is_none());
        assert_eq!(
            body["systemInstruction"]["parts"][0]["text"],
            format!("{DROID_LINE}\nBe brief.\nUse tools.")
        );
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "high"
        );
    }
}
