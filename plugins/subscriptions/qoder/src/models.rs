//! Native COSY model catalog and per-request model configuration.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use hofund_plugin_api::{AccountId, HostContext, ModelList, ModelSpec, PluginError};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::Value;

use crate::creds::Stored;
use crate::{Endpoints, cosy, identity, refresh};

const MODELS_PATH: &str = "/algo/api/v2/model/list?Encode=1";
const EFFORT_ORDER: &[&str] = &["minimal", "low", "medium", "high", "xhigh", "max"];

#[derive(Clone, Debug)]
pub(crate) struct ModelInfo {
    pub key: String,
    pub name: String,
    pub source: String,
    pub context: u64,
    pub windows: Vec<u64>,
    pub default_window: u64,
    pub images: bool,
    pub thinks: bool,
    pub always_thinks: bool,
    pub efforts: Vec<String>,
    pub default_effort: String,
    pub free: bool,
    pub raw: Value,
}

impl ModelInfo {
    pub(crate) fn spec(&self) -> Result<ModelSpec, PluginError> {
        Ok(ModelSpec::new(&self.key, Some(self.name.clone()))?
            .with_context(self.context)
            .with_efforts(self.efforts.clone())
            .with_decides(self.thinks)
            .with_image_input(Some(self.images))
            .with_input_modalities(if self.images {
                vec!["text".to_owned(), "image".to_owned()]
            } else {
                vec!["text".to_owned()]
            })
            .with_output_modalities(vec!["text".to_owned()])
            .with_free(self.free))
    }
}

#[derive(Default)]
pub(crate) struct Catalog {
    entries: Mutex<HashMap<String, Arc<Vec<ModelInfo>>>>,
}

impl Catalog {
    pub(crate) async fn model(
        &self,
        host: &HostContext,
        endpoints: Arc<Endpoints>,
        stored: &Stored,
        id: &str,
    ) -> Result<Option<ModelInfo>, PluginError> {
        let key = catalog_key(stored);
        if let Some(model) = self.cached(&key, id) {
            return Ok(Some(model));
        }
        let listed = fetch(host, Arc::clone(&endpoints), stored).await?;
        let found = listed.iter().find(|model| model.key == id).cloned();
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key.clone(), Arc::new(listed));
        if found.is_some() {
            return Ok(found);
        }
        let listed = fetch(host, endpoints, stored).await?;
        let found = listed.iter().find(|model| model.key == id).cloned();
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key, Arc::new(listed));
        Ok(found)
    }

    fn cached(&self, key: &str, id: &str) -> Option<ModelInfo> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(key)
            .and_then(|models| models.iter().find(|model| model.key == id).cloned())
    }
}

fn catalog_key(stored: &Stored) -> String {
    format!("{}\0{}", stored.credential.vpc, stored.credential.uid)
}

pub(crate) async fn list_account(
    host: &HostContext,
    endpoints: Arc<Endpoints>,
    account: &AccountId,
) -> Result<ModelList, PluginError> {
    let stored = refresh::ensure_fresh(host, Arc::clone(&endpoints), account).await?;
    let listed = fetch(host, endpoints, &stored).await?;
    let models = listed
        .iter()
        .map(ModelInfo::spec)
        .collect::<Result<Vec<_>, _>>()?;
    if models.is_empty() {
        return Err(PluginError::decode("Qoder model list listed nothing"));
    }
    Ok(ModelList::new(models, None))
}

async fn fetch(
    host: &HostContext,
    endpoints: Arc<Endpoints>,
    stored: &Stored,
) -> Result<Vec<ModelInfo>, PluginError> {
    let site = endpoints.resolve(&stored.credential)?;
    let url = format!("{}{MODELS_PATH}", site.chat.trim_end_matches('/'));
    let signature = cosy::sign(&url, &stored.credential, "", host.clock().now())?;
    let mut call = Call::get(url);
    for (name, value) in identity::model_headers(&stored.credential) {
        call = call.sensitive_header(name, value);
    }
    let reply = cosy::signed_call(call, signature)
        .fetch(host.http())
        .await?;
    match reply.status() {
        200..=299 => parse_list(reply.body()),
        401 | 403 => Err(PluginError::status_error(
            reply.status(),
            "Qoder refused the sign-in while listing models",
        )),
        status => Err(PluginError::status_error(
            status,
            "Qoder did not return a model list",
        )),
    }
}

pub(crate) fn parse_list(body: &[u8]) -> Result<Vec<ModelInfo>, PluginError> {
    let root: Value = hofund_plugin_sdk::json::from_slice(body)
        .map_err(|_| PluginError::decode("Qoder models JSON is invalid"))?;
    let entries = scene_models(&root);
    let models = entries
        .into_iter()
        .filter(|entry| {
            entry.get("enable").and_then(Value::as_bool) == Some(true)
                && !matches!(
                    entry.get("key").and_then(Value::as_str).unwrap_or(""),
                    "" | "auto" | "default"
                )
        })
        .map(model_info)
        .collect();
    Ok(models)
}

