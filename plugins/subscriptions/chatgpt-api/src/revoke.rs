//! Refresh-token revocation on sign-out.

use hofund_plugin_api::{HostContext, PluginError};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::form_body;
use hofund_plugin_sdk::oauth::{SecretObject, secret_text};

use crate::Endpoints;

pub(crate) async fn revoke(
    host: &HostContext,
    endpoints: &Endpoints,
    blob: &SecretObject,
) -> Result<(), PluginError> {
    let client_id = blob.text("client_id").unwrap_or_default();
    let refresh = match blob.secret("refresh_token") {
        Some(token) => token,
        None => return Ok(()),
    };
    if client_id.is_empty() {
        return Ok(());
    }
    let discovery = format!(
        "{}/.well-known/openid-configuration",
        endpoints.issuer.trim_end_matches('/')
    );
    let reply = Call::get(discovery)
        .header("accept", "application/json")
        .fetch(host.http())
        .await?;
    let endpoint =
        hofund_plugin_sdk::json::from_slice::<hofund_plugin_sdk::json::Value>(reply.body())
            .ok()
            .and_then(|value| {
                value
                    .get("revocation_endpoint")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
            });
    let Some(endpoint) = endpoint.filter(|url| !url.is_empty()) else {
        return Err(PluginError::unsupported(
            "ChatGPT API: OpenAI names no revocation endpoint",
        ));
    };
    let form = form_body(&[
        ("token", secret_text(&refresh)),
        ("token_type_hint", "refresh_token"),
        ("client_id", client_id),
    ]);
    let reply = Call::post(&endpoint)
        .header("content-type", "application/x-www-form-urlencoded")
        .bytes(form)
        .fetch(host.http())
        .await?;
    if reply.status() != 200 {
        return Err(PluginError::status_error(
            reply.status(),
            format!("ChatGPT API sign-out: HTTP {}", reply.status()),
        ));
    }
    Ok(())
}
