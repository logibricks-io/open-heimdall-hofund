//! Importing Claude accounts from external export files.

use hofund_plugin_api::{AccountId, HostContext, PluginError};
use hofund_plugin_sdk::json::Value;

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

/// Stores every account in a swap export.
///
/// # Errors
///
/// Returns [`PluginError`] when the file is this format and cannot be stored.
/// A file that is not a swap export is [`PluginErrorKind::Unsupported`](hofund_plugin_api::PluginErrorKind).
pub async fn store(host: &HostContext, text: &str) -> Result<Vec<AccountId>, PluginError> {
    let trimmed = text.trim();
    let Ok(value) = hofund_plugin_sdk::json::from_str::<Value>(trimmed) else {
        return Err(PluginError::unsupported(
            "not an account export this subscription reads",
        ));
    };
    let is_swap = value
        .as_object()
        .is_some_and(|root| root.get("swapVersion").is_some());
    if !is_swap {
        return Err(PluginError::unsupported(
            "not an account export this subscription reads",
        ));
    }
    let rows = parse_export(trimmed)?;
    let mut ids = Vec::new();
    for row in rows {
        let profile = crate::creds::Profile::parse(&row.profile);
        let credentials = crate::creds::parse_credentials(&row.credentials)
            .ok_or_else(|| PluginError::config("an imported account has no credentials"))?;
        let id = profile.account_id()?;
        let blob = crate::creds::blob(&credentials, &profile);
        let account = host.accounts().account(id.clone(), row.email)?;
        host.accounts().upsert(account, blob.to_secret()).await?;
        ids.push(id);
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

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
