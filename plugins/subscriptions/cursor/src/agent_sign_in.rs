//! Writing and importing cursor-agent sign-in.

use std::path::Path;

use hofund_plugin_api::{AccountId, HostContext, PluginError};
use hofund_plugin_sdk::oauth::SecretObject;

use crate::auth;
use crate::endpoints::Endpoints;
use crate::tokens;

pub(crate) const IMPORT_EXISTS: &str = "a newer Cursor sign-in is already stored for this account";

pub(crate) async fn import_live(home: &Path, host: &HostContext) -> Result<AccountId, PluginError> {
    let access = auth::read_agent_access(home).ok_or_else(|| {
        PluginError::auth("cursor-agent has no readable sign-in in that directory")
    })?;
    let (label, id) = tokens::jwt_identity(&access)
        .ok_or_else(|| PluginError::auth("cursor-agent's token does not name an account"))?;
    if let Some(stored) = host.accounts().secret(&id).await? {
        let blob = SecretObject::parse(&stored)?;
        if !tokens::is_agent(&blob) {
            return Err(PluginError::config(IMPORT_EXISTS));
        }
        return Ok(id);
    }
    let blob = tokens::blob_agent(&label, None);
    let account = host.accounts().account(id.clone(), label)?;
    host.accounts().upsert(account, blob.to_secret()).await?;
    Ok(id)
}

pub(crate) async fn write(
    account: &AccountId,
    home: &Path,
    host: &HostContext,
    endpoints: &Endpoints,
) -> Result<(), PluginError> {
    let Some(secret) = host.accounts().secret(account).await? else {
        return Err(PluginError::auth(
            "no Cursor sign-in is stored for this account",
        ));
    };
    let blob = SecretObject::parse(&secret)?;
    if tokens::is_agent(&blob) {
        return Ok(());
    }
    let access = blob
        .secret(tokens::ACCESS)
        .ok_or_else(|| PluginError::auth("the stored Cursor sign-in has no access token"))?;
    let config = auth::config_dir(home);
    auth::write_auth_file(&config, hofund_plugin_sdk::oauth::secret_text(&access))?;
    let _host = host;
    let _endpoints = endpoints;
    Ok(())
}
