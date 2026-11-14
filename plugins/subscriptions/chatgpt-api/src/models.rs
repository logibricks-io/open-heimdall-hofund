//! Model list from OpenAI `/v1/models`.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use hofund_plugin_api::{AccountId, HostContext, ModelList, ModelSpec, PluginError};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::oauth::secret_text;

use crate::Endpoints;
use crate::errors;
use crate::tokens::Access;

const CACHE_FOR: Duration = Duration::from_secs(60 * 60);

#[derive(Default)]
pub(crate) struct Cache(Mutex<HashMap<AccountId, (SystemTime, ModelList)>>);

impl Cache {
    pub(crate) fn get(&self, account: &AccountId, now: SystemTime) -> Option<ModelList> {
        let guard = self.0.lock().ok()?;
        guard.get(account).and_then(|(at, list)| {
            (now.duration_since(*at).unwrap_or_default() < CACHE_FOR).then(|| list.clone())
        })
    }

    pub(crate) fn put(&self, account: &AccountId, list: ModelList, now: SystemTime) {
        if let Ok(mut guard) = self.0.lock() {
            guard.insert(account.clone(), (now, list));
        }
    }
}

pub(crate) async fn list(
    host: &HostContext,
    endpoints: &Endpoints,
    access: &Access,
    cache: &Cache,
    account: &AccountId,
) -> Result<ModelList, PluginError> {
    let now = host.clock().now();
    if let Some(list) = cache.get(account, now) {
        return Ok(list);
    }
    let url = format!("{}/models", endpoints.api.trim_end_matches('/'));
    let reply = Call::get(url)
        .sensitive_header(
            "authorization",
            format!("Bearer {}", secret_text(&access.token)),
        )
        .fetch(host.http())
        .await?;
    if !(200..300).contains(&reply.status()) {
        let mut message = format!("ChatGPT API models: HTTP {}", reply.status());
        if let Some(more) = errors::explain(reply.status(), reply.body()) {
            message.push_str(" — ");
            message.push_str(&more);
        }
        return Err(PluginError::status_error(reply.status(), message));
    }
    let parsed = parse(reply.body())?;
    if parsed.models().is_empty() {
        return Err(PluginError::decode(
            "ChatGPT API lists no models for this account",
        ));
    }
    cache.put(account, parsed.clone(), now);
    Ok(parsed)
}

fn parse(body: &[u8]) -> Result<ModelList, PluginError> {
    let value: Value = hofund_plugin_sdk::json::from_slice(body)
        .map_err(|_| PluginError::decode("ChatGPT API sent an unreadable model list"))?;
    let Some(items) = value.get("models").and_then(Value::as_array) else {
        return Ok(ModelList::new(Vec::new(), None));
    };
    let mut specs = Vec::new();
    for item in items {
        let Some(obj) = item.as_object() else {
            continue;
        };
        let slug = obj.get("slug").and_then(Value::as_str).unwrap_or_default();
        if slug.is_empty() || slug.contains('/') {
            continue;
        }
        let visibility = obj
            .get("visibility")
            .and_then(Value::as_str)
            .unwrap_or("list");
        if visibility != "list" && !visibility.is_empty() {
            continue;
        }
        let name = obj
            .get("display_name")
            .and_then(Value::as_str)
            .unwrap_or(slug)
            .to_owned();
        let context = obj
            .get("context_window")
            .and_then(Value::as_u64)
            .unwrap_or(128_000);
        let max_context = obj
            .get("max_context_window")
            .and_then(Value::as_u64)
            .filter(|max| *max > context);
        let images = obj
            .get("input_modalities")
            .and_then(Value::as_array)
            .is_some_and(|mods| mods.iter().any(|m| m.as_str() == Some("image")));
        let efforts: Vec<String> = obj
            .get("supported_reasoning_levels")
            .and_then(Value::as_array)
            .map(|levels| {
                levels
                    .iter()
                    .filter_map(|level| {
                        level
                            .get("effort")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut spec = ModelSpec::new(slug.to_owned(), Some(name))?
            .with_context(context)
            .with_image_input(Some(images))
            .with_endpoints(vec!["responses".to_owned()]);
        if let Some(max) = max_context {
            spec = spec.with_max_output(max);
        }
        if !efforts.is_empty() {
            spec = spec.with_efforts(efforts);
        }
        specs.push(spec);
    }
    Ok(ModelList::new(specs, None))
}

#[cfg(test)]
pub(crate) fn parse_fixture(body: &[u8]) -> Result<ModelList, PluginError> {
    parse(body)
}
