//! Factory API reads and writes.

use hofund_plugin_api::{HostContext, PluginError, SecretString};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::{self, Value};

use crate::client;

/// Whether an HTTP status means the sign-in itself is gone.
pub(crate) fn sign_in_lapsed(status: u16) -> bool {
    status == 401 || status == 403
}

fn join_url(base: &str, path: &str) -> String {
    format!("{}{path}", base.trim_end_matches('/'))
}

fn apply_headers(
    mut call: Call,
    bearer: Option<&SecretString>,
    org_id: Option<&str>,
    whoami_extended: bool,
) -> Result<Call, PluginError> {
    for (name, value) in client::headers() {
        call = call.header(name, value);
    }
    if whoami_extended {
        call = call.header("x-factory-whoami-extended", "true");
    }
    if let Some(org) = org_id.filter(|text| !text.is_empty()) {
        call = call.header("x-factory-org-id", org);
    }
    if let Some(token) = bearer {
        call = call.bearer(token)?;
    }
    Ok(call)
}

/// GET `/api/cli/whoami` with Factory client headers.
pub(crate) async fn whoami(
    host: &HostContext,
    api_base: &str,
    bearer: &SecretString,
    org_id: Option<&str>,
) -> Result<Value, PluginError> {
    get(
        host,
        api_base,
        "/api/cli/whoami",
        Some(bearer),
        org_id,
        true,
    )
    .await
}

/// GET one Factory path with client headers and optional bearer.
pub(crate) async fn get(
    host: &HostContext,
    api_base: &str,
    path: &str,
    bearer: Option<&SecretString>,
    org_id: Option<&str>,
    lapsed: bool,
) -> Result<Value, PluginError> {
    let url = join_url(api_base, path);
    let whoami = path == "/api/cli/whoami";
    let call = apply_headers(Call::get(url), bearer, org_id, whoami)?;
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
#[allow(dead_code)]
pub(crate) async fn post_json(
    host: &HostContext,
    api_base: &str,
    path: &str,
    body: &Value,
    bearer: Option<&SecretString>,
    org_id: Option<&str>,
    lapsed: bool,
) -> Result<Value, PluginError> {
    let url = join_url(api_base, path);
    let payload = json::to_vec(body)
        .map_err(|_| PluginError::decode("Factory request body could not be encoded"))?;
    let call = apply_headers(
        Call::post(url)
            .header("content-type", "application/json")
            .bytes(payload),
        bearer,
        org_id,
        false,
    )?;
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
