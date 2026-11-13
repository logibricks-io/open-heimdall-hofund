//! Model list parsing and fetch.

use std::sync::Arc;

use hofund_plugin_api::{HostContext, ModelList, ModelSpec, PluginError};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::oauth::secret_text;

use crate::Endpoints;
use crate::creds::Stored;
use crate::identity;
use crate::refresh;

pub(crate) async fn list(
    host: &HostContext,
    endpoints: Arc<Endpoints>,
    stored: &Stored,
) -> Result<ModelList, PluginError> {
    let url = format!(
        "{}/api/v1/models",
        endpoints.openapi_for(stored.credential.site)
    );
    let mut call = Call::get(url);
    for (name, value) in identity::model_headers(&stored.credential) {
        call = call.sensitive_header(name, value);
    }
    let reply = call
        .sensitive_header(
            "authorization",
            format!("Bearer {}", secret_text(&stored.credential.token)),
        )
        .header("Accept", "application/json")
        .fetch(host.http())
        .await?;
    match reply.status() {
        200..=299 => parse_list(reply.body()),
        401 => Err(PluginError::auth(
            "Qoder refused the sign-in while listing models",
        )),
        status => Err(PluginError::status_error(
            status,
            "Qoder did not return a model list",
        )),
    }
}

pub(crate) fn parse_list(body: &[u8]) -> Result<ModelList, PluginError> {
    let root: Value = hofund_plugin_sdk::json::from_slice(body)
        .map_err(|_| PluginError::decode("Qoder models JSON is invalid"))?;
    let items = root
        .get("models")
        .or_else(|| root.get("data"))
        .and_then(Value::as_array)
        .ok_or_else(|| PluginError::decode("Qoder models JSON has no models array"))?;
    let mut models = Vec::new();
    for item in items {
        let Some(map) = item.as_object() else {
            continue;
        };
        let id = map
            .get("id")
            .or_else(|| map.get("model"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty());
        let Some(id) = id else {
            continue;
        };
        let name = map
            .get("name")
            .or_else(|| map.get("display_name"))
            .and_then(Value::as_str)
            .unwrap_or(id);
        if let Ok(spec) = ModelSpec::new(id, Some(name.to_owned())) {
            models.push(spec);
        }
    }
    if models.is_empty() {
        return Err(PluginError::decode("Qoder models JSON listed nothing"));
    }
    Ok(ModelList::new(models, None))
}

pub(crate) async fn list_account(
    host: &HostContext,
    endpoints: Arc<Endpoints>,
    account: &hofund_plugin_api::AccountId,
) -> Result<ModelList, PluginError> {
    let stored = refresh::ensure_fresh(host, Arc::clone(&endpoints), account).await?;
    list(host, endpoints, &stored).await
}
