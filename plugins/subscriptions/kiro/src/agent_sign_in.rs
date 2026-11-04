//! Writing kiro-auth-token.json for agent sign-in switching.

use std::path::Path;

use hofund_plugin_api::{AccountId, HostContext, PluginError};

use crate::creds::{self, Stored, TOKEN_FILE};

pub(crate) async fn write(
    account: &AccountId,
    home: &Path,
    host: &HostContext,
) -> Result<(), PluginError> {
    let stored = Stored::load(host, account).await?;
    let path = creds::default_token_path(home);
    if path.file_name().is_some_and(|name| name != TOKEN_FILE) {
        return creds::write_token_file(&home.join(TOKEN_FILE), &stored.credential);
    }
    creds::write_token_file(&path, &stored.credential)
}
