//! Writing Claude Code's sign-in files from a stored account.

use std::path::Path;

use hofund_plugin_api::{AccountId, HostContext, PluginError};
use hofund_plugin_sdk::oauth::SecretObject;

use crate::creds::{self, Profile};

/// Copies the stored credentials and profile into `home`.
pub(crate) async fn write(
    account: &AccountId,
    home: &Path,
    host: &HostContext,
) -> Result<(), PluginError> {
    let Some(secret) = host.accounts().secret(account).await? else {
        return Err(PluginError::auth(
            "no Claude Code sign-in is stored for this account",
        ));
    };
    let blob = SecretObject::parse(&secret)?;
    if blob.flag(creds::LAPSED) {
        return Err(PluginError::auth(creds::LAPSED_REASON));
    }
    let Some(stored) = blob.text(creds::CREDENTIALS) else {
        return Err(PluginError::auth(
            "the stored Claude Code sign-in has no credentials",
        ));
    };
    let profile = Profile::parse(blob.text(creds::PROFILE).unwrap_or_default());
    creds::private_dir(home)?;
    creds::write_private(&home.join(creds::CREDENTIALS_FILE), stored)?;
    creds::write_private(&home.join(creds::PROFILE_FILE), &profile.settings_text())?;
    Ok(())
}
