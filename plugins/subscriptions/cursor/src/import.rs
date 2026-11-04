//! Importing Cursor auth.json text into an account.

use crate::auth;
use crate::tokens;
use hofund_plugin_api::{AccountId, HostContext, PluginError};

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
    if host.accounts().secret(&id).await?.is_some() {
        return Ok(vec![id]);
    }
    let access_secret = hofund_plugin_api::SecretString::from(access.clone());
    let refresh = tokens::blob_oauth(
        &access_secret,
        &hofund_plugin_api::SecretString::from("none"),
        tokens::jwt_expiry_ms(&access).unwrap_or(0),
        &label,
        None,
    );
    let account = host.accounts().account(id.clone(), label)?;
    host.accounts().upsert(account, refresh.to_secret()).await?;
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
