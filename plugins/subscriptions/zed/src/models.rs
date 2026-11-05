//! Zed `/models` list parsing, vendor cache, and live fetch with snapshot fallback.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use hofund_plugin_api::{AccountId, HostContext, ModelList, ModelSpec, PluginError};
use hofund_plugin_sdk::json::Value;

use crate::body;
use crate::creds::Stored;
use crate::errors;
use crate::headers;
use crate::org;
use crate::tokens;

const KNOWN_PROVIDERS: &[&str] = &["anthropic", "open_ai", "x_ai", "google"];

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Capabilities {
    pub supports_images: bool,
    pub supports_thinking: bool,
}

type VendorMap = Mutex<HashMap<String, String>>;
type CapMap = Mutex<HashMap<String, Capabilities>>;

fn vendors() -> &'static VendorMap {
    static MAP: OnceLock<VendorMap> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

fn caps() -> &'static CapMap {
    static MAP: OnceLock<CapMap> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn vendor_of(model: &str) -> String {
    vendors()
        .lock()
        .expect("lock")
        .get(model)
        .cloned()
        .unwrap_or_else(|| body::guess_vendor(model).to_owned())
}

pub(crate) fn capabilities(stored: &Stored, model: &str) -> Capabilities {
    if let Some(found) = caps().lock().expect("lock").get(model).copied() {
        return found;
    }
    stored_models_table(stored)
        .as_ref()
        .and_then(|items| capabilities_of(items, model))
        .unwrap_or(Capabilities {
            supports_images: true,
            supports_thinking: true,
        })
}

fn stored_models_table(stored: &Stored) -> Option<Vec<Value>> {
    let root: Value = hofund_plugin_sdk::json::from_str(&stored.credential.models_json).ok()?;
    match root {
        Value::Array(items) => Some(items),
        Value::Object(map) => map.get("models").and_then(|v| v.as_array()).cloned(),
        _ => None,
    }
}

fn capabilities_of(items: &[Value], model: &str) -> Option<Capabilities> {
    let map = items
        .iter()
        .find(|item| item.get("id").and_then(Value::as_str) == Some(model))?;
    Some(Capabilities {
        supports_images: map
            .get("supports_images")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        supports_thinking: map
            .get("supports_thinking")
            .and_then(Value::as_bool)
            .unwrap_or(true),
    })
}

pub(crate) async fn list_account(
    host: &HostContext,
    endpoints: Arc<crate::Endpoints>,
    account: &AccountId,
) -> Result<ModelList, PluginError> {
    let stored = Stored::load(host, account).await?;
    match fetch_live(host, Arc::clone(&endpoints), &stored).await {
        Ok(list) => Ok(list),
        Err(error) if error.kind() == hofund_plugin_api::PluginErrorKind::Auth => Err(error),
        Err(_) => parse_stored_models(&stored),
    }
}

pub(crate) async fn repick_org(
    host: &HostContext,
    endpoints: Arc<crate::Endpoints>,
    stored: &Stored,
) -> Result<Option<String>, PluginError> {
    let url = format!("{}/client/users/me", endpoints.cloud);
    let mut call = hofund_plugin_sdk::Call::get(url)
        .header("Accept", "application/json")
        .sensitive_header(
            "authorization",
            crate::creds::auth_header(&stored.credential),
        );
    for (name, value) in headers::cloud_headers() {
        call = call.header(name, value);
    }
    let reply = match call.fetch(host.http()).await {
        Ok(reply) => reply,
        Err(_) => return Ok(None),
    };
    if !(200..300).contains(&reply.status()) {
        return Ok(None);
    }
    let me = org::parse_me(reply.body())?;
    let next = org::org_of(&me, &stored.credential.org);
    if next.is_empty() || next == stored.credential.org {
        Ok(None)
    } else {
        Ok(Some(next))
    }
}

async fn fetch_live(
    host: &HostContext,
    endpoints: Arc<crate::Endpoints>,
    stored: &Stored,
) -> Result<ModelList, PluginError> {
    let mut renewed = false;
    loop {
        let token = tokens::model_token(host, Arc::clone(&endpoints), stored, renewed).await?;
        let url = format!("{}/models", endpoints.cloud);
        let mut call = hofund_plugin_sdk::Call::get(url)
            .sensitive_header("authorization", format!("Bearer {token}"))
            .header("Accept", "application/json");
        for (name, value) in headers::model_token_headers() {
            call = call.header(name, value);
        }
        let reply = call.fetch(host.http()).await?;
        let status = reply.status();
        if status == 401 {
            return Err(errors::sign_in_expired(&crate::creds::label(
                &stored.credential,
            )));
        }
        let headers: Vec<(String, String)> = Vec::new();
        if !renewed && errors::token_stale(status, &headers) {
            renewed = true;
            continue;
        }
        if status == 402
            && repick_org(host, Arc::clone(&endpoints), stored)
                .await?
                .is_some()
        {
            continue;
        }
        if !(200..300).contains(&status) {
            return Err(PluginError::status_error(
                status,
                "Zed did not return a model list",
            ));
        }
        return parse_list(reply.body());
    }
}

pub(crate) fn parse_stored_models(stored: &Stored) -> Result<ModelList, PluginError> {
    let items = stored_models_table(stored)
        .ok_or_else(|| PluginError::decode("stored Zed models JSON is invalid"))?;
    build_list(&items)
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
        let provider = map.get("provider").and_then(Value::as_str).unwrap_or("");
        if !KNOWN_PROVIDERS.contains(&provider) {
            continue;
        }
        let id = map
            .get("id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty());
        let Some(id) = id else {
            continue;
        };
        vendors()
            .lock()
            .expect("lock")
            .insert(id.to_owned(), provider.to_owned());
        caps().lock().expect("lock").insert(
            id.to_owned(),
            Capabilities {
                supports_images: map
                    .get("supports_images")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                supports_thinking: map
                    .get("supports_thinking")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            },
        );
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
        return Err(PluginError::decode("Zed models JSON listed nothing"));
    }
    Ok(ModelList::new(models, None))
}

#[cfg(test)]
pub(crate) fn clear_vendor_cache_for_tests() {
    vendors().lock().expect("lock").clear();
    caps().lock().expect("lock").clear();
}
