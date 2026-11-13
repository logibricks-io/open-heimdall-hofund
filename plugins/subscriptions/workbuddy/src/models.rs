//! Model list from WorkBuddy `/v3/config`.

use std::sync::Arc;

use hofund_plugin_api::{HostContext, ModelList, ModelSpec, PluginError};
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::{Call, Reply};

use crate::Endpoints;
use crate::creds::{Stored, apply_signed_headers};
use crate::site::Site;

const CN_MODELS: &[(&str, &str, u64)] = &[
    ("auto", "Auto", 168000),
    ("hy4-preview-f", "Hy4 preview", 1000000),
    ("hy3", "Hy3", 192000),
    ("hy3-x", "Hy3-X", 192000),
    ("deepseek-v4.1-flash", "Deepseek-V4.1-Flash", 1000000),
    ("glm-5.3", "GLM-5.3", 1000000),
    ("glm-5.3-flash", "GLM-5.3-Flash", 1000000),
    ("glm-5.2", "GLM-5.2", 1000000),
    ("glm-5.1", "GLM-5.1", 200000),
    ("glm-5v-turbo", "GLM-5v-Turbo", 200000),
    ("minimax-m3", "MiniMax-M3", 512000),
    ("kimi-k3-1", "Kimi-K3", 1000000),
    ("kimi-k2.7", "Kimi-K2.7-Code", 256000),
    ("kimi-k2.6", "Kimi-K2.6", 256000),
    ("deepseek-v4-pro", "Deepseek-V4-Pro", 1000000),
];

const AI_MODELS: &[(&str, &str, u64)] = &[
    ("default-model", "Default", 176000),
    ("fast-model", "Fast", 200000),
    ("balanced-model", "Balanced", 256000),
    ("primary-model", "Primary", 272000),
    ("deep-model", "Deep", 176000),
    ("gpt-5.5", "GPT-5.5", 1000000),
    ("gpt-5.4", "GPT-5.4", 272000),
    ("gpt-5.3-codex", "GPT-5.3-Codex", 272000),
    ("gemini-3.1-pro", "Gemini-3.1-Pro", 400000),
    ("gemini-3.5-flash", "Gemini-3.5-Flash", 1000000),
    ("glm-5.3", "GLM-5.3", 1000000),
    ("glm-5.2", "GLM-5.2", 1000000),
    ("hy3", "Hy3", 192000),
    ("kimi-k3", "Kimi-K3", 1000000),
    ("kimi-k2.6", "Kimi-K2.6", 256000),
    ("minimax-m3", "MiniMax-M3", 512000),
];

pub(crate) async fn list_account(
    host: &HostContext,
    endpoints: Arc<Endpoints>,
    stored: &Stored,
) -> Result<ModelList, PluginError> {
    let site = stored.credential.site;
    match config(host, endpoints, stored).await {
        Ok(reply) => Ok(parse_config(reply.body(), site).unwrap_or_else(|_| declared(site))),
        Err(_) => Ok(declared(site)),
    }
}

async fn config(
    host: &HostContext,
    endpoints: Arc<Endpoints>,
    stored: &Stored,
) -> Result<Reply, PluginError> {
    let base = endpoints.base_for(stored.credential.site);
    let url = format!("{base}/v3/config");
    let call = apply_signed_headers(
        Call::get(url).header("Accept", "application/json"),
        &stored.credential,
        base,
    );
    let reply = call.fetch(host.http()).await?;
    match reply.status() {
        200..=299 => Ok(reply),
        401 => Err(PluginError::auth(
            "WorkBuddy refused the sign-in while reading models",
        )),
        status => Err(PluginError::status_error(
            status,
            "WorkBuddy did not return model config",
        )),
    }
}

pub(crate) async fn editor(
    host: &HostContext,
    endpoints: Arc<Endpoints>,
    stored: &Stored,
    model: &str,
) -> Result<String, PluginError> {
    let reply = config(host, endpoints, stored).await?;
    let root: Value = hofund_plugin_sdk::json::from_slice(reply.body())
        .map_err(|_| PluginError::decode("WorkBuddy config JSON is invalid"))?;
    let items = config_items(&root)
        .ok_or_else(|| PluginError::decode("WorkBuddy config JSON has no models array"))?;
    editor_in(items, model).ok_or_else(|| {
        PluginError::status_error(
            400,
            format!(
                "WorkBuddy draws {model} from a prompt only: its config lists no model to edit images with"
            ),
        )
    })
}

fn editor_in(items: &[Value], model: &str) -> Option<String> {
    [model.to_owned(), format!("{model}-edit")]
        .into_iter()
        .find(|candidate| {
            items.iter().any(|item| {
                item.get("id").and_then(Value::as_str) == Some(candidate.as_str())
                    && tagged(item, "image-to-image")
            })
        })
}

fn tagged(item: &Value, wanted: &str) -> bool {
    item.get("tags")
        .and_then(Value::as_array)
        .is_some_and(|tags| {
            tags.iter().any(|tag| {
                tag.as_str()
                    .is_some_and(|tag| tag.trim().eq_ignore_ascii_case(wanted))
            })
        })
}

fn text_only(site: Site, id: &str) -> bool {
    site == Site::International && id.starts_with("glm-")
}

