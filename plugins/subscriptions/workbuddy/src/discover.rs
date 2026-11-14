//! Discover WorkBuddy desktop auth from CodeBuddyExtension storage.

use std::path::Path;

use hofund_plugin_api::{AccountId, DiscoveredAgentAccount, HostContext, PluginError};

use crate::auth_store;
use crate::creds::{self, Credential};
use crate::site::Site;

pub(crate) const REFERENCE_CN: &str = "desktop-auth-cn";
pub(crate) const REFERENCE_AI: &str = "desktop-auth-ai";

pub(crate) fn list(
    home: &Path,
    now: std::time::SystemTime,
) -> Result<Vec<DiscoveredAgentAccount>, PluginError> {
    let mut out = Vec::new();
    for (site, reference) in [
        (Site::China, REFERENCE_CN),
        (Site::International, REFERENCE_AI),
    ] {
        let path = auth_store::auth_path_in(home, site);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let now_ms = now
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as i64)
            .unwrap_or(0);
        let Ok(credential) = creds::parse_desktop_info(&text, site, &path, now_ms) else {
            continue;
        };
        let id = creds::account_id(&credential);
        let row = DiscoveredAgentAccount::new(id, credential.label.clone(), reference)?;
        out.push(row);
    }
    Ok(out)
}

pub(crate) async fn import(
    home: &Path,
    reference: &str,
    host: &HostContext,
) -> Result<AccountId, PluginError> {
    let site = match reference {
        REFERENCE_CN => Site::China,
        REFERENCE_AI => Site::International,
        _ => {
            return Err(PluginError::config(format!(
                "unknown agent sign-in reference `{reference}`"
            )));
        }
    };
    let path = auth_store::auth_path_in(home, site);
    let text = std::fs::read_to_string(&path).map_err(|error| {
        PluginError::auth(format!("{} can't be read ({error})", path.display()))
    })?;
    let now_ms = host
        .clock()
        .now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0);
    let credential = creds::parse_desktop_info(&text, site, &path, now_ms)?;
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
