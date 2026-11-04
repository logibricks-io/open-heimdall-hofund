//! Writing and importing cursor-agent sign-in.

use std::path::Path;

use hofund_plugin_api::{AccountId, HostContext, PluginError};
use hofund_plugin_sdk::oauth::SecretObject;

use crate::auth;
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
