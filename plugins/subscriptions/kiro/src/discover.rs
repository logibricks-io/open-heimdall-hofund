//! Account discovery from kiro-auth-token.json.

use std::path::Path;

use hofund_plugin_api::{AccountId, DiscoveredAgentAccount, HostContext, PluginError};

use crate::creds::{self, Credential};

pub(crate) const REFERENCE: &str = "ide-token";

pub(crate) fn list(home: &Path) -> Result<Vec<DiscoveredAgentAccount>, PluginError> {
    let path = creds::token_path_in_home(home);
    let Ok(credential) = creds::read_token_file(&path) else {
        return Ok(Vec::new());
    };
    let id = creds::account_id_for_home(home, &credential);
    let row = DiscoveredAgentAccount::new(id, credential.label.clone(), REFERENCE)?;
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
    let path = creds::token_path_in_home(home);
    let credential = creds::read_token_file(&path)?;
    store(home, &credential, host).await
}

pub(crate) async fn store(
    home: &Path,
    credential: &Credential,
    host: &HostContext,
) -> Result<AccountId, PluginError> {
    let id = creds::account_id_for_home(home, credential);
    let account = host
        .accounts()
        .account(id.clone(), credential.label.clone())?;
    host.accounts()
        .upsert(account, creds::secret_blob(credential).to_secret())
        .await?;
    Ok(id)
}
