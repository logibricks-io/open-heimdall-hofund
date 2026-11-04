//! Factory API envelope reads and writes.

use hofund_plugin_api::{HostContext, PluginError, SecretString};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::{self, Value};

use crate::Endpoints;
use crate::client;

/// Prefix OAuth access tokens carry in `Authorization`.
pub(crate) const WORKOS_PREFIX: &str = "workos:";

/// Reads a Factory `{success, data}` reply or plain JSON.
pub(crate) fn decode_data(body: &[u8]) -> Result<Value, PluginError> {
    let value: Value =
        json::from_slice(body).map_err(|_| PluginError::decode("Factory's reply is not JSON"))?;
    let Value::Object(object) = value else {
        return Ok(value);
    };
    if let Some(success) = object.get("success").and_then(Value::as_bool)
        && !success
    {
        return Err(PluginError::status_error(
            502,
            "Factory refused the request",
        ));
    }
    if object.contains_key("data") {
        return Ok(object.get("data").cloned().unwrap_or(Value::Null));
    }
    Ok(Value::Object(object))
}

/// Whether an HTTP status and body mean the sign-in itself is gone.
pub(crate) fn sign_in_lapsed(status: u16, body: &[u8]) -> bool {
    if status == 401 || status == 403 {
        return true;
    }
    if status == 400 && body_lapsed(body) {
        return true;
    }
    false
}

fn body_lapsed(body: &[u8]) -> bool {
    let text = String::from_utf8_lossy(body).to_ascii_lowercase();
    ["invalid", "expired", "revoked", "unauthorized"]
        .iter()
        .any(|word| text.contains(word))
}

/// GET one Factory path with client headers and optional bearer.
pub(crate) async fn get(
    host: &HostContext,
    endpoints: &Endpoints,
    path: &str,
    bearer: Option<&SecretString>,
    lapsed: bool,
) -> Result<Value, PluginError> {
    let url = format!("{}{path}", endpoints.api.trim_end_matches('/'));
    let mut call = Call::get(url);
    for (name, value) in client::headers(None) {
        call = call.header(name, value);
    }
    if let Some(token) = bearer {
        call = call.bearer(token)?;
    }
    let reply = call.fetch(host.http()).await?;
    let status = reply.status();
    let body = reply.body();
    if lapsed && sign_in_lapsed(status, body) {
        return Err(PluginError::auth(
            "Factory refused the stored sign-in; sign in to Factory again",
        ));
    }
    if !(200..300).contains(&status) {
        return Err(PluginError::status_error(
            status,
            format!("Factory request failed (HTTP {status})"),
        ));
    }
    decode_data(body)
}

/// POST JSON to one Factory path.
pub(crate) async fn post_json(
    host: &HostContext,
    endpoints: &Endpoints,
    path: &str,
    body: &Value,
    bearer: Option<&SecretString>,
    lapsed: bool,
) -> Result<Value, PluginError> {
    let url = format!("{}{path}", endpoints.api.trim_end_matches('/'));
    let payload = json::to_vec(body)
        .map_err(|_| PluginError::decode("Factory request body could not be encoded"))?;
    let mut call = Call::post(url)
        .header("content-type", "application/json")
        .bytes(payload);
    for (name, value) in client::headers(None) {
        call = call.header(name, value);
    }
    if let Some(token) = bearer {
        call = call.bearer(token)?;
    }
    let reply = call.fetch(host.http()).await?;
    let status = reply.status();
    let raw = reply.body();
    if lapsed && sign_in_lapsed(status, raw) {
        return Err(PluginError::auth(
            "Factory refused the stored sign-in; sign in to Factory again",
        ));
    }
    if !(200..300).contains(&status) {
        if status == 401 || status == 403 {
            return Err(PluginError::auth(format!(
                "Factory request failed (HTTP {status})"
            )));
        }
        return Err(PluginError::status_error(
            status,
            format!("Factory request failed (HTTP {status})"),
        ));
    }
    decode_data(raw)
}

/// Builds the bearer string Factory expects for OAuth tokens.
#[must_use]
pub(crate) fn bearer_oauth(access: &str) -> String {
    if access.to_ascii_lowercase().starts_with(WORKOS_PREFIX) {
        access.to_owned()
    } else {
        format!("{WORKOS_PREFIX}{access}")
    }
}
