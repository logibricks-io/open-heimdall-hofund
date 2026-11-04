//! Connect JSON calls to Cursor's dashboard API.

use hofund_plugin_api::{HostContext, PluginError, SecretString};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::Value;

use crate::endpoints::Endpoints;

pub(crate) async fn unary(
    host: &HostContext,
    endpoints: &Endpoints,
    path: &str,
    token: &SecretString,
    body: &str,
) -> Result<Value, PluginError> {
    let url = endpoints.dashboard(path);
    let reply = Call::post(url)
        .header("content-type", "application/json")
        .header("connect-protocol-version", "1")
        .header(
            "authorization",
            format!("Bearer {}", hofund_plugin_sdk::oauth::secret_text(token)),
        )
        .header("x-cursor-client-version", &endpoints.client_version)
        .header("x-cursor-client-type", "cli")
        .header("x-ghost-mode", "true")
        .bytes(body.as_bytes())
        .fetch(host.http())
        .await?;
    if !(200..300).contains(&reply.status()) {
        return Err(PluginError::status_error(
            reply.status(),
            "Cursor's API refused the request",
        ));
    }
    hofund_plugin_sdk::json::from_slice(reply.body())
        .map_err(|error| PluginError::decode(format!("Cursor sent unreadable JSON: {error}")))
}

pub(crate) async fn who_is(
    host: &HostContext,
    endpoints: &Endpoints,
    token: &SecretString,
) -> Result<(String, Option<String>), PluginError> {
    let me = unary(host, endpoints, "GetMe", token, "{}").await;
    let plan = unary(host, endpoints, "GetPlanInfo", token, "{}").await;
    let email = me
        .ok()
        .and_then(|value| {
            value
                .get("email")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_default();
    let plan_name = plan
        .ok()
        .and_then(|value| {
            value
                .get("planInfo")
                .and_then(|info| info.get("planName"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    if email.is_empty() {
        return Err(PluginError::auth(
            "Cursor did not say which account is signed in",
        ));
    }
    Ok((email, plan_name))
}
