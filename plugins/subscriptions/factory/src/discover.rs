//! Built-in account discovery and import from mover shapes.

use std::path::Path;

use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::json::{Map, Value, from_slice, from_str};
use hofund_plugin_sdk::oauth::{SecretObject, account_id_from};

use crate::tokens::{self, AuthKind, LAPSED};

const FACTORY_AGENT: &str = "factory";
const KEY_PREFIX: &str = "fk-";
const BUILTIN_REF_PREFIX: &str = "builtin:";

/// One built-in account found before import. Holds no secret material in `reference`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DiscoveredBuiltin {
    pub(crate) label: String,
    pub(crate) reference: String,
    pub(crate) api: bool,
    entry: BuiltinEntry,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BuiltinEntry {
    user: String,
    lapsed: bool,
    creds: BuiltinCreds,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum BuiltinCreds {
    Oauth {
        access: String,
        refresh: String,
        expires_ms: u64,
        email: Option<String>,
        uid: Option<String>,
        region: Option<String>,
        active_org: Option<String>,
        prem_base_host: Option<String>,
    },
    Api {
        key: String,
        email: Option<String>,
        uid: Option<String>,
        region: Option<String>,
        prem_base_host: Option<String>,
    },
}

/// Lists built-in Factory accounts described in `text` without storing secrets.
pub(crate) fn list(text: &str) -> Result<Vec<DiscoveredBuiltin>, PluginError> {
    let entries = parse_entries(text)?;
    Ok(entries
        .into_iter()
        .enumerate()
        .map(|(index, entry)| DiscoveredBuiltin {
            api: matches!(entry.creds, BuiltinCreds::Api { .. }),
            label: label_for(&entry),
            reference: format!("{BUILTIN_REF_PREFIX}{index}"),
            entry,
        })
        .collect())
}

/// Reads `home/logins.json` when present and lists Factory rows only.
pub(crate) fn list_in_home(home: &Path) -> Result<Vec<DiscoveredBuiltin>, PluginError> {
    let path = home.join("logins.json");
    let body = match std::fs::read_to_string(&path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(PluginError::config(format!(
                "could not read {}: {error}",
                path.display()
            )));
        }
    };
    list(&body)
}

/// Stores every built-in account in a mover or logins export.
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
    let rows = list(trimmed)?;
    if rows.is_empty() {
        return Err(PluginError::config("no Factory accounts in it"));
    }
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        out.push(store(host, &row.entry).await?);
    }
    Ok(out)
}

/// Stores one row from [`list`] or [`list_in_home`].
pub(crate) async fn import_reference(
    host: &HostContext,
    text: &str,
    reference: &str,
) -> Result<AccountId, PluginError> {
    let prefix = format!("{BUILTIN_REF_PREFIX}");
    let Some(index) = reference
        .strip_prefix(&prefix)
        .and_then(|tail| tail.parse::<usize>().ok())
    else {
        return Err(PluginError::config(format!(
            "unknown built-in account reference `{reference}`"
        )));
    };
    let rows = list(text)?;
    let Some(row) = rows.get(index) else {
        return Err(PluginError::config(format!(
            "built-in account reference `{reference}` is out of range"
        )));
    };
    store(host, &row.entry).await
}

fn recognizes(text: &str) -> bool {
    let Ok(value) = parse_value(text) else {
        return false;
    };
    match &value {
        Value::Object(map) => {
            if is_moving_auth(map) {
                return true;
            }
            if is_factory_creds(map) {
                return true;
            }
            if map.get("agent").and_then(Value::as_str) == Some(FACTORY_AGENT) {
                return true;
            }
            map.get("accounts")
                .or_else(|| map.get("logins"))
                .is_some_and(Value::is_array)
        }
        Value::Array(items) => items.iter().any(|item| {
            item.as_object().is_some_and(|map| {
                is_moving_auth(map)
                    || is_factory_creds(map)
                    || map.get("agent").and_then(Value::as_str) == Some(FACTORY_AGENT)
                    || map.get("auth").is_some()
            })
        }),
        _ => false,
    }
}

