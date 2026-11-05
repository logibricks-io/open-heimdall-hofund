//! Import Qoder credential JSON supplied by the user.

use hofund_plugin_api::{AccountId, HostContext, PluginError};

use crate::creds::{self, Credential};

pub(crate) async fn import_accounts(
    host: &HostContext,
    text: &str,
) -> Result<Vec<AccountId>, PluginError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(PluginError::config("nothing to import"));
    }
    let credential = creds::parse_json(trimmed)?;
    store(&credential, host).await.map(|id| vec![id])
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
    Ok(id)
}
