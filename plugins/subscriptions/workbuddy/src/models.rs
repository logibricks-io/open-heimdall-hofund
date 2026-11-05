//! Model list from WorkBuddy `/v3/config`.

use std::sync::Arc;

use hofund_plugin_api::{HostContext, ModelList, ModelSpec, PluginError};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::Value;

use crate::Endpoints;
use crate::creds::{Stored, bearer_headers};

pub(crate) async fn list_account(
    host: &HostContext,
    endpoints: Arc<Endpoints>,
    stored: &Stored,
) -> Result<ModelList, PluginError> {
    let base = endpoints.base_for(stored.credential.site);
    let url = format!("{base}/v3/config");
    let mut call = Call::get(url)
        .header("Accept", "application/json")
        .header("User-Agent", stored.credential.site.user_agent());
    for (name, value) in bearer_headers(&stored.credential) {
        if name == "Authorization" {
            call = call.sensitive_header("authorization", value);
        } else if name != "User-Agent" {
            call = call.header(name, value);
        }
    }
    let reply = call.fetch(host.http()).await?;
    match reply.status() {
        200..=299 => parse_config(reply.body()),
        401 => Err(PluginError::auth(
            "WorkBuddy refused the sign-in while reading models",
        )),
        status => Err(PluginError::status_error(
            status,
            "WorkBuddy did not return model config",
        )),
    }
}

pub(crate) fn parse_config(body: &[u8]) -> Result<ModelList, PluginError> {
    let root: Value = hofund_plugin_sdk::json::from_slice(body)
        .map_err(|_| PluginError::decode("WorkBuddy config JSON is invalid"))?;
    let items = root
        .get("models")
        .and_then(Value::as_array)
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
        if let Ok(spec) = ModelSpec::new(id, Some(name.to_owned())) {
            models.push(spec);
        }
    }
    if models.is_empty() {
        return Err(PluginError::decode("WorkBuddy config listed no models"));
    }
    Ok(ModelList::new(models, None))
}
