//! Writing credentials.toml for agent sign-in switching.

use std::path::Path;

use hofund_plugin_api::{AccountId, HostContext, PluginError};

use crate::creds::{self, Stored};

pub(crate) async fn write(
    account: &AccountId,
    home: &Path,
    host: &HostContext,
) -> Result<(), PluginError> {
    let stored = Stored::load(host, account).await?;
    let path = creds::agent_credentials_path(home);
    creds::write_credentials(&path, &stored.credential)
}
