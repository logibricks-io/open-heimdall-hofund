//! Running `claude` for one stored account.
//!
//! Each account has `<state>/accounts/<id>` (0700). Before a run the stored
//! credentials file is written there (0600); afterwards it is read back and
//! removed, so the tokens rest only in the account store. A file Claude Code
//! rotated is committed under the account lease, which is held for the run.

use std::path::{Path, PathBuf};
use std::time::Duration;

use hofund_plugin_api::{AccountId, HostContext, PluginError};
use hofund_plugin_sdk::oauth::SecretObject;

use crate::creds::{self, Profile};

/// How long a refresh waits for another holder of the account lease.
const LEASE_WAIT: Duration = Duration::from_secs(30);

/// `<state>/accounts/<id>`. An account id is kebab case.
pub(crate) fn account_dir(host: &HostContext, account: &AccountId) -> Result<PathBuf, PluginError> {
    Ok(host
        .dirs()
        .state_dir()?
        .join("accounts")
        .join(account.as_str()))
}

/// Asks Claude Code for the account's status, which refreshes its tokens
/// when they are due, and keeps what it wrote.
pub(crate) async fn refresh(
    host: &HostContext,
    program: impl FnOnce() -> Result<PathBuf, PluginError>,
    account: &AccountId,
) -> Result<(), PluginError> {
    let lease = host.accounts().lease(account, LEASE_WAIT).await?;
    let blob = SecretObject::parse(lease.secret())?;
    if blob.flag(creds::LAPSED) {
        return Err(PluginError::auth(creds::LAPSED_REASON));
    }
    let Some(stored) = blob.text(creds::CREDENTIALS) else {
        return Err(PluginError::auth(
            "the stored Claude Code sign-in has no credentials",
        ));
    };
    let stored = stored.to_owned();
    let profile = Profile::parse(blob.text(creds::PROFILE).unwrap_or_default());
    let program = program()?;
    let dir = account_dir(host, account)?;
    creds::private_dir(&dir)?;
    let file = dir.join(creds::CREDENTIALS_FILE);
    let ran = run(host, &program, &dir, &file, &stored, &profile).await;
    let read_back = creds::read_credentials(&dir);
    let _ = std::fs::remove_file(&file);
    let status = ran?;
    if crate::creds::logged_in_says_no(status.as_ref()) {
        let lapsed = blob.with_flag(creds::LAPSED, true);
        lease.commit(None, lapsed.to_secret()).await?;
        return Err(PluginError::auth(creds::LAPSED_REASON));
    }
    // A file Claude Code rotated is kept even when it then failed to report.
    if let Some(rotated) = read_back.filter(|rotated| rotated.text != stored) {
        let updated = blob.with_text(creds::CREDENTIALS, &rotated.text);
        lease.commit(None, updated.to_secret()).await?;
    }
    if status.is_none() {
        return Err(PluginError::io(
            "Claude Code did not report the account status; the stored sign-in is kept",
        ));
    }
    Ok(())
}

async fn run(
    host: &HostContext,
    program: &Path,
    dir: &Path,
    file: &Path,
    stored: &str,
    profile: &Profile,
) -> Result<Option<hofund_plugin_sdk::json::Value>, PluginError> {
    creds::write_private(file, stored)?;
    let settings = dir.join(creds::PROFILE_FILE);
    if !settings.exists() {
        creds::write_private(&settings, &profile.settings_text())?;
    }
    Ok(crate::status(host, program, dir).await)
}
