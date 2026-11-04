//! Discover WorkBuddy desktop auth beside an agent home.

use std::path::{Path, PathBuf};

use hofund_plugin_api::{AccountId, DiscoveredAgentAccount, HostContext, PluginError};

use crate::creds::{self, Credential};
use crate::site::Site;

pub(crate) const REFERENCE_CN: &str = "desktop-auth-cn";
pub(crate) const REFERENCE_AI: &str = "desktop-auth-ai";

pub(crate) fn auth_path(home: &Path, site: Site) -> PathBuf {
    home.join("auth").join(site.auth_file())
}

pub(crate) fn list(home: &Path) -> Result<Vec<DiscoveredAgentAccount>, PluginError> {
    let mut out = Vec::new();
    for (site, reference) in [
        (Site::China, REFERENCE_CN),
        (Site::International, REFERENCE_AI),
    ] {
        let path = auth_path(home, site);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(credential) = creds::parse_desktop_info(&text, site) else {
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
    let path = auth_path(home, site);
    let text = std::fs::read_to_string(&path).map_err(|error| {
        PluginError::auth(format!("{} can't be read ({error})", path.display()))
    })?;
    let credential = creds::parse_desktop_info(&text, site)?;
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
