//! Zed `/models` list parsing, vendor cache, and live fetch with snapshot fallback.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use hofund_plugin_api::{AccountId, HostContext, ModelList, ModelSpec, PluginError};
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::transport::HttpResponse;

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

fn cache_key(user_id: &str, model: &str) -> String {
    format!("{user_id}:{model}")
}

pub(crate) fn vendor_of(stored: &Stored, model: &str) -> String {
    let key = cache_key(&stored.credential.user_id, model);
    vendors()
        .lock()
        .expect("lock")
        .get(&key)
        .cloned()
        .unwrap_or_else(|| body::guess_vendor(model).to_owned())
}

pub(crate) fn capabilities(stored: &Stored, model: &str) -> Capabilities {
    let key = cache_key(&stored.credential.user_id, model);
    if let Some(found) = caps().lock().expect("lock").get(&key).copied() {
        return found;
    }
    stored_models_table(stored)
        .as_ref()
        .and_then(|items| capabilities_of(items, model))
        .unwrap_or_default()
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
            .unwrap_or(false),
        supports_thinking: map
            .get("supports_thinking")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

pub(crate) async fn list_account(
    host: &HostContext,
    endpoints: Arc<crate::Endpoints>,
    account: &AccountId,
) -> Result<ModelList, PluginError> {
    let mut stored = Stored::load(host, account).await?;
    match fetch_live(host, Arc::clone(&endpoints), account, &mut stored).await {
        Ok(list) => Ok(list),
        Err(error) if error.kind() == hofund_plugin_api::PluginErrorKind::Auth => Err(error),
        Err(error) if error.kind() == hofund_plugin_api::PluginErrorKind::Decode => Err(error),
        Err(error)
            if error.kind() == hofund_plugin_api::PluginErrorKind::Status
                && matches!(error.status(), Some(401..=403)) =>
        {
            Err(error)
        }
        Err(_) => parse_stored_models(&stored),
    }
}

pub(crate) async fn repick_and_apply(
    host: &HostContext,
    endpoints: Arc<crate::Endpoints>,
    account: &AccountId,
    stored: &mut Stored,
) -> Result<bool, PluginError> {
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
        Err(_) => return Ok(false),
    };
    if !(200..300).contains(&reply.status()) {
        return Ok(false);
    }
    let me = org::parse_me(reply.body())?;
    let next = org::org_of(&me, &stored.credential.org);
    if next.is_empty() || next == stored.credential.org {
        return Ok(false);
    }
    org::apply_org(&mut stored.credential, &me);
    crate::creds::persist(host, account, &stored.credential).await?;
    Ok(true)
}

async fn fetch_live(
    host: &HostContext,
    endpoints: Arc<crate::Endpoints>,
    account: &AccountId,
    stored: &mut Stored,
) -> Result<ModelList, PluginError> {
    let mut renewed = false;
    let mut moved = false;
    loop {
        let token = tokens::model_token(host, Arc::clone(&endpoints), stored, renewed).await?;
        let url = format!("{}/models", endpoints.cloud);
        let mut call = hofund_plugin_sdk::Call::get(url)
            .sensitive_header("authorization", format!("Bearer {token}"))
            .header("Accept", "application/json");
        for (name, value) in headers::model_token_headers() {
            call = call.header(name, value);
        }
        let response = call.open_raw(host.http(), 2).await?;
        let status = response.status().as_u16();
        let header_rows: Vec<(String, String)> = response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    value.to_str().unwrap_or("").to_owned(),
                )
            })
            .collect();
        if !renewed && errors::token_stale(status, &header_rows) {
            renewed = true;
            continue;
        }
        if status == 401 {
            return Err(PluginError::status_error(
                status,
                errors::failure_message(status, ""),
            ));
        }
        if status == 402 && !moved {
            moved = true;
            if repick_and_apply(host, Arc::clone(&endpoints), account, stored).await? {
                continue;
            }
        }
        if !(200..300).contains(&status) {
            return Err(PluginError::status_error(
                status,
                "Zed did not return a model list",
            ));
        }
        let body = read_body(response).await?;
        match parse_list(&body) {
            Ok(list) => return Ok(list),
            Err(error) if !moved => {
                moved = true;
                if repick_and_apply(host, Arc::clone(&endpoints), account, stored).await? {
                    continue;
                }
                return Err(error);
            }
            Err(error) => return Err(error),
        }
    }
}

async fn read_body(response: HttpResponse) -> Result<Vec<u8>, PluginError> {
    let mut body = response.into_body();
    let mut out = Vec::new();
    while let Some(chunk) = body.next_chunk().await {
        out.extend_from_slice(&chunk?);
    }
    Ok(out)
}

pub(crate) fn parse_stored_models(stored: &Stored) -> Result<ModelList, PluginError> {
    let items = stored_models_table(stored)
        .ok_or_else(|| PluginError::decode("stored Zed models JSON is invalid"))?;
    build_list(&stored.credential.user_id, &items)
}

pub(crate) fn parse_list(body: &[u8]) -> Result<ModelList, PluginError> {
    let root: Value = hofund_plugin_sdk::json::from_slice(body)
        .map_err(|_| PluginError::decode("Zed models JSON is invalid"))?;
    let items = root
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| PluginError::decode("Zed models JSON has no models array"))?;
    build_list("", items)
}

fn build_list(user_id: &str, items: &[Value]) -> Result<ModelList, PluginError> {
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
        if !user_id.is_empty() {
            vendors()
                .lock()
                .expect("lock")
                .insert(cache_key(user_id, id), provider.to_owned());
            caps().lock().expect("lock").insert(
                cache_key(user_id, id),
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
        }
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
#[allow(dead_code)]
pub(crate) fn clear_vendor_cache_for_tests(user_id: &str) {
    let prefix = format!("{user_id}:");
    vendors()
        .lock()
        .expect("lock")
        .retain(|key, _| !key.starts_with(&prefix));
    caps()
        .lock()
        .expect("lock")
        .retain(|key, _| !key.starts_with(&prefix));
}
