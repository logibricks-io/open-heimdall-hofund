//! Factory API reads and writes.

use hofund_plugin_api::{HostContext, PluginError, SecretString};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::{self, Value};

use crate::Endpoints;
use crate::client;

/// Whether an HTTP status means the sign-in itself is gone.
pub(crate) fn sign_in_lapsed(status: u16) -> bool {
    status == 401 || status == 403
}

fn join_url(base: &str, path: &str) -> String {
    format!("{}{path}", base.trim_end_matches('/'))
}

/// GET one Factory path with client headers and optional bearer.
pub(crate) async fn get(
    host: &HostContext,
    endpoints: &Endpoints,
    path: &str,
    bearer: Option<&SecretString>,
    lapsed: bool,
) -> Result<Value, PluginError> {
    let url = join_url(endpoints.api_base(), path);
    let mut call = Call::get(url);
    for (name, value) in client::headers() {
        call = call.header(name, value);
    }
    if let Some(token) = bearer {
        call = call.bearer(token)?;
    }
    let reply = call.fetch(host.http()).await?;
    let status = reply.status();
    let body = reply.body();
    if lapsed && sign_in_lapsed(status) {
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
    json::from_slice(body).map_err(|_| PluginError::decode("Factory's reply is not JSON"))
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
    let url = join_url(endpoints.api_base(), path);
    let payload = json::to_vec(body)
        .map_err(|_| PluginError::decode("Factory request body could not be encoded"))?;
    let mut call = Call::post(url)
        .header("content-type", "application/json")
        .bytes(payload);
    for (name, value) in client::headers() {
        call = call.header(name, value);
    }
    if let Some(token) = bearer {
        call = call.bearer(token)?;
    }
    let reply = call.fetch(host.http()).await?;
    let status = reply.status();
    let raw = reply.body();
    if lapsed && sign_in_lapsed(status) {
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
    json::from_slice(raw).map_err(|_| PluginError::decode("Factory's reply is not JSON"))
}
