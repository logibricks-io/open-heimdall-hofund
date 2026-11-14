//! Zed account credentials.

use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::json::{self, Value};
use hofund_plugin_sdk::oauth::{SecretObject, account_id_from, secret_text};

#[derive(Clone, Debug)]
pub(crate) struct Credential {
    pub user_id: String,
    pub access: SecretString,
    pub system_id: String,
    pub org: String,
    pub login: String,
    pub name: String,
    pub plan: String,
    pub models_json: String,
}

#[derive(Clone, Debug)]
pub(crate) struct Stored {
    pub credential: Credential,
}

impl Stored {
    pub(crate) async fn load(host: &HostContext, account: &AccountId) -> Result<Self, PluginError> {
        let Some(secret) = host.accounts().secret(account).await? else {
            return Err(PluginError::auth(
                "no Zed sign-in is stored for this account",
            ));
        };
        parse_secret(&secret)
    }
}

pub(crate) fn parse_secret(secret: &SecretString) -> Result<Stored, PluginError> {
    let blob = SecretObject::parse(secret)?;
    let user_id = blob
        .text("user_id")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| PluginError::auth("the stored Zed sign-in has no user id"))?
        .to_owned();
    let access = blob
        .secret("access_token")
        .ok_or_else(|| PluginError::auth("the stored Zed sign-in has no access token"))?;
    Ok(Stored {
        credential: Credential {
            user_id,
            access,
            system_id: blob.text("system_id").unwrap_or("").to_owned(),
            org: blob.text("org").unwrap_or("").to_owned(),
            login: blob.text("login").unwrap_or("").to_owned(),
            name: blob.text("name").unwrap_or("").to_owned(),
            plan: blob.text("plan").unwrap_or("").to_owned(),
            models_json: blob.text("models_json").unwrap_or("[]").to_owned(),
        },
    })
}

pub(crate) fn secret_blob(credential: &Credential) -> SecretObject {
    let refresh = json::json!({
        "userId": credential.user_id,
        "systemId": credential.system_id,
        "org": credential.org,
        "plan": credential.plan,
        "login": credential.login,
        "name": credential.name,
        "models": json::from_str::<Value>(&credential.models_json).unwrap_or(Value::Null),
    })
    .to_string();
    SecretObject::new()
        .with_number("v", 1)
        .with_text("user_id", &credential.user_id)
        .with_secret("access_token", &credential.access)
        .with_text("system_id", &credential.system_id)
        .with_text("org", &credential.org)
        .with_text("login", &credential.login)
        .with_text("name", &credential.name)
        .with_text("plan", &credential.plan)
        .with_text("models_json", &credential.models_json)
        .with_text("refresh_json", &refresh)
}

pub(crate) fn account_id(credential: &Credential) -> AccountId {
    account_id_from(&["zed", &credential.user_id])
}

pub(crate) fn label(credential: &Credential) -> String {
    if !credential.login.is_empty() {
        credential.login.clone()
    } else if !credential.name.is_empty() {
        credential.name.clone()
    } else {
        credential.user_id.clone()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CredentialParts {
    pub user_id: String,
    pub access: String,
    pub system_id: String,
    pub org: String,
    pub login: String,
    pub name: String,
    pub plan: String,
    pub models: Value,
}

pub(crate) fn assemble(parts: CredentialParts) -> Result<Credential, PluginError> {
    if parts.user_id.is_empty() {
        return Err(PluginError::auth("credential JSON has no userId"));
    }
    if parts.access.is_empty() {
        return Err(PluginError::auth("credential JSON has no access token"));
    }
    Ok(Credential {
        user_id: parts.user_id,
        access: SecretString::from(parts.access),
        system_id: parts.system_id,
        org: parts.org,
        login: parts.login,
        name: parts.name,
        plan: parts.plan,
        models_json: parts.models.to_string(),
    })
}

pub(crate) fn credential_from_zed_creds_map(
    map: &json::Map<String, Value>,
) -> Result<Credential, PluginError> {
    let user_id = field(map, "userId").or_else(|_| field(map, "user_id"))?;
    let access = field(map, "accessToken").or_else(|_| field(map, "access"))?;
    let models = map.get("models").cloned().unwrap_or(Value::Array(vec![]));
    let org = optional(map, "organizationId");
    let org = if org.is_empty() {
        optional(map, "org")
    } else {
        org
    };
    assemble(CredentialParts {
        user_id,
        access,
        system_id: optional(map, "systemId"),
        org,
        login: optional(map, "login"),
        name: optional(map, "name"),
        plan: optional(map, "plan"),
        models,
    })
}

fn field(map: &json::Map<String, Value>, key: &str) -> Result<String, PluginError> {
    map.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| PluginError::auth(format!("credential JSON has no {key}")))
}

fn optional(map: &json::Map<String, Value>, key: &str) -> String {
    map.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}
pub(crate) async fn persist(
    host: &HostContext,
    account: &AccountId,
    credential: &Credential,
) -> Result<(), PluginError> {
    let label = label(credential);
    let account = host.accounts().account(account.clone(), label)?;
    host.accounts()
        .upsert(account, secret_blob(credential).to_secret())
        .await
}

pub(crate) fn auth_header(credential: &Credential) -> String {
    format!("{} {}", credential.user_id, secret_text(&credential.access))
}