fn parse_entries(text: &str) -> Result<Vec<BuiltinEntry>, PluginError> {
    let value = parse_value(text)?;
    match value {
        Value::Object(map) if is_moving_auth(&map) => Ok(vec![entry_from_moving(&map, "")?]),
        Value::Object(map) if is_factory_creds(&map) => {
            Ok(vec![entry_from_factory_creds(&map, "")?])
        }
        Value::Object(map) => parse_login_container(&map),
        Value::Array(items) => parse_login_array(&items),
        _ => Err(PluginError::config("unrecognized Factory account export")),
    }
}

fn parse_login_container(map: &Map<String, Value>) -> Result<Vec<BuiltinEntry>, PluginError> {
    if map.get("agent").and_then(Value::as_str) == Some(FACTORY_AGENT) {
        return Ok(vec![entry_from_saved_login(map)?]);
    }
    let accounts = map
        .get("accounts")
        .or_else(|| map.get("logins"))
        .and_then(Value::as_array)
        .ok_or_else(|| PluginError::config("no Factory accounts in it"))?;
    parse_login_array(accounts)
}

fn parse_login_array(items: &[Value]) -> Result<Vec<BuiltinEntry>, PluginError> {
    let mut out = Vec::new();
    for item in items {
        let Some(map) = item.as_object() else {
            continue;
        };
        if is_moving_auth(map) {
            out.push(entry_from_moving(map, "")?);
            continue;
        }
        if is_factory_creds(map) {
            out.push(entry_from_factory_creds(map, "")?);
            continue;
        }
        if map.get("agent").and_then(Value::as_str) != Some(FACTORY_AGENT) {
            continue;
        }
        out.push(entry_from_saved_login(map)?);
    }
    if out.is_empty() {
        return Err(PluginError::config("no Factory accounts in it"));
    }
    Ok(out)
}

fn entry_from_saved_login(map: &Map<String, Value>) -> Result<BuiltinEntry, PluginError> {
    let user = text_field(map, &["user", "accountId", "email"]).unwrap_or_default();
    let lapsed = map
        .get("lapsed")
        .and_then(Value::as_str)
        .is_some_and(|reason| !reason.trim().is_empty());
    let auth = map
        .get("auth")
        .ok_or_else(|| PluginError::config("a Factory login is missing auth"))?;
    let mut entry = entry_from_auth_value(auth, &user)?;
    entry.user = if user.is_empty() {
        entry.user.clone()
    } else {
        user
    };
    entry.lapsed = lapsed;
    Ok(entry)
}

fn entry_from_auth_value(value: &Value, fallback_user: &str) -> Result<BuiltinEntry, PluginError> {
    let Some(map) = value.as_object() else {
        return Err(PluginError::config("a Factory login has unreadable auth"));
    };
    if is_moving_auth(map) {
        return entry_from_moving(map, fallback_user);
    }
    entry_from_factory_creds(map, fallback_user)
}

