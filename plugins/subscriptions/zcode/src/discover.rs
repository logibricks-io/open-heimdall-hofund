//! Account discovery from Z Code credentials.json.

use std::path::Path;

use hofund_plugin_api::{AccountId, DiscoveredAgentAccount, HostContext, PluginError};

use crate::creds::{self, PlanAuth};
use crate::device;

pub(crate) const REFERENCE: &str = "agent-credentials";

pub(crate) fn list(home: &Path) -> Result<Vec<DiscoveredAgentAccount>, PluginError> {
    let Ok(auth) = creds::parse_agent_home(home) else {
        return Ok(Vec::new());
    };
    let id = creds::account_id_for(&auth);
    let row = DiscoveredAgentAccount::new(id, auth.label.clone(), REFERENCE)?;
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
    let auth = creds::parse_agent_home(home)?;
    store(&auth, home, host).await
}

pub(crate) async fn store(
    auth: &PlanAuth,
    _home: &Path,
    host: &HostContext,
) -> Result<AccountId, PluginError> {
    let mid = device::device_mid(&device::device_path(host)?)?;
    store_with_device(auth, &mid, "", host).await
}

pub(crate) async fn store_with_device(
    auth: &PlanAuth,
    device: &str,
    plan: &str,
    host: &HostContext,
) -> Result<AccountId, PluginError> {
    let id = creds::account_id_for(auth);
    let account = host.accounts().account(id.clone(), auth.label.clone())?;
    let mut blob = creds::secret_blob(auth, device);
    if !plan.is_empty() {
        blob = blob.with_text("plan", plan);
    }
    host.accounts().upsert(account, blob.to_secret()).await?;
    Ok(id)
}
