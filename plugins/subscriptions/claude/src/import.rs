//! Importing Claude accounts from external export files.

use hofund_plugin_api::{AccountId, HostContext, PluginError};
use hofund_plugin_sdk::json::{Map, Value};

/// One account parsed from an export file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportedLogin {
    /// Account email.
    pub email: String,
    /// Credentials file text.
    pub credentials: String,
    /// Profile text.
    pub profile: String,
    /// Optional alias from the export tool.
    pub alias: Option<String>,
}

/// Parses a Claude export file. Supports the neutral swap-export envelope
/// from the reference fixture shape (commit 64cdb6803).
///
/// # Errors
///
/// Returns [`PluginError`] when the file is unreadable or encrypted.
pub fn parse_export(data: &str) -> Result<Vec<ImportedLogin>, PluginError> {
    let trimmed = data.trim();
    if trimmed.is_empty() {
        return Err(PluginError::config("nothing to import"));
    }
    let value: Value = hofund_plugin_sdk::json::from_str(trimmed)
        .map_err(|error| PluginError::config(format!("not valid JSON: {error}")))?;
    let Some(root) = value.as_object() else {
        return Err(PluginError::config("expected a JSON object"));
    };
    if root.get("swapVersion").is_some() {
        if root.get("encrypted").and_then(Value::as_bool) == Some(true) {
            return Err(PluginError::config(
                "an encrypted swap export: decrypt it first, then import what that gives",
            ));
        }
        let version = root.get("version").and_then(Value::as_f64);
        if version != Some(1.0) {
            return Err(PluginError::config(
                "a swap export of an unsupported format: only format 1 is read",
            ));
        }
    }
    let Some(accounts) = root.get("accounts").and_then(Value::as_array) else {
        return Err(PluginError::config("the export has no accounts array"));
    };
    let mut out = Vec::new();
    for entry in accounts {
        out.push(parse_swap_account(entry)?);
    }
    Ok(out)
}

fn parse_swap_account(entry: &Value) -> Result<ImportedLogin, PluginError> {
    let Some(object) = entry.as_object() else {
        return Err(PluginError::config("an account entry is not an object"));
    };
    let email = object
        .get("email")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| PluginError::config("an account has no email"))?;
    let credentials = object
        .get("credentials")
        .and_then(Value::as_object)
        .and_then(|credentials| credentials.get("claudeAiOauth"))
        .ok_or_else(|| PluginError::config("an account has no credentials"))?;
    let mut cred_root = hofund_plugin_sdk::json::Map::new();
    cred_root.insert("claudeAiOauth".to_owned(), credentials.clone());
    let credentials_text = Value::Object(cred_root).to_string();
    let profile = object
        .get("config")
        .and_then(|config| config.as_object())
        .and_then(|config| config.get("oauthAccount"))
        .map(std::string::ToString::to_string)
        .unwrap_or_default();
    let alias = object
        .get("alias")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    Ok(ImportedLogin {
        email: email.to_owned(),
        credentials: credentials_text,
        profile,
        alias,
    })
}

/// Stores every account in a supported import file.
///
/// # Errors
///
/// Returns [`PluginError`] when the file is recognized and cannot be stored.
/// A file that is not recognized is [`PluginErrorKind::Unsupported`](hofund_plugin_api::PluginErrorKind).
pub async fn store(host: &HostContext, text: &str) -> Result<Vec<AccountId>, PluginError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(PluginError::config("nothing to import"));
    }
    let Ok(value) = hofund_plugin_sdk::json::from_str::<Value>(trimmed) else {
        return Err(PluginError::unsupported(
            "not an account export this subscription reads",
        ));
    };
    let is_swap = value
        .as_object()
        .is_some_and(|root| root.get("swapVersion").is_some());
    if is_swap {
        return store_swap(host, trimmed).await;
    }
    if let Some(row) = parse_login_object(&value)? {
        return Ok(vec![store_login(host, row).await?]);
    }
    Err(PluginError::unsupported(
        "not an account export this subscription reads",
    ))
}

