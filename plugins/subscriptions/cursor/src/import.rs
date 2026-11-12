//! Importing Cursor auth.json text or a Cursor API key into an account.

use crate::auth;
use crate::endpoints::Endpoints;
use crate::exchange;
use crate::tokens;
use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::oauth::{SecretObject, secret_text};

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

/// Exchanges `key`, stores only the key (not the access token), and returns
/// the account id derived from the exchanged token.
pub(crate) async fn store_api_key(
    host: &HostContext,
    endpoints: &Endpoints,
    key: SecretString,
) -> Result<AccountId, PluginError> {
    let trimmed = secret_text(&key).trim();
    if trimmed.is_empty() {
        return Err(PluginError::auth("Cursor API key is empty"));
    }
    let key = SecretString::from(trimmed.to_owned());
    let access = exchange::exchange_key(host, endpoints, &key).await?;
    let (label, id) = tokens::jwt_identity(secret_text(&access))
        .ok_or_else(|| PluginError::auth("Cursor signed in but did not name the account"))?;
    let blob = tokens::blob_api_key(&key, &label, None);
    let account = host.accounts().account(id.clone(), label)?;
    host.accounts().upsert(account, blob.to_secret()).await?;
    Ok(id)
}

/// Returns unsupported for inputs this subscription does not read.
///
/// `CURSOR_API_KEY` is read here when `text` is empty, then stored. Later
/// calls do not read the process environment.
pub(crate) async fn import_accounts(
    host: &HostContext,
    endpoints: &Endpoints,
    text: &str,
) -> Result<Vec<AccountId>, PluginError> {
    let trimmed = text.trim();
    if trimmed.starts_with('{') && trimmed.contains("accessToken") {
        return store(host, trimmed).await;
    }
    if !trimmed.is_empty() && !trimmed.starts_with('{') {
        let id = store_api_key(host, endpoints, SecretString::from(trimmed.to_owned())).await?;
        return Ok(vec![id]);
    }
    if trimmed.is_empty() {
        if let Some(key) = exchange::read_env_api_key() {
            let id = store_api_key(host, endpoints, key).await?;
            return Ok(vec![id]);
        }
        return Err(PluginError::config("nothing to import"));
    }
    Err(PluginError::unsupported(
        "not an account file this subscription reads",
    ))
}
