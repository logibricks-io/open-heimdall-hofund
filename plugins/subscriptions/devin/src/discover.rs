//! Account discovery from the CLI credentials file.

use std::path::Path;

use hofund_plugin_api::{AccountId, DiscoveredAgentAccount, HostContext, PluginError};

use crate::creds::{self, Credential};

pub(crate) const REFERENCE: &str = "cli-credentials";

pub(crate) fn list(home: &Path) -> Result<Vec<DiscoveredAgentAccount>, PluginError> {
    let path = if home.join("credentials.toml").is_file() {
        home.join("credentials.toml")
    } else if let Ok(default) = creds::default_credentials_path() {
        default
    } else {
        return Ok(Vec::new());
    };
    let Ok(credential) = creds::read_credentials(&path) else {
        return Ok(Vec::new());
    };
    let id = creds::account_id(&credential);
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
    let path = if home.join("credentials.toml").is_file() {
        home.join("credentials.toml")
    } else {
        creds::default_credentials_path()?
    };
    let credential = creds::read_credentials(&path)?;
    let id = creds::account_id(&credential);
    if host.accounts().secret(&id).await?.is_some() {
        return Ok(id);
    }
    store(&credential, host).await
}

pub(crate) async fn store(
    credential: &Credential,
    host: &HostContext,
) -> Result<AccountId, PluginError> {
    let id = creds::account_id(credential);
    let account = host
        .accounts()
        .account(id.clone(), credential.label.clone())?;
    host.accounts()
        .upsert(account, creds::secret_blob(credential).to_secret())
        .await?;
    Ok(id)
}
