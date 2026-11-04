//! Account discovery from the CLI credentials file.

use std::path::Path;

use hofund_plugin_api::{AccountId, DiscoveredAgentAccount, HostContext, PluginError};

use crate::creds::{self, Credential};

pub(crate) const REFERENCE: &str = "cli-credentials";

pub(crate) fn list(home: &Path) -> Result<Vec<DiscoveredAgentAccount>, PluginError> {
    let path = credentials_path(home);
    let Ok(credential) = creds::read_credentials(&path) else {
        return Ok(Vec::new());
    };
    let id = creds::agent_account_id(home);
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
    let path = credentials_path(home);
    let credential = creds::read_credentials(&path)?;
    let id = creds::agent_account_id(home);
    store(&credential, host, id).await
}

fn credentials_path(home: &Path) -> std::path::PathBuf {
    let direct = home.join("credentials.toml");
    if direct.is_file() {
        return direct;
    }
    creds::agent_credentials_path(home)
}

pub(crate) async fn store(
    credential: &Credential,
    host: &HostContext,
    id: AccountId,
) -> Result<AccountId, PluginError> {
    let account = host
        .accounts()
        .account(id.clone(), credential.label.clone())?;
    host.accounts()
        .upsert(account, creds::secret_blob(credential).to_secret())
        .await?;
    Ok(id)
}
