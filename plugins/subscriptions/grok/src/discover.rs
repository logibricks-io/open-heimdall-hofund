//! Reading the CLI's auth.json for account discovery.

use std::path::Path;

use hofund_plugin_api::{AccountId, DiscoveredAgentAccount, HostContext, PluginError};

use crate::creds::{self, Credential};

/// Opaque import handle for the sign-in the CLI keeps in its home.
pub(crate) const REFERENCE: &str = "agent-sign-in";

/// Lists the account named in `home`, when auth.json is present.
pub(crate) fn list(home: &Path) -> Result<Vec<DiscoveredAgentAccount>, PluginError> {
    let credential = match creds::read_sync(home) {
        Ok(credential) => credential,
        Err(_) => return Ok(Vec::new()),
    };
    let label = if !credential.email.is_empty() {
        credential.email.clone()
    } else if !credential.who.is_empty() {
        credential.who.clone()
    } else {
        "Grok Build".to_owned()
    };
    let id = creds::account_id(&credential);
    let row = DiscoveredAgentAccount::new(id, label, REFERENCE)?;
    Ok(vec![row])
}

/// Stores the sign-in read from `home`.
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
    let credential = creds::read_sync(home).map_err(|error| {
        PluginError::auth(format!(
            "Grok Build has no readable sign-in in that directory: {}",
            error.message()
        ))
    })?;
    let id = creds::account_id(&credential);
    if host.accounts().secret(&id).await?.is_some() {
        return Ok(id);
    }
    store(home, &credential, host).await
}

pub(crate) async fn store(
    home: &Path,
    credential: &Credential,
    host: &HostContext,
) -> Result<AccountId, PluginError> {
    let id = creds::account_id(credential);
    let label = if !credential.email.is_empty() {
        credential.email.clone()
    } else {
        credential.who.clone()
    };
    let secret = creds::secret_blob(home, credential).to_secret();
    let account = host.accounts().account(id.clone(), label)?;
    host.accounts().upsert(account, secret).await?;
    Ok(id)
}