async fn store_swap(host: &HostContext, text: &str) -> Result<Vec<AccountId>, PluginError> {
    let rows = parse_export(text)?;
    let mut ids = Vec::new();
    for row in rows {
        let profile = crate::creds::Profile::parse(&row.profile);
        let credentials = crate::creds::parse_credentials(&row.credentials)
            .ok_or_else(|| PluginError::config("an imported account has no credentials"))?;
        let id = profile.account_id()?;
        if let Some(secret) = host.accounts().secret(&id).await? {
            let blob = hofund_plugin_sdk::oauth::SecretObject::parse(&secret)?;
            let same_refresh = blob
                .text(crate::creds::CREDENTIALS)
                .and_then(crate::creds::refresh_token)
                .is_some_and(|stored| {
                    crate::creds::refresh_token(&credentials.text)
                        .is_some_and(|incoming| incoming == stored)
                });
            if same_refresh {
                ids.push(id);
                continue;
            }
            if crate::agent_sign_in::import_blocked(host, &id, &credentials.text, &profile).await? {
                return Err(PluginError::config(crate::agent_sign_in::IMPORT_EXISTS));
            }
        }
        let blob = crate::creds::blob(&credentials, &profile);
        let account = host.accounts().account(id.clone(), row.email)?;
        host.accounts().upsert(account, blob.to_secret()).await?;
        ids.push(id);
    }
    Ok(ids)
}

const IMPORT_SCOPES: [&str; 4] = [
    "user:inference",
    "user:profile",
    "user:sessions:claude_code",
    "user:mcp_servers",
];

struct LoginRow {
    email: String,
    credentials: String,
    profile: String,
}

fn parse_login_object(value: &Value) -> Result<Option<LoginRow>, PluginError> {
    let Some(object) = value.as_object() else {
        return Ok(None);
    };
    if object.contains_key("claudeAiOauth") {
        return parse_credentials_root(object);
    }
    let typ = object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if typ == "codex" {
        return Err(PluginError::config("a ChatGPT sign-in, not Claude's"));
    }
    if typ != "claude" && !typ.is_empty() {
        return Ok(None);
    }
    let email = object
        .get("email")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| PluginError::config("the file doesn't say which Claude account this is; sign in to it from hofund instead"))?;
    let src = object
        .get("claudeAiOauth")
        .and_then(Value::as_object)
        .map(|_| object)
        .unwrap_or(object);
    let access = json_str(src, &["access_token", "accessToken"]);
    let refresh = json_str(src, &["refresh_token", "refreshToken"]);
    if refresh.is_empty() && !access.is_empty() {
        return Err(PluginError::config(
            "only an access token, no refresh_token: it would stop working within days and can't be renewed; sign in to this account instead",
        ));
    }
    if refresh.is_empty() {
        return Err(PluginError::config("no refresh_token in it"));
    }
    let expires = src
        .get("expiresAt")
        .or_else(|| src.get("expires_at"))
        .and_then(Value::as_i64)
        .unwrap_or(1);
    let cred = format!(
        r#"{{"claudeAiOauth":{{"accessToken":"{access}","refreshToken":"{refresh}","expiresAt":{expires},"scopes":{scopes}}}}}"#,
        scopes = serde_scopes()
    );
    let profile = format!(r#"{{"emailAddress":"{email}"}}"#);
    Ok(Some(LoginRow {
        email: email.to_owned(),
        credentials: cred,
        profile,
    }))
}

fn parse_credentials_root(object: &Map<String, Value>) -> Result<Option<LoginRow>, PluginError> {
    let refresh = object
        .get("claudeAiOauth")
        .and_then(Value::as_object)
        .and_then(|o| {
            let text = json_str(o, &["refreshToken", "refresh_token"]);
            (!text.is_empty()).then_some(text)
        });
    let _refresh = refresh
        .filter(|r| !r.is_empty())
        .ok_or_else(|| PluginError::config("no refresh_token in it"))?;
    let cred = Value::Object(object.clone()).to_string();
    Ok(Some(LoginRow {
        email: "imported@claude.local".to_owned(),
        credentials: cred,
        profile: String::new(),
    }))
}

fn json_str(object: &Map<String, Value>, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| object.get(*key).and_then(Value::as_str))
        .unwrap_or_default()
        .to_owned()
}

