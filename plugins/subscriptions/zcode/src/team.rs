//! Team plan project API key resolution.

use hofund_plugin_api::{HostContext, PluginError, SecretString};
use hofund_plugin_sdk::json::Value;

use crate::Endpoints;
use crate::api;
use crate::creds::PlanAuth;

pub(crate) async fn resolve_project_key(
    host: &HostContext,
    endpoints: &Endpoints,
    auth: &PlanAuth,
) -> Result<SecretString, PluginError> {
    resolve(host, endpoints, auth).await
}

pub(crate) async fn resolve(
    host: &HostContext,
    endpoints: &Endpoints,
    auth: &PlanAuth,
) -> Result<SecretString, PluginError> {
    let token = auth
        .team_token
        .as_ref()
        .ok_or_else(|| PluginError::auth("the team seat has no business sign-in token"))?;
    let biz = endpoints.biz_root(&auth.base);
    let headers = team_headers(&auth.base, &auth.org, &auth.project);
    let header_refs: Vec<(&str, &str)> = headers
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect();
    let list_url = format!(
        "{biz}/api/biz/v1/organization/{}/projects/{}/api_keys",
        auth.org, auth.project
    );
    let device_path = std::sync::Mutex::new(None);
    let data = api::get_envelope(&api::EnvelopeRequest::get(
        host,
        endpoints,
        &list_url,
        Some(token),
        &header_refs,
        &device_path,
    ))
    .await?;
    let key_id = find_team_key_id(&data)?;
    let copy_url = format!(
        "{biz}/api/biz/v1/organization/{}/projects/{}/api_keys/copy/{key_id}",
        auth.org, auth.project
    );
    let secret = api::get_envelope(&api::EnvelopeRequest::get(
        host,
        endpoints,
        &copy_url,
        Some(token),
        &header_refs,
        &device_path,
    ))
    .await?;
    let text = secret
        .get("secret")
        .or_else(|| secret.get("apiKeySecret"))
        .and_then(Value::as_str)
        .filter(|value| value.contains('.'))
        .ok_or_else(|| PluginError::decode("Z Code team key copy returned no secret"))?;
    Ok(SecretString::from(text.to_owned()))
}

fn find_team_key_id(data: &Value) -> Result<String, PluginError> {
    let rows = data
        .as_array()
        .cloned()
        .or_else(|| data.get("list").and_then(Value::as_array).cloned())
        .unwrap_or_default();
    for row in rows {
        let Some(object) = row.as_object() else {
            continue;
        };
        let key_type = object
            .get("keyType")
            .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|n| n as i64)));
        if key_type != Some(2) {
            continue;
        }
        if object
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|name| name.contains("team"))
            && let Some(id) = object.get("apiKey").and_then(Value::as_str)
        {
            return Ok(id.to_owned());
        }
    }
    Err(PluginError::decode(
        "Z Code team project has no team coding plan API key",
    ))
}

pub(crate) fn team_headers(base: &str, org: &str, project: &str) -> Vec<(&'static str, String)> {
    let lang = if base.contains("bigmodel.cn") {
        "zh"
    } else {
        "en"
    };
    vec![
        ("bigmodel-organization", org.to_owned()),
        ("bigmodel-project", project.to_owned()),
        ("set-language", lang.to_owned()),
        ("accept-language", "en-US,en".to_owned()),
    ]
}
