//! Import Zed credential JSON, built-in side logins, and mover OAuth shapes.

use hofund_plugin_api::{AccountId, HostContext, PluginError};
use hofund_plugin_sdk::json::{Map, Value, from_slice, from_str};

use crate::creds::{self, Credential};
use crate::models;

const ZED_AGENT: &str = "zed";

pub(crate) async fn import_accounts(
    host: &HostContext,
    text: &str,
) -> Result<Vec<AccountId>, PluginError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(PluginError::config("nothing to import"));
    }
    if !recognizes(trimmed) {
        return Err(PluginError::unsupported(
            "not an account export this subscription reads",
        ));
    }
    let credentials = parse_credentials(trimmed)?;
    if credentials.is_empty() {
        return Err(PluginError::config("no Zed accounts in it"));
    }
    let mut out = Vec::with_capacity(credentials.len());
    for credential in credentials {
        out.push(store(&credential, host).await?);
    }
    Ok(out)
}

pub(crate) async fn store(
    credential: &Credential,
    host: &HostContext,
) -> Result<AccountId, PluginError> {
    let id = creds::account_id(credential);
    let label = creds::label(credential);
    let account = host.accounts().account(id.clone(), label)?;
    host.accounts()
        .upsert(account, creds::secret_blob(credential).to_secret())
        .await?;
    models::invalidate_vendor_cache(&credential.user_id);
    Ok(id)
}

fn recognizes(text: &str) -> bool {
    let Ok(value) = parse_value(text) else {
        return false;
    };
    match &value {
        Value::Object(map) => {
            if is_mover_auth(map) || is_zed_creds(map) {
                return true;
            }
            if map.get("agent").and_then(Value::as_str) == Some(ZED_AGENT) {
                return true;
            }
            map.get("accounts")
                .or_else(|| map.get("logins"))
                .is_some_and(Value::is_array)
        }
        Value::Array(items) => items.iter().any(|item| {
            item.as_object().is_some_and(|map| {
                is_mover_auth(map)
                    || is_zed_creds(map)
                    || map.get("agent").and_then(Value::as_str) == Some(ZED_AGENT)
                    || map.get("auth").is_some()
            })
        }),
        _ => false,
    }
}

fn parse_credentials(text: &str) -> Result<Vec<Credential>, PluginError> {
    let value = parse_value(text)?;
    match value {
        Value::Object(map) if is_mover_auth(&map) => Ok(vec![credential_from_mover(&map, "")?]),
        Value::Object(map) if is_zed_creds(&map) => {
            Ok(vec![creds::credential_from_zed_creds_map(&map)?])
        }
        Value::Object(map) if map.get("agent").and_then(Value::as_str) == Some(ZED_AGENT) => {
            Ok(parse_saved_login(&map)?)
        }
        Value::Object(map) => parse_login_container(&map),
        Value::Array(items) => parse_login_array(&items),
        _ => Err(PluginError::config("unrecognized Zed account export")),
    }
}

fn parse_login_container(map: &Map<String, Value>) -> Result<Vec<Credential>, PluginError> {
    if map.get("agent").and_then(Value::as_str) == Some(ZED_AGENT) {
        return parse_saved_login(map);
    }
    let accounts = map
        .get("accounts")
        .or_else(|| map.get("logins"))
        .and_then(Value::as_array)
        .ok_or_else(|| PluginError::config("no Zed accounts in it"))?;
    parse_login_array(accounts)
}

fn parse_login_array(items: &[Value]) -> Result<Vec<Credential>, PluginError> {
    let mut out = Vec::new();
    for item in items {
        let Some(map) = item.as_object() else {
            continue;
        };
        if is_mover_auth(map) {
            if let Ok(credential) = credential_from_mover(map, "") {
                out.push(credential);
            }
            continue;
        }
        if is_zed_creds(map) {
            if let Ok(credential) = creds::credential_from_zed_creds_map(map) {
                out.push(credential);
            }
            continue;
        }
        if map.get("agent").and_then(Value::as_str) != Some(ZED_AGENT) {
            continue;
        }
        if let Ok(rows) = parse_saved_login(map) {
            out.extend(rows);
        }
    }
    Ok(out)
}

fn parse_saved_login(map: &Map<String, Value>) -> Result<Vec<Credential>, PluginError> {
    if map
        .get("lapsed")
        .and_then(Value::as_str)
        .is_some_and(|reason| !reason.trim().is_empty())
    {
        return Ok(Vec::new());
    }
    let user = text_field(map, &["user", "accountId", "login"]).unwrap_or_default();
    let auth = map
        .get("auth")
        .ok_or_else(|| PluginError::config("a Zed login is missing auth"))?;
    let credential = credential_from_auth_value(auth, &user)?;
    Ok(vec![credential])
}