fn serde_scopes() -> String {
    let inner = IMPORT_SCOPES
        .iter()
        .map(|scope| format!("\"{scope}\""))
        .collect::<Vec<_>>()
        .join(",");
    format!("[{inner}]")
}

async fn store_login(host: &HostContext, row: LoginRow) -> Result<AccountId, PluginError> {
    let profile = crate::creds::Profile::parse(&row.profile);
    let credentials = crate::creds::parse_credentials(&row.credentials)
        .ok_or_else(|| PluginError::config("an imported account has no credentials"))?;
    let id = profile.account_id()?;
    let blob = crate::creds::blob(&credentials, &profile);
    let label = profile.email().unwrap_or(&row.email).to_owned();
    let account = host.accounts().account(id.clone(), label)?;
    host.accounts().upsert(account, blob.to_secret()).await?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use hofund_kit_subscription::{TestHost, block_on};
    use hofund_plugin_api::{PluginId, PluginKind};
    use hofund_plugin_sdk::oauth::{SecretObject, account_id_from};

    use super::*;
    use crate::creds;

    fn host(dir: &tempfile::TempDir) -> TestHost {
        TestHost::open_for(
            PluginKind::Subscription,
            PluginId::from_static("claude"),
            dir.path(),
        )
    }

    fn seed_stored(host: &TestHost, id: &hofund_plugin_api::AccountId, expires_at: i64) {
        let blob = SecretObject::new()
            .with_text(
                creds::CREDENTIALS,
                &format!(
                    r#"{{"claudeAiOauth":{{"accessToken":"a","refreshToken":"rt-stored","expiresAt":{expires_at}}}}}"#
                ),
            )
            .with_text(creds::PROFILE, r#"{"accountUuid":"11111111-2222-3333-4444-555555555555"}"#)
            .to_secret();
        let account = host
            .context()
            .accounts()
            .account(id.clone(), "user@example.test")
            .expect("account");
        block_on(host.context().accounts().upsert(account, blob)).expect("upsert");
    }

    fn seed_stored_refresh_no_expiry(host: &TestHost, id: &hofund_plugin_api::AccountId) {
        let blob = SecretObject::new()
            .with_text(
                creds::CREDENTIALS,
                r#"{"claudeAiOauth":{"accessToken":"a","refreshToken":"rt-stored"}}"#,
            )
            .with_text(
                creds::PROFILE,
                r#"{"accountUuid":"11111111-2222-3333-4444-555555555555"}"#,
            )
            .to_secret();
        let account = host
            .context()
            .accounts()
            .account(id.clone(), "user@example.test")
            .expect("account");
        block_on(host.context().accounts().upsert(account, blob)).expect("upsert");
    }

    #[test]
    fn d36_21_import_rejects_equal_expires_at() {
        let dir = tempfile::tempdir().expect("dir");
        let host = host(&dir);
        let id = account_id_from(&["claude", "11111111-2222-3333-4444-555555555555"]);
        seed_stored(&host, &id, 2000);
        let text = r#"{"swapVersion":1,"version":1,"accounts":[{"email":"user@example.test","credentials":{"claudeAiOauth":{"accessToken":"same-time","refreshToken":"rt-file","expiresAt":2000}},"config":{"oauthAccount":{"accountUuid":"11111111-2222-3333-4444-555555555555"}}}]}"#;
        let error = block_on(store(host.context(), text)).expect_err("equal expiry");
        assert!(
            error
                .to_string()
                .contains(crate::agent_sign_in::IMPORT_EXISTS)
        );
    }

    #[test]
    fn d36_22_import_rejects_undated_over_refresh_only_stored() {
        let dir = tempfile::tempdir().expect("dir");
        let host = host(&dir);
        let id = account_id_from(&["claude", "11111111-2222-3333-4444-555555555555"]);
        seed_stored_refresh_no_expiry(&host, &id);
        let text = r#"{"swapVersion":1,"version":1,"accounts":[{"email":"user@example.test","credentials":{"claudeAiOauth":{"accessToken":"old","refreshToken":"rt-file"}},"config":{"oauthAccount":{"accountUuid":"11111111-2222-3333-4444-555555555555"}}}]}"#;
        let error = block_on(store(host.context(), text)).expect_err("undated over refresh");
        assert!(
            error
                .to_string()
                .contains(crate::agent_sign_in::IMPORT_EXISTS)
        );
    }

    #[test]
    fn d36_10_import_rejects_older_than_stored_sign_in() {
        let dir = tempfile::tempdir().expect("dir");
        let host = host(&dir);
        let id = account_id_from(&["claude", "11111111-2222-3333-4444-555555555555"]);
        seed_stored(&host, &id, 2000);
        let text = r#"{"swapVersion":1,"version":1,"accounts":[{"email":"user@example.test","credentials":{"claudeAiOauth":{"accessToken":"old","refreshToken":"rt-file","expiresAt":1000}},"config":{"oauthAccount":{"accountUuid":"11111111-2222-3333-4444-555555555555"}}}]}"#;
        let error = block_on(store(host.context(), text)).expect_err("older");
        assert!(
            error
                .to_string()
                .contains(crate::agent_sign_in::IMPORT_EXISTS)
        );
    }

    #[test]
    fn d36_11_import_rejects_undated_export_over_usable_sign_in() {
        let dir = tempfile::tempdir().expect("dir");
        let host = host(&dir);
        let id = account_id_from(&["claude", "11111111-2222-3333-4444-555555555555"]);
        seed_stored(&host, &id, 3000);
        let text = r#"{"swapVersion":1,"version":1,"accounts":[{"email":"user@example.test","credentials":{"claudeAiOauth":{"accessToken":"old","refreshToken":"rt-file"}},"config":{"oauthAccount":{"accountUuid":"11111111-2222-3333-4444-555555555555"}}}]}"#;
        let error = block_on(store(host.context(), text)).expect_err("undated");
        assert!(
            error
                .to_string()
                .contains(crate::agent_sign_in::IMPORT_EXISTS)
        );
    }

    #[test]
    fn d36_12_import_accepts_newer_than_live() {
        let dir = tempfile::tempdir().expect("dir");
        let host = host(&dir);
        let id = account_id_from(&["claude", "11111111-2222-3333-4444-555555555555"]);
        seed_stored(&host, &id, 1000);
        let text = r#"{"swapVersion":1,"version":1,"accounts":[{"email":"user@example.test","credentials":{"claudeAiOauth":{"accessToken":"new","refreshToken":"rt-new","expiresAt":5000}},"config":{"oauthAccount":{"accountUuid":"11111111-2222-3333-4444-555555555555"}}}]}"#;
        let ids = block_on(store(host.context(), text)).expect("import");
        assert_eq!(ids, [id]);
    }

    #[test]
    fn d36_13_import_rejects_older_than_saved_account() {
        let dir = tempfile::tempdir().expect("dir");
        let host = host(&dir);
        let id = account_id_from(&["claude", "11111111-2222-3333-4444-555555555555"]);
        seed_stored(&host, &id, 1000);
        let account_dir = crate::account::account_dir(host.context(), &id).expect("dir");
        std::fs::create_dir_all(&account_dir).expect("account dir");
        std::fs::write(
            account_dir.join(crate::creds::CREDENTIALS_FILE),
            r#"{"claudeAiOauth":{"accessToken":"saved-dir","refreshToken":"rt-saved-dir","expiresAt":5000}}"#,
        )
        .expect("account creds");
        let text = r#"{"swapVersion":1,"version":1,"accounts":[{"email":"user@example.test","credentials":{"claudeAiOauth":{"accessToken":"old","refreshToken":"rt-file","expiresAt":2000}},"config":{"oauthAccount":{"accountUuid":"11111111-2222-3333-4444-555555555555"}}}]}"#;
        let error = block_on(store(host.context(), text)).expect_err("older saved");
        assert!(
            error
                .to_string()
                .contains(crate::agent_sign_in::IMPORT_EXISTS)
        );
    }

    #[test]
    fn d27_4_parse_swap_export_fixture_shape() {
        let fixture = include_str!("fixtures/swap_export.json");
        let accounts = parse_export(fixture).expect("parse");
        assert_eq!(accounts.len(), 2);
        assert_eq!(accounts[0].email, "alpha@example.test");
        assert_eq!(accounts[1].alias.as_deref(), Some("work"));
        assert!(accounts[0].credentials.contains("claudeAiOauth"));
    }
}