fn config_items(root: &Value) -> Option<&Vec<Value>> {
    root.get("models")
        .or_else(|| root.get("data").and_then(|data| data.get("models")))
        .and_then(Value::as_array)
}

fn effort_names(map: &hofund_plugin_sdk::json::Map<String, Value>, id: &str) -> Vec<String> {
    const ORDER: &[&str] = &["low", "medium", "high", "xhigh", "max"];
    let reasoning = map.get("reasoning");
    let listed = reasoning
        .and_then(|reasoning| reasoning.get("supportedEfforts"))
        .and_then(Value::as_array)
        .filter(|efforts| !efforts.is_empty());
    let mut efforts = if let Some(listed) = listed {
        listed
            .iter()
            .filter_map(|effort| effort.as_str().map(str::to_owned))
            .collect::<Vec<_>>()
    } else {
        if map.get("supportsReasoning").and_then(Value::as_bool) != Some(true) {
            return Vec::new();
        }
        let own = reasoning
            .and_then(|reasoning| reasoning.get("effort"))
            .and_then(Value::as_str)
            .filter(|own| ORDER.contains(own));
        ORDER
            .iter()
            .filter(|effort| matches!(**effort, "low" | "high" | "max") || Some(**effort) == own)
            .map(|effort| (*effort).to_owned())
            .collect()
    };
    if efforts.is_empty() {
        return efforts;
    }
    let only_reasoning = map.get("onlyReasoning").and_then(Value::as_bool) == Some(true);
    let can_disable = reasoning
        .and_then(|reasoning| reasoning.get("canDisableThinking"))
        .and_then(Value::as_bool);
    let off = (!only_reasoning && can_disable != Some(false))
        || matches!(id, "deepseek-v4.1-flash" | "deepseek-v4.1-flash-sg");
    if off {
        efforts.insert(0, "none".to_owned());
    }
    efforts
}

pub(crate) fn parse_config(body: &[u8], site: Site) -> Result<ModelList, PluginError> {
    let root: Value = hofund_plugin_sdk::json::from_slice(body)
        .map_err(|_| PluginError::decode("WorkBuddy config JSON is invalid"))?;
    let items = config_items(&root)
        .ok_or_else(|| PluginError::decode("WorkBuddy config JSON has no models array"))?;
    let mut models = Vec::new();
    for item in items {
        let Some(map) = item.as_object() else {
            continue;
        };
        let id = map
            .get("id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty());
        let Some(id) = id else {
            continue;
        };
        let name = map
            .get("name")
            .or_else(|| map.get("displayName"))
            .and_then(Value::as_str)
            .unwrap_or(id);
        let draws = tagged(item, "text-to-image");
        let image_input = if draws {
            editor_in(items, id).is_some()
        } else {
            map.get("supportsImages").and_then(Value::as_bool) == Some(true) && !text_only(site, id)
        };
        let modalities = if image_input {
            vec!["text".to_owned(), "image".to_owned()]
        } else {
            vec!["text".to_owned()]
        };
        if let Ok(spec) = ModelSpec::new(id, Some(name.to_owned())) {
            models.push(
                spec.with_context(
                    map.get("maxInputTokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                )
                .with_max_output(
                    map.get("maxOutputTokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                )
                .with_input_modalities(modalities)
                .with_output_modalities(vec![if draws {
                    "image".to_owned()
                } else {
                    "text".to_owned()
                }])
                .with_image_input(Some(image_input))
                .with_draws(draws)
                .with_efforts(effort_names(map, id)),
            );
        }
    }
    if models.is_empty() {
        return Err(PluginError::decode("WorkBuddy config listed no models"));
    }
    Ok(ModelList::new(models, None))
}

fn declared_efforts(site: Site, id: &str) -> &'static [&'static str] {
    match (site, id) {
        (Site::China, "hy4-preview-f") => &["high"],
        (Site::China, "hy3" | "hy3-x") | (_, "hy3") => &["low", "high"],
        (Site::China, "deepseek-v4.1-flash") => &["none", "low", "high", "max"],
        (_, "glm-5.3" | "glm-5.3-flash") => &["low", "high", "max"],
        (_, "glm-5.2") => &["high", "xhigh"],
        (Site::China, "kimi-k3-1") => &["low", "high", "xhigh"],
        (Site::China, "deepseek-v4-pro") => &["none", "high", "xhigh"],
        _ => &[],
    }
}

pub(crate) fn declared(site: Site) -> ModelList {
    let declared = match site {
        Site::China => CN_MODELS,
        Site::International => AI_MODELS,
    };
    let models = declared
        .iter()
        .filter_map(|(id, name, context)| {
            let image = !text_only(site, id);
            ModelSpec::new(*id, Some((*name).to_owned()))
                .ok()
                .map(|model| {
                    model
                        .with_context(*context)
                        .with_input_modalities(if image {
                            vec!["text".to_owned(), "image".to_owned()]
                        } else {
                            vec!["text".to_owned()]
                        })
                        .with_output_modalities(vec!["text".to_owned()])
                        .with_image_input(Some(image))
                        .with_efforts(
                            declared_efforts(site, id)
                                .iter()
                                .map(|effort| (*effort).to_owned())
                                .collect(),
                        )
                })
        })
        .collect();
    ModelList::new(models, None)
}