fn entry_from_moving(
    map: &Map<String, Value>,
    fallback_user: &str,
) -> Result<BuiltinEntry, PluginError> {
    let auth_type = text_field(map, &["type"]).unwrap_or_default();
    if auth_type == "api" {
        let key = text_field(map, &["key"])
            .ok_or_else(|| PluginError::config("a Factory API account has no key"))?;
        if !key.starts_with(KEY_PREFIX) {
            return Err(PluginError::config("a Factory API key must start with fk-"));
        }
        let md = map.get("metadata").and_then(Value::as_object);
        let email = md.and_then(|m| text_field(m, &["email"]));
        let uid = md.and_then(|m| text_field(m, &["userId", "user_id"]));
        let region = md.and_then(|m| text_field(m, &["region"]));
        let prem = md.and_then(|m| text_field(m, &["premBaseHost", "prem_base_host"]));
        let user = first_non_empty(&[
            fallback_user,
            email.as_deref().unwrap_or(""),
            uid.as_deref().unwrap_or(""),
        ]);
        return Ok(BuiltinEntry {
            user: user.to_owned(),
            lapsed: false,
            creds: BuiltinCreds::Api {
                key,
                email,
                uid,
                region,
                prem_base_host: prem,
            },
        });
    }
    if auth_type != "oauth" {
        return Err(PluginError::config(
            "a Factory mover auth has an unknown type",
        ));
    }
    let access = text_field(map, &["access", "accessToken", "access_token"])
        .ok_or_else(|| PluginError::config("a Factory OAuth account has no access token"))?;
    if access.is_empty() {
        return Err(PluginError::config(
            "a Factory OAuth account has no access token",
        ));
    }
    let refresh =
        text_field(map, &["refresh", "refreshToken", "refresh_token"]).unwrap_or_default();
    let expires_ms = number_field(map, &["expires", "expiresAt", "expires_at", "expires_ms"]);
    let email = text_field(map, &["email"]);
    let uid = text_field(map, &["userId", "user_id", "uid"]);
    let region = text_field(map, &["region"]);
    let active_org = first_map_text(
        map,
        &[
            "activeOrganizationId",
            "active_organization_id",
            "orgId",
            "org_id",
        ],
    );
    let prem = text_field(map, &["premBaseHost", "prem_base_host", "premBaseHostV2"]);
    let account_id = text_field(map, &["accountId", "account_id"]);
    let user = first_non_empty(&[
        fallback_user,
        account_id.as_deref().unwrap_or(""),
        email.as_deref().unwrap_or(""),
        uid.as_deref().unwrap_or(""),
    ]);
    Ok(BuiltinEntry {
        user: user.to_owned(),
        lapsed: false,
        creds: BuiltinCreds::Oauth {
            access,
            refresh,
            expires_ms,
            email,
            uid,
            region,
            active_org,
            prem_base_host: prem,
        },
    })
}

fn entry_from_factory_creds(
    map: &Map<String, Value>,
    fallback_user: &str,
) -> Result<BuiltinEntry, PluginError> {
    let access = text_field(map, &["accessToken", "access", "access_token"])
        .ok_or_else(|| PluginError::config("a Factory credential has no access token"))?;
    let api = map
        .get("apiKey")
        .and_then(Value::as_bool)
        .unwrap_or_else(|| access.starts_with(KEY_PREFIX));
    if api {
        if !access.starts_with(KEY_PREFIX) {
            return Err(PluginError::config("a Factory API key must start with fk-"));
        }
        let email = text_field(map, &["email"]);
        let uid = text_field(map, &["userId", "user_id", "uid"]);
        let region = text_field(map, &["region"]);
        let prem = text_field(map, &["premBaseHost", "prem_base_host", "premBaseHostV2"]);
        let user = first_non_empty(&[
            fallback_user,
            email.as_deref().unwrap_or(""),
            uid.as_deref().unwrap_or(""),
        ]);
        return Ok(BuiltinEntry {
            user: user.to_owned(),
            lapsed: false,
            creds: BuiltinCreds::Api {
                key: access,
                email,
                uid,
                region,
                prem_base_host: prem,
            },
        });
    }
    let refresh =
        text_field(map, &["refreshToken", "refresh", "refresh_token"]).unwrap_or_default();
    let expires_ms = number_field(map, &["expiresAt", "expires_at", "expires", "expires_ms"]);
    let email = text_field(map, &["email"]);
    let uid = text_field(map, &["userId", "user_id", "uid"]);
    let region = text_field(map, &["region"]);
    let active_org = first_map_text(
        map,
        &[
            "activeOrganizationId",
            "active_organization_id",
            "orgId",
            "org_id",
        ],
    );
    let prem = text_field(map, &["premBaseHost", "prem_base_host", "premBaseHostV2"]);
    let user = first_non_empty(&[
        fallback_user,
        email.as_deref().unwrap_or(""),
        uid.as_deref().unwrap_or(""),
    ]);
    Ok(BuiltinEntry {
        user: user.to_owned(),
        lapsed: false,
        creds: BuiltinCreds::Oauth {
            access,
            refresh,
            expires_ms,
            email,
            uid,
            region,
            active_org,
            prem_base_host: prem,
        },
    })
}