fn credential_from_auth_value(
    value: &Value,
    fallback_user: &str,
) -> Result<Credential, PluginError> {
    let Some(map) = value.as_object() else {
        return Err(PluginError::config("a Zed login has unreadable auth"));
    };
    if is_mover_auth(map) {
        return credential_from_mover(map, fallback_user);
    }
    let mut credential = creds::credential_from_zed_creds_map(map)?;
    if credential.login.is_empty() {
        let login = text_field(map, &["login"]).unwrap_or_default();
        if !login.is_empty() {
            credential.login = login;
        } else if !fallback_user.is_empty() {
            credential.login = fallback_user.to_owned();
        }
    }
    if credential.name.is_empty() {
        credential.name = optional_field(map, &["name"]);
    }
    Ok(credential)
}

fn credential_from_mover(
    map: &Map<String, Value>,
    fallback_user: &str,
) -> Result<Credential, PluginError> {
    let auth_type = text_field(map, &["type"]).unwrap_or_default();
    if auth_type != "oauth" {
        return Err(PluginError::config("a Zed mover auth has an unknown type"));
    }
    let access = text_field(map, &["access", "accessToken", "access_token"])
        .ok_or_else(|| PluginError::config("a Zed OAuth account has no access token"))?;
    if access.is_empty() {
        return Err(PluginError::config(
            "a Zed OAuth account has no access token",
        ));
    }
    let refresh = map.get("refresh").ok_or_else(|| {
        PluginError::config("a Zed OAuth account has unreadable refresh metadata")
    })?;
    let refresh_map = refresh_map(refresh)?;
    let user_id = json_user_id(
        refresh_map
            .get("userId")
            .or_else(|| refresh_map.get("user_id")),
    )
    .ok_or_else(|| PluginError::config("a Zed OAuth account has no user id"))?;
    let mut models = refresh_map
        .get("models")
        .cloned()
        .unwrap_or(Value::Array(Vec::new()));
    let account_id = text_field(map, &["accountId", "account_id"]).unwrap_or_default();
    let refresh_login = optional_field(&refresh_map, &["login"]);
    let login = first_non_empty(&[refresh_login.as_str(), account_id.as_str(), fallback_user]);
    let name = optional_field(&refresh_map, &["name"]);
    let plan = optional_field(&refresh_map, &["plan"]);
    if models == Value::Null {
        models = Value::Array(Vec::new());
    }
    creds::assemble(creds::CredentialParts {
        user_id,
        access,
        system_id: optional_field(&refresh_map, &["systemId", "system_id"]),
        org: optional_field(&refresh_map, &["org", "organizationId", "organization_id"]),
        login: login.to_owned(),
        name,
        plan,
        models,
    })
}

fn refresh_map(value: &Value) -> Result<Map<String, Value>, PluginError> {
    match value {
        Value::Object(map) => Ok(map.clone()),
        Value::String(text) => from_str(text.trim()).map_err(|_| {
            PluginError::config("a Zed OAuth account has unreadable refresh metadata")
        }),
        _ => Err(PluginError::config(
            "a Zed OAuth account has unreadable refresh metadata",
        )),
    }
}

fn json_user_id(value: Option<&Value>) -> Option<String> {
    let value = value?;
    match value {
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_owned())
            }
        }
        Value::Number(number) => number
            .as_u64()
            .map(|n| n.to_string())
            .or_else(|| number.as_i64().map(|n| n.to_string())),
        _ => None,
    }
}

fn is_mover_auth(map: &Map<String, Value>) -> bool {
    map.get("type").and_then(Value::as_str) == Some("oauth")
        && map.get("access").is_some()
        && map.get("refresh").is_some()
}

fn is_zed_creds(map: &Map<String, Value>) -> bool {
    if map.get("type").and_then(Value::as_str) == Some("oauth") {
        return false;
    }
    let user = text_field(map, &["userId", "user_id"]);
    let access = text_field(map, &["accessToken", "access", "access_token"]);
    matches!((user, access), (Some(_), Some(_)))
}

fn parse_value(text: &str) -> Result<Value, PluginError> {
    let trimmed = text.strip_prefix('\u{feff}').unwrap_or(text).trim();
    if trimmed.starts_with(['{', '[']) {
        from_str(trimmed).map_err(|error| PluginError::decode(error.to_string()))
    } else {
        from_slice(trimmed.as_bytes()).map_err(|error| PluginError::decode(error.to_string()))
    }
}

fn text_field(map: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| map.get(*key))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn optional_field(map: &Map<String, Value>, keys: &[&str]) -> String {
    text_field(map, keys).unwrap_or_default()
}

fn first_non_empty<'a>(parts: &[&'a str]) -> &'a str {
    for part in parts {
        let trimmed = part.trim();
        if !trimmed.is_empty() {
            return trimmed;
        }
    }
    ""
}
