//! Importing Cursor auth.json text into an account.

use crate::auth;
use crate::tokens;
use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::oauth::SecretObject;

/// Stores an account from raw auth.json (`accessToken` field).
pub(crate) async fn store(host: &HostContext, text: &str) -> Result<Vec<AccountId>, PluginError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(PluginError::config("nothing to import"));
    }
    let access = auth::parse_access_text(trimmed)
        .ok_or_else(|| PluginError::config("auth.json must contain a non-empty accessToken"))?;
    let (label, id) = tokens::jwt_identity(&access)
        .ok_or_else(|| PluginError::auth("the access token does not name an account"))?;
    let access_secret = SecretString::from(access.clone());
    let expires = tokens::jwt_expiry_ms(&access)
        .unwrap_or_else(|| tokens::unix_ms(host.clock().now()) as u64 + 30 * 24 * 3600 * 1000);
    let refresh_secret = if let Some(stored) = host.accounts().secret(&id).await? {
        let blob = SecretObject::parse(&stored)?;
        auth::parse_refresh_text(trimmed)
            .map(SecretString::from)
            .or_else(|| blob.secret(tokens::REFRESH))
            .unwrap_or_else(|| SecretString::from("none"))
    } else {
        auth::parse_refresh_text(trimmed)
            .map(SecretString::from)
            .unwrap_or_else(|| SecretString::from("none"))
    };
    let blob = tokens::blob_oauth(&access_secret, &refresh_secret, expires, &label, None);
    let account = host.accounts().account(id.clone(), label)?;
    host.accounts().upsert(account, blob.to_secret()).await?;
    Ok(vec![id])
}

/// Returns unsupported for non-auth.json imports.
pub(crate) async fn import_accounts(
    host: &HostContext,
    text: &str,
) -> Result<Vec<AccountId>, PluginError> {
    let trimmed = text.trim();
    if trimmed.starts_with('{') && trimmed.contains("accessToken") {
        return store(host, trimmed).await;
    }
    Err(PluginError::unsupported(
        "not an account file this subscription reads",
    ))
}
