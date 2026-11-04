//! Zed account credentials.

use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::json::{self, Value};
use hofund_plugin_sdk::oauth::{SecretObject, account_id_from, secret_text};

pub(crate) const CREDENTIAL_FILE: &str = "credential.json";

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
    let who = label(credential);
    account_id_from(&["zed", &who])
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

pub(crate) fn read_file(path: &std::path::Path) -> Result<Credential, PluginError> {
    let text = std::fs::read_to_string(path).map_err(|error| {
        PluginError::auth(format!("{} can't be read ({error})", path.display()))
    })?;
    parse_json(&text)
}

pub(crate) fn parse_json(text: &str) -> Result<Credential, PluginError> {
    let root: Value =
        json::from_str(text.trim()).map_err(|_| PluginError::auth("credential JSON is invalid"))?;
    let map = root
        .as_object()
        .ok_or_else(|| PluginError::auth("credential JSON must be an object"))?;
    let user_id = field(map, "userId").or_else(|_| field(map, "user_id"))?;
    let access = field(map, "accessToken").or_else(|_| field(map, "access"))?;
    let models = map.get("models").cloned().unwrap_or(Value::Array(vec![]));
    Ok(Credential {
        user_id,
        access: SecretString::from(access),
        system_id: optional(map, "systemId"),
        org: optional(map, "organizationId"),
        login: optional(map, "login"),
        name: optional(map, "name"),
        plan: optional(map, "plan"),
        models_json: models.to_string(),
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

pub(crate) fn credential_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join(CREDENTIAL_FILE)
}

pub(crate) fn auth_header(credential: &Credential) -> String {
    format!("{} {}", credential.user_id, secret_text(&credential.access))
}