async fn store(host: &HostContext, entry: &BuiltinEntry) -> Result<AccountId, PluginError> {
    let (id, label, secret) = match &entry.creds {
        BuiltinCreds::Api {
            key,
            email,
            uid,
            region,
            prem_base_host,
        } => store_api(key, email, uid, region, prem_base_host, &entry.user),
        BuiltinCreds::Oauth {
            access,
            refresh,
            expires_ms,
            email,
            uid,
            region,
            active_org,
            prem_base_host,
        } => store_oauth(
            host,
            access,
            refresh,
            *expires_ms,
            email,
            uid,
            region,
            active_org,
            prem_base_host,
            &entry.user,
        ),
    }?;
    let mut blob = secret;
    if entry.lapsed {
        blob = blob.with_flag(LAPSED, true);
    }
    let account = host.accounts().account(id.clone(), label)?;
    host.accounts().upsert(account, blob.to_secret()).await?;
    Ok(id)
}

fn store_api(
    key: &str,
    email: &Option<String>,
    uid: &Option<String>,
    region: &Option<String>,
    prem_base_host: &Option<String>,
    fallback_user: &str,
) -> Result<(AccountId, String, SecretObject), PluginError> {
    let account_key = first_non_empty(&[
        email.as_deref().unwrap_or(""),
        uid.as_deref().unwrap_or(""),
        fallback_user,
    ]);
    let label = if let Some(addr) = email {
        addr.clone()
    } else if let Some(user) = uid {
        user.clone()
    } else if fallback_user.is_empty() {
        "Factory API key".to_owned()
    } else {
        fallback_user.to_owned()
    };
    let id = account_id_from(&["factory", "api", &account_key.to_ascii_lowercase()]);
    let mut blob = SecretObject::new()
        .with_number("v", 1)
        .with_text("auth_type", AuthKind::Api.as_str())
        .with_secret("api_key", &SecretString::from(key.to_owned()))
        .with_text("account_id", account_key);
    if let Some(addr) = email {
        blob = blob.with_text("email", addr);
    }
    if let Some(user) = uid {
        blob = blob.with_text("uid", user);
    }
    if let Some(reg) = region {
        blob = blob.with_text("region", reg);
    }
    if let Some(host) = prem_base_host {
        blob = blob.with_text("prem_base_host", host);
    }
    Ok((id, label, blob))
}

fn store_oauth(
    host: &HostContext,
    access: &str,
    refresh: &str,
    expires_ms: u64,
    email: &Option<String>,
    uid: &Option<String>,
    region: &Option<String>,
    active_org: &Option<String>,
    prem_base_host: &Option<String>,
    fallback_user: &str,
) -> Result<(AccountId, String, SecretObject), PluginError> {
    let account_key = first_non_empty(&[
        email.as_deref().unwrap_or(""),
        uid.as_deref().unwrap_or(""),
        fallback_user,
    ]);
    let label = if let Some(addr) = email {
        addr.clone()
    } else if let Some(user) = uid {
        user.clone()
    } else if fallback_user.is_empty() {
        "Factory".to_owned()
    } else {
        fallback_user.to_owned()
    };
    let id = account_id_from(&["factory", &account_key.to_ascii_lowercase()]);
    let access_secret = SecretString::from(access.to_owned());
    let refresh_secret = SecretString::from(refresh.to_owned());
    let expires = if expires_ms > 0 {
        expires_ms
    } else {
        tokens::expires_ms_from_access(&access_secret, host.clock().now())
    };
    let mut blob = SecretObject::new()
        .with_number("v", 1)
        .with_text("auth_type", AuthKind::Oauth.as_str())
        .with_secret("access_token", &access_secret)
        .with_secret("refresh_token", &refresh_secret)
        .with_number("expires_ms", expires)
        .with_text("account_id", account_key);
    if let Some(addr) = email {
        blob = blob.with_text("email", addr);
    }
    if let Some(user) = uid {
        blob = blob.with_text("uid", user);
    }
    if let Some(reg) = region {
        blob = blob.with_text("region", reg);
    }
    if let Some(host_name) = prem_base_host {
        blob = blob.with_text("prem_base_host", host_name);
    }
    if let Some(org) = active_org {
        blob = blob.with_text("active_organization_id", org);
    }
    Ok((id, label, blob))
}

