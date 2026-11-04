//! Zed `/models` list parsing.

use std::sync::Arc;

use hofund_plugin_api::{HostContext, ModelList, ModelSpec, PluginError};
use hofund_plugin_sdk::json::Value;

use crate::creds::Stored;
use crate::refresh;

pub(crate) async fn list_account(
    host: &HostContext,
    endpoints: std::sync::Arc<crate::Endpoints>,
    account: &hofund_plugin_api::AccountId,
) -> Result<ModelList, PluginError> {
    let stored = Stored::load(host, account).await?;
    if let Ok(list) = parse_stored_models(&stored) {
        return Ok(list);
    }
    let token = refresh::model_token(host, Arc::clone(&endpoints), account).await?;
    let url = format!("{}/models", endpoints.cloud);
    let reply = hofund_plugin_sdk::Call::get(url)
        .sensitive_header("authorization", format!("Bearer {token}"))
        .header("Accept", "application/json")
        .fetch(host.http())
        .await?;
    if !(200..300).contains(&reply.status()) {
        return parse_stored_models(&stored);
    }
    parse_list(reply.body())
}

pub(crate) fn parse_stored_models(stored: &Stored) -> Result<ModelList, PluginError> {
    let root: Value = hofund_plugin_sdk::json::from_str(&stored.credential.models_json)
        .map_err(|_| PluginError::decode("stored Zed models JSON is invalid"))?;
    let items = root
        .as_array()
        .ok_or_else(|| PluginError::decode("stored Zed models JSON must be an array"))?;
    build_list(items)
}

pub(crate) fn parse_list(body: &[u8]) -> Result<ModelList, PluginError> {
    let root: Value = hofund_plugin_sdk::json::from_slice(body)
        .map_err(|_| PluginError::decode("Zed models JSON is invalid"))?;
    let items = root
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| PluginError::decode("Zed models JSON has no models array"))?;
    build_list(items)
}

fn build_list(items: &[Value]) -> Result<ModelList, PluginError> {
    let mut models = Vec::new();
    for item in items {
        let Some(map) = item.as_object() else {
            continue;
        };
        if map.get("is_disabled").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let id = map
            .get("id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty());
        let Some(id) = id else {
            continue;
        };
        let name = map.get("name").and_then(Value::as_str).unwrap_or(id);
        if let Ok(spec) = ModelSpec::new(id, Some(name.to_owned())) {
            models.push(spec);
        }
    }
    if models.is_empty() {
        return Err(PluginError::decode("Zed models JSON listed nothing"));
    }
    Ok(ModelList::new(models, None))
}