fn scene_models(root: &Value) -> Vec<&Value> {
    let app = root.get("app").and_then(Value::as_array);
    let chat = root.get("chat").and_then(Value::as_array);
    let Some(chat) = chat else {
        return app.map_or_else(Vec::new, |items| items.iter().collect());
    };
    let Some(app) = app else {
        return chat.iter().collect();
    };
    let mut out: Vec<&Value> = app.iter().collect();
    let mut after = 0_usize;
    for model in chat {
        let key = model.get("key").and_then(Value::as_str);
        if let Some(index) = out
            .iter()
            .position(|listed| listed.get("key").and_then(Value::as_str) == key)
        {
            after = index + 1;
        } else {
            out.insert(after, model);
            after += 1;
        }
    }
    out
}

fn model_info(raw: &Value) -> ModelInfo {
    let (windows, default_window) = windows_of(raw);
    let context = windows
        .last()
        .copied()
        .or_else(|| raw.get("max_input_tokens").and_then(Value::as_u64))
        .unwrap_or(0);
    let key = raw
        .get("key")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let mut info = ModelInfo {
        name: raw
            .get("display_name")
            .and_then(Value::as_str)
            .unwrap_or(&key)
            .to_owned(),
        source: raw
            .get("source")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        key,
        context,
        windows,
        default_window,
        images: raw.get("is_vl").and_then(Value::as_bool).unwrap_or(false),
        thinks: raw
            .get("is_reasoning")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        always_thinks: false,
        efforts: raw
            .get("reasoning_efforts")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        default_effort: String::new(),
        free: free_of(raw),
        raw: raw.clone(),
    };
    if let Some(config) = raw.get("thinking_config").and_then(Value::as_object) {
        info.thinks = config.contains_key("enabled");
        info.always_thinks = info.thinks && !config.contains_key("disabled");
        info.efforts.clear();
        if let Some(efforts) = config
            .get("enabled")
            .and_then(|enabled| enabled.get("efforts"))
            .and_then(Value::as_object)
        {
            for (name, settings) in efforts {
                info.efforts.push(name.clone());
                if settings.get("is_default").and_then(Value::as_bool) == Some(true) {
                    info.default_effort = name.clone();
                }
            }
        }
        info.efforts.sort_by(|left, right| {
            effort_rank(left)
                .cmp(&effort_rank(right))
                .then_with(|| left.cmp(right))
        });
    }
    info
}

fn effort_rank(effort: &str) -> usize {
    EFFORT_ORDER
        .iter()
        .position(|candidate| *candidate == effort)
        .unwrap_or(EFFORT_ORDER.len())
}

fn windows_of(raw: &Value) -> (Vec<u64>, u64) {
    let mut windows = Vec::new();
    let mut default = 0;
    if let Some(config) = raw.get("context_config").and_then(Value::as_object) {
        for entry in config.values() {
            let Some(tokens) = entry.get("token_count").and_then(Value::as_u64) else {
                continue;
            };
            if tokens > 0 && !windows.contains(&tokens) {
                windows.push(tokens);
            }
            let is_default = entry.get("is_default").is_some_and(|value| {
                value.as_bool() == Some(true)
                    || value.as_u64() == Some(1)
                    || value.as_str() == Some("1")
            });
            if is_default {
                default = tokens;
            }
        }
    }
    if windows.is_empty() {
        if let Some(listed) = raw
            .get("available_context_windows")
            .and_then(Value::as_array)
        {
            for tokens in listed.iter().filter_map(Value::as_u64) {
                if tokens > 0 && !windows.contains(&tokens) {
                    windows.push(tokens);
                }
            }
        }
        default = raw
            .get("default_context_window")
            .and_then(Value::as_u64)
            .filter(|tokens| windows.contains(tokens))
            .unwrap_or(0);
    }
    windows.sort_unstable();
    (windows, default)
}

fn free_of(raw: &Value) -> bool {
    if let Some(price) = raw
        .get("price_factor")
        .or_else(|| raw.get("priceFactor"))
        .and_then(Value::as_f64)
    {
        if price != 0.0 {
            return false;
        }
        let promotion = raw.get("promotion").or_else(|| raw.get("prommotion"));
        let before = promotion
            .and_then(|value| value.get("before_promotion_price_factor"))
            .and_then(Value::as_f64);
        return !(promotion
            .and_then(|value| value.get("active"))
            .and_then(Value::as_bool)
            == Some(true)
            && before.is_some_and(|price| price > 0.0));
    }
    raw.get("is_free").and_then(Value::as_bool) == Some(true)
}

pub(crate) fn model_config(model: &ModelInfo, thinking: bool) -> Result<Value, PluginError> {
    if model.key.is_empty() || matches!(model.key.as_str(), "auto" | "default") {
        return Err(PluginError::status_error(
            400,
            "Qoder request has no model key",
        ));
    }
    let raw = &model.raw;
    let max = raw
        .get("max_input_tokens")
        .and_then(Value::as_u64)
        .filter(|tokens| *tokens > 0)
        .unwrap_or(200_000);
    let mut config = hofund_plugin_sdk::json::json!({
        "key": model.key,
        "display_name": model.name,
        "format": raw.get("format").and_then(Value::as_str).unwrap_or("openai"),
        "is_vl": model.images,
        "is_reasoning": raw.get("is_reasoning").and_then(Value::as_bool).unwrap_or(false) && thinking,
        "api_key": "",
        "url": "",
        "source": raw.get("source").and_then(Value::as_str).unwrap_or("system"),
        "max_input_tokens": max,
    });
    if let Some(provider) = raw.get("outer_provider") {
        config["outer_provider"] = provider.clone();
    } else {
        config["model"] = Value::String(String::new());
    }
    Ok(config)
}
