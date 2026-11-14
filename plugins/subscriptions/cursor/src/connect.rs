//! Connect JSON calls to Cursor's dashboard API.

use std::time::Duration;

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
    let team_id = crate::team::header_value(token);
    unary_opts(host, endpoints, path, token, body, team_id.as_deref(), None).await
}

pub(crate) async fn unary_opts(
    host: &HostContext,
    endpoints: &Endpoints,
    path: &str,
    token: &SecretString,
    body: &str,
    team_id: Option<&str>,
    read_timeout: Option<Duration>,
) -> Result<Value, PluginError> {
    let url = endpoints.dashboard(path);
    unary_url(host, endpoints, url, token, body, team_id, read_timeout).await
}

pub(crate) async fn picker_models(
    host: &HostContext,
    endpoints: &Endpoints,
    token: &SecretString,
) -> Result<Value, PluginError> {
    let team_id = crate::team::header_value(token);
    unary_url(
        host,
        endpoints,
        endpoints.available_models(),
        token,
        r#"{"useModelParameters":true,"doNotUseMarkdown":true}"#,
        team_id.as_deref(),
        Some(Duration::from_secs(30)),
    )
    .await
}

pub(crate) async fn catalog_models(
    host: &HostContext,
    endpoints: &Endpoints,
    token: &SecretString,
) -> Result<Value, PluginError> {
    let team_id = crate::team::header_value(token);
    unary_url(
        host,
        endpoints,
        endpoints.available_models(),
        token,
        r#"{"useModelParameters":true,"doNotUseMarkdown":true,"scope":3}"#,
        team_id.as_deref(),
        Some(Duration::from_secs(8)),
    )
    .await
}

pub(crate) async fn usable_models(
    host: &HostContext,
    endpoints: &Endpoints,
    token: &SecretString,
) -> Result<Value, PluginError> {
    let team_id = crate::team::header_value(token);
    unary_url(
        host,
        endpoints,
        endpoints.usable_models(),
        token,
        "{}",
        team_id.as_deref(),
        Some(Duration::from_secs(8)),
    )
    .await
}

async fn unary_url(
    host: &HostContext,
    endpoints: &Endpoints,
    url: String,
    token: &SecretString,
    body: &str,
    team_id: Option<&str>,
    read_timeout: Option<Duration>,
) -> Result<Value, PluginError> {
    let mut request = Call::post(url)
        .header("content-type", "application/json")
        .header("connect-protocol-version", "1")
        .sensitive_header(
            "authorization",
            format!("Bearer {}", hofund_plugin_sdk::oauth::secret_text(token)),
        )
        .header("x-cursor-client-version", &endpoints.client_version)
        .header("x-cursor-client-type", "cli")
        .header("x-ghost-mode", "true");
    if let Some(team) = team_id.filter(|value| !value.is_empty()) {
        request = request.header("x-cursor-team-id", team);
    }
    if let Some(budget) = read_timeout {
        request = request.read_timeout(budget);
    }
    let reply = request.bytes(body.as_bytes()).fetch(host.http()).await?;
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
