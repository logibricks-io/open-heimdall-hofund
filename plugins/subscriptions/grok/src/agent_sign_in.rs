//! Writing auth.json from a stored account for agent sign-in switching.

use std::path::Path;

use hofund_plugin_api::{AccountId, HostContext, PluginError};

use crate::creds::Stored;

/// Writes `home/auth.json` in the shape the CLI reads.
pub(crate) async fn write(
    account: &AccountId,
    home: &Path,
    host: &HostContext,
) -> Result<(), PluginError> {
    let stored = Stored::load(host, account).await?;
    crate::creds::write_auth(home, &stored)
}
