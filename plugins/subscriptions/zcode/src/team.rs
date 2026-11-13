//! Team plan project API key resolution.

use std::path::Path;

use hofund_plugin_api::{HostContext, PluginError, SecretString};
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::oauth::secret_text;

use crate::Endpoints;
use crate::api;
use crate::creds::{PlanAuth, Site};

pub(crate) async fn resolve_project_key(
    host: &HostContext,
    endpoints: &Endpoints,
    auth: &PlanAuth,
    device_file: &Path,
) -> Result<SecretString, PluginError> {
    resolve(host, endpoints, auth, device_file).await
}

pub(crate) async fn resolve(
    host: &HostContext,
    endpoints: &Endpoints,
    auth: &PlanAuth,
    device_file: &Path,
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
    let auth_header = business_authorization(auth, token);
    let data = api::get_envelope(&api::EnvelopeRequest::get(
        host,
        endpoints,
        &list_url,
        Some(&auth_header),
        &header_refs,
        device_file,
    ))
    .await?;
    let key_id = match find_team_key_id(&data) {
        Some(id) => id,
        None => {
            let body = hofund_plugin_sdk::json::json!({
                "name": "zcode-team-api-key",
                "keyType": 2,
            });
            let request = api::EnvelopeRequest {
                method: "POST",
                body: Some(&body),
                ..api::EnvelopeRequest::get(
                    host,
                    endpoints,
                    &list_url,
                    Some(&auth_header),
                    &header_refs,
                    device_file,
                )
            };
            let created = api::get_envelope(&request).await?;
            created
                .get("apiKey")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| PluginError::decode("Z Code team key creation returned no key id"))?
                .to_owned()
        }
    };
    let copy_url = format!(
        "{biz}/api/biz/v1/organization/{}/projects/{}/api_keys/copy/{key_id}",
        auth.org, auth.project
    );
    let secret = api::get_envelope(&api::EnvelopeRequest::get(
        host,
        endpoints,
        &copy_url,
        Some(&auth_header),
        &header_refs,
        device_file,
    ))
    .await?;
    let text = secret
        .get("secret")
        .or_else(|| secret.get("apiKeySecret"))
        .or_else(|| secret.get("secretKey"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if text.is_empty() {
        return Ok(SecretString::from(key_id));
    }
    if text.contains('.') {
        Ok(SecretString::from(text.to_owned()))
    } else {
        Ok(SecretString::from(format!("{key_id}.{text}")))
    }
}

pub(crate) fn business_authorization(auth: &PlanAuth, token: &SecretString) -> SecretString {
    let raw = secret_text(token);
    if auth.site == Site::Zai && !raw.starts_with("Bearer ") {
        SecretString::from(format!("Bearer {raw}"))
    } else {
        token.clone()
    }
}

fn find_team_key_id(data: &Value) -> Option<String> {
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
        if object.get("name").and_then(Value::as_str) == Some("zcode-team-api-key")
            && let Some(id) = object
                .get("apiKey")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
        {
            return Some(id.to_owned());
        }
    }
    None
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

#[cfg(test)]
mod tests {
    use super::{business_authorization, team_headers};
    use crate::creds::{PlanAuth, Site};
    use crate::{BIGMODEL_ANTHROPIC_BASE, ZAI_ANTHROPIC_BASE};
    use hofund_plugin_api::SecretString;
    use hofund_plugin_sdk::oauth::secret_text;

    #[test]
    fn a2d_14_business_authorization_bearer_on_zai_only() {
        let token = SecretString::from("biz-token".to_owned());
        let zai = PlanAuth {
            api_key: None,
            jwt: None,
            team_token: Some(token.clone()),
            org: "o".to_owned(),
            project: "p".to_owned(),
            base: ZAI_ANTHROPIC_BASE.to_owned(),
            site: Site::Zai,
            uid: String::new(),
            label: String::new(),
            follow_agent: false,
            agent_credentials: None,
        };
        let bigmodel = PlanAuth {
            site: Site::Bigmodel,
            base: BIGMODEL_ANTHROPIC_BASE.to_owned(),
            team_token: Some(token.clone()),
            org: zai.org.clone(),
            project: zai.project.clone(),
            api_key: None,
            jwt: None,
            uid: String::new(),
            label: String::new(),
            follow_agent: false,
            agent_credentials: None,
        };
        assert_eq!(
            secret_text(&business_authorization(&zai, &token.clone())),
            "Bearer biz-token"
        );
        assert_eq!(
            secret_text(&business_authorization(&bigmodel, &token)),
            "biz-token"
        );
        let headers = team_headers(ZAI_ANTHROPIC_BASE, "org", "proj");
        assert!(
            headers
                .iter()
                .any(|(name, _)| *name == "bigmodel-organization")
        );
    }
}
