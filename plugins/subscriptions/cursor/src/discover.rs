//! Discovering cursor-agent sign-in from auth.json.

use std::path::Path;

use hofund_plugin_api::{AccountId, DiscoveredAgentAccount, HostContext, PluginError};

use crate::agent_sign_in;
use crate::auth;
use crate::tokens;

/// Opaque handle for the agent sign-in cursor-agent keeps locally.
pub(crate) const REFERENCE: &str = "agent-sign-in";

pub(crate) fn list(home: &Path) -> Result<Vec<DiscoveredAgentAccount>, PluginError> {
    let token = auth::read_access_file(&auth::auth_path(home));
    let Some(token) = token else {
        return Ok(Vec::new());
    };
    let (label, id) = tokens::jwt_identity(&token)
        .ok_or_else(|| PluginError::auth("Cursor auth.json does not name an account"))?;
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
    agent_sign_in::import_live(home, host).await
}
