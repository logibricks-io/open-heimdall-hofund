//! In-memory model tokens minted from `/client/llm_tokens`.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use hofund_plugin_api::{HostContext, PluginError};
use hofund_plugin_sdk::Call;

use crate::Endpoints;
use crate::creds::{self, Credential, Stored};
use crate::errors;
use crate::headers;

type TokenMap = Mutex<HashMap<String, String>>;

fn cache() -> &'static TokenMap {
    static MAP: OnceLock<TokenMap> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

fn key(credential: &Credential) -> String {
    format!(
        "{} {} {}",
        credential.user_id,
        credential.org,
        hofund_plugin_sdk::oauth::secret_text(&credential.access)
    )
}

pub(crate) async fn model_token(
    host: &HostContext,
    endpoints: std::sync::Arc<Endpoints>,
    stored: &Stored,
    renew: bool,
) -> Result<String, PluginError> {
    let cache_key = key(&stored.credential);
    if !renew {
        if let Some(token) = cache().lock().expect("lock").get(&cache_key).cloned() {
            return Ok(token);
        }
    } else {
        cache().lock().expect("lock").remove(&cache_key);
    }
    let token = mint(host, &endpoints, stored).await?;
    cache()
        .lock()
        .expect("lock")
        .insert(cache_key, token.clone());
    Ok(token)
}

async fn mint(
    host: &HostContext,
    endpoints: &Endpoints,
    stored: &Stored,
) -> Result<String, PluginError> {
    let url = format!("{}/client/llm_tokens", endpoints.cloud);
    let body = hofund_plugin_sdk::json::json!({
        "organization_id": stored.credential.org,
    })
    .to_string();
    let mut call = Call::post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .sensitive_header("authorization", creds::auth_header(&stored.credential));
    for (name, value) in headers::cloud_headers() {
        call = call.header(name, value);
    }
    if !stored.credential.system_id.is_empty() {
        call = call.header("x-zed-system-id", stored.credential.system_id.clone());
    }
    let reply = call.bytes(body.into_bytes()).fetch(host.http()).await?;
    let status = reply.status();
    match status {
        200..=299 => {}
        401 | 403 => {
            let who = creds::label(&stored.credential);
            if status == 401 {
                return Err(errors::sign_in_expired(&who));
            }
            return Err(PluginError::status_error(
                status,
                errors::failure_message(status, std::str::from_utf8(reply.body()).unwrap_or("")),
            ));
        }
        status => {
            return Err(PluginError::status_error(
                status,
                "Zed did not mint a model token",
            ));
        }
    }
    let root: hofund_plugin_sdk::json::Value = hofund_plugin_sdk::json::from_slice(reply.body())
        .map_err(|_| PluginError::decode("Zed model token JSON is invalid"))?;
    root.get("token")
        .and_then(hofund_plugin_sdk::json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| PluginError::auth("Zed sent back no model token"))
}

#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn clear_cache_for_tests() {
    cache().lock().expect("lock").clear();
}
