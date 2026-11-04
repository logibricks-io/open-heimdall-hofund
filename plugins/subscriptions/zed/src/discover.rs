//! Agent-home credential discovery.

use std::path::Path;

use hofund_plugin_api::{AccountId, DiscoveredAgentAccount, HostContext, PluginError};

use crate::creds::{self, Credential};

pub(crate) const REFERENCE: &str = "credential-json";

pub(crate) fn list(home: &Path) -> Result<Vec<DiscoveredAgentAccount>, PluginError> {
    let path = creds::credential_path(home);
    let Ok(credential) = creds::read_file(&path) else {
        return Ok(Vec::new());
    };
    let label = creds::label(&credential);
    let id = creds::account_id(&credential);
    let row = DiscoveredAgentAccount::new(id, label, REFERENCE)?;
    Ok(vec![row])
}

pub(crate) async fn import(
    home: &Path,
    reference: &str,
    host: &HostContext,
) -> Result<AccountId, PluginError> {
    if reference != REFERENCE {
        return Err(PluginError::config(format!(
            "unknown agent sign-in reference `{reference}`"
        )));
    }
    let credential = creds::read_file(&creds::credential_path(home))?;
    store(&credential, host).await
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
