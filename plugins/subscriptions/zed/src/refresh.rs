//! Mint short-lived model tokens from Zed cloud.

use std::sync::Arc;

use hofund_plugin_api::{AccountId, HostContext, PluginError};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::Value;

use crate::Endpoints;
use crate::creds::{self, Stored};

pub(crate) async fn model_token(
    host: &HostContext,
    endpoints: Arc<Endpoints>,
    account: &AccountId,
) -> Result<String, PluginError> {
    let stored = Stored::load(host, account).await?;
    mint(host, &endpoints, &stored).await
}

pub(crate) async fn run(
    host: &HostContext,
    endpoints: Arc<Endpoints>,
    account: &AccountId,
) -> Result<(), PluginError> {
    let _ = model_token(host, endpoints, account).await?;
    Ok(())
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
    if !stored.credential.system_id.is_empty() {
        call = call.header("x-zed-system-id", stored.credential.system_id.clone());
    }
    let reply = call.bytes(body.into_bytes()).fetch(host.http()).await?;
    match reply.status() {
        200..=299 => {}
        401 | 403 => {
            return Err(PluginError::auth(
                "the Zed sign-in has expired; import credentials again",
            ));
        }
        status => {
            return Err(PluginError::status_error(
                status,
                "Zed did not mint a model token",
            ));
        }
    }
    let root: Value = hofund_plugin_sdk::json::from_slice(reply.body())
        .map_err(|_| PluginError::decode("Zed model token JSON is invalid"))?;
    root.get("token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| PluginError::auth("Zed sent back no model token"))
}