fn label_for(entry: &BuiltinEntry) -> String {
    match &entry.creds {
        BuiltinCreds::Api { email, uid, .. } => email
            .clone()
            .or(uid.clone())
            .unwrap_or_else(|| entry.user.clone()),
        BuiltinCreds::Oauth { email, uid, .. } => email
            .clone()
            .or(uid.clone())
            .unwrap_or_else(|| entry.user.clone()),
    }
}

fn is_moving_auth(map: &Map<String, Value>) -> bool {
    map.get("type")
        .and_then(Value::as_str)
        .is_some_and(|text| text == "api" || text == "oauth")
}

fn is_factory_creds(map: &Map<String, Value>) -> bool {
    map.contains_key("accessToken")
        || (map.contains_key("access") && !map.contains_key("type"))
        || map.get("apiKey").and_then(Value::as_bool) == Some(true)
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

fn number_field(map: &Map<String, Value>, keys: &[&str]) -> u64 {
    keys.iter()
        .find_map(|key| map.get(*key))
        .and_then(|value| match value {
            Value::Number(number) => number.as_u64().or_else(|| {
                number
                    .as_f64()
                    .filter(|n| n.is_finite() && *n >= 0.0)
                    .map(|n| n as u64)
            }),
            Value::String(text) => text.trim().parse().ok(),
            _ => None,
        })
        .unwrap_or(0)
}

fn first_map_text(map: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    text_field(map, keys)
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

#[cfg(test)]
mod tests {
    use super::*;
    use hofund_kit_subscription::{TestHost, block_on};
    use hofund_plugin_api::{PluginId, PluginKind};
    use hofund_plugin_sdk::json::json;
    use hofund_plugin_sdk::oauth::{SecretObject, secret_text};

    fn host(dir: &tempfile::TempDir) -> TestHost {
        TestHost::open_for(
            PluginKind::Subscription,
            PluginId::from_static("factory"),
            dir.path(),
        )
    }

    #[test]
    fn r52d_discover_lists_oauth_and_api_without_secrets() {
        let oauth = json!({
            "type": "oauth",
            "access": "oauth-access",
            "refresh": "oauth-refresh",
            "expires": 1_900_000_000_000_u64,
            "email": "a@f.com",
            "userId": "user_1",
            "activeOrganizationId": "fo1",
            "region": "eu",
            "premBaseHost": "llm.example.com",
            "accountId": "a@f.com"
        });
        let api = json!({
            "type": "api",
            "key": "fk-key_0123456789abcdef",
            "metadata": {
                "email": "k@f.com",
                "userId": "user_k",
                "region": "eu",
                "premBaseHost": "llm.example.com"
            }
        });
        let text = format!("[{oauth},{api}]");
        let rows = list(&text).expect("discover");
        assert_eq!(rows.len(), 2);
        assert!(!rows[0].api);
        assert!(rows[1].api);
        assert_eq!(rows[0].label, "a@f.com");
        assert_eq!(rows[1].label, "k@f.com");
        assert_eq!(rows[0].reference, "builtin:0");
        assert_eq!(rows[1].reference, "builtin:1");
    }

    #[test]
    fn r52d_import_moving_shapes_and_logins_fixture() {
        let dir = tempfile::tempdir().expect("dir");
        let host = host(&dir);
        let logins = json!([
            {
                "agent": "factory",
                "user": "user_2",
                "auth": {
                    "accessToken": "a2",
                    "refreshToken": "r2",
                    "expiresAt": 1_900_000_000_001_i64,
                    "userId": "user_2"
                }
            },
            {
                "agent": "factory",
                "user": "a@f.com",
                "auth": {
                    "accessToken": "a1",
                    "refreshToken": "r1",
                    "expiresAt": 1_900_000_000_000_i64,
                    "orgId": "org_1",
                    "activeOrganizationId": "fo1",
                    "email": "a@f.com",
                    "userId": "user_1",
                    "region": "eu",
                    "premBaseHost": "llm.example.com"
                }
            },
            {
                "agent": "factory",
                "user": "k@f.com",
                "auth": {
                    "accessToken": "fk-key_0123456789abcdef",
                    "apiKey": true,
                    "email": "k@f.com",
                    "userId": "user_k",
                    "region": "eu",
                    "premBaseHost": "llm.example.com"
                }
            }
        ]);
        let ids = block_on(import_accounts(host.context(), &logins.to_string())).expect("import");
        assert_eq!(ids.len(), 3);
        let oauth = block_on(host.context().accounts().secret(&ids[1]))
            .expect("read")
            .expect("stored");
        let blob = SecretObject::parse(&oauth).expect("blob");
        assert_eq!(blob.text("auth_type"), Some("oauth"));
        assert_eq!(blob.text("email"), Some("a@f.com"));
        assert_eq!(blob.text("region"), Some("eu"));
        assert_eq!(blob.text("prem_base_host"), Some("llm.example.com"));
        assert_eq!(blob.text("active_organization_id"), Some("fo1"));
        assert_eq!(
            blob.secret("refresh_token").as_ref().map(secret_text),
            Some("r1")
        );
        let key = block_on(host.context().accounts().secret(&ids[2]))
            .expect("read")
            .expect("stored");
        let key_blob = SecretObject::parse(&key).expect("blob");
        assert_eq!(key_blob.text("auth_type"), Some("api"));
        assert_eq!(
            key_blob.secret("api_key").as_ref().map(secret_text),
            Some("fk-key_0123456789abcdef")
        );
        assert!(host.http().calls().is_empty());
    }

    #[test]
    fn r52d_discover_logins_json_in_home() {
        let dir = tempfile::tempdir().expect("dir");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).expect("mkdir");
        std::fs::write(
            home.join("logins.json"),
            r#"[{"agent":"codex","user":"x"},{"agent":"factory","user":"u","auth":{"accessToken":"a","refreshToken":"r","email":"u@factory.test"}}]"#,
        )
        .expect("write");
        let rows = list_in_home(&home).expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "u@factory.test");
    }

    #[test]
    fn r52d_import_reference_reads_one_row() {
        let dir = tempfile::tempdir().expect("dir");
        let host = host(&dir);
        let text = json!([
            {"type": "oauth", "access": "a", "refresh": "r", "email": "one@factory.test"},
            {"type": "api", "key": "fk-other12345678", "metadata": {"email": "two@factory.test"}}
        ])
        .to_string();
        let id = block_on(import_reference(host.context(), &text, "builtin:1")).expect("import");
        let secret = block_on(host.context().accounts().secret(&id))
            .expect("read")
            .expect("stored");
        let blob = SecretObject::parse(&secret).expect("blob");
        assert_eq!(blob.text("auth_type"), Some("api"));
        assert_eq!(blob.text("email"), Some("two@factory.test"));
    }
}
