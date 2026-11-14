//! Writing Claude Code's sign-in files from a stored account.

use std::path::Path;

use hofund_plugin_api::{AccountId, HostContext, PluginError};
use hofund_plugin_sdk::oauth::SecretObject;

use crate::account;
use crate::creds::{self, Profile};
use crate::keychain::{self, CredentialLocation, ReadContext};
use crate::lock::{self};
use crate::run_lock;

/// Copies the stored credentials and profile into `home`.
pub(crate) async fn write(
    account: &AccountId,
    home: &Path,
    host: &HostContext,
) -> Result<(), PluginError> {
    run_lock::wait(account, host).await?;
    creds::private_dir(home)?;
    let _refresh = lock::refresh_locks(home).await?;
    let live = keychain::read_sign_in(host, home, ReadContext::AgentHome, false).await;
    let live_refresh = live
        .as_ref()
        .and_then(|(credentials, _)| creds::refresh_token(&credentials.text));
    sync_live_sign_in(home, host, live.as_ref().map(|(c, _)| c)).await?;

    let Some(secret) = host.accounts().secret(account).await? else {
        return Err(PluginError::auth(
            "no Claude Code sign-in is stored for this account",
        ));
    };
    let blob = SecretObject::parse(&secret)?;
    if crate::setup_token::is_setup_token(&blob) {
        return Err(PluginError::config(crate::setup_token::AGENT_SWITCH));
    }
    if blob.flag(creds::LAPSED) {
        return Err(PluginError::auth(creds::LAPSED_REASON));
    }
    let Some(stored) = blob.text(creds::CREDENTIALS) else {
        return Err(PluginError::auth(
            "the stored Claude Code sign-in has no credentials",
        ));
    };
    let profile = Profile::parse(blob.text(creds::PROFILE).unwrap_or_default());
    let write_loc = live
        .map(|(_, loc)| loc)
        .unwrap_or_else(|| keychain::default_write_location(home, ReadContext::AgentHome));
    write_outgoing(host, home, stored, live_refresh.as_deref(), &write_loc).await?;
    let _config = lock::config_lock(&home.join(creds::PROFILE_FILE)).await?;
    creds::write_private(&home.join(creds::PROFILE_FILE), &profile.settings_text())?;
    Ok(())
}

async fn write_outgoing(
    host: &HostContext,
    dir: &Path,
    stored: &str,
    live_refresh: Option<&str>,
    loc: &CredentialLocation,
) -> Result<(), PluginError> {
    match loc {
        CredentialLocation::File(path) => creds::write_credentials_cas(path, stored, live_refresh),
        CredentialLocation::Keychain { .. } => {
            keychain::write_sign_in(host, dir, stored, loc).await
        }
    }
}

async fn sync_live_sign_in(
    home: &Path,
    host: &HostContext,
    live: Option<&creds::Credentials>,
) -> Result<(), PluginError> {
    let Some(credentials) = live else {
        return Ok(());
    };
    let profile = Profile::read(home);
    let id = profile.account_id()?;
    let Some(secret) = host.accounts().secret(&id).await? else {
        return Ok(());
    };
    let blob = SecretObject::parse(&secret)?;
    let stored = blob.text(creds::CREDENTIALS).unwrap_or_default();
    if stored == credentials.text {
        return Ok(());
    }
    let updated = blob.with_text(creds::CREDENTIALS, &credentials.text);
    host.accounts()
        .upsert(
            host.accounts()
                .account(id.clone(), profile.email().unwrap_or(id.as_str()))?,
            updated.to_secret(),
        )
        .await?;
    Ok(())
}

/// Writes the sign-in read from `home` into the account store. Used by discovery import.
pub(crate) async fn import_live(
    home: &Path,
    host: &HostContext,
    ctx: ReadContext,
    honor_discovery_off: bool,
) -> Result<AccountId, PluginError> {
    creds::private_dir(home)?;
    let _refresh = lock::refresh_locks(home).await?;
    let Some((credentials, _loc)) =
        keychain::read_sign_in(host, home, ctx, honor_discovery_off).await
    else {
        return Err(PluginError::auth(
            "Claude Code has no readable sign-in in that directory",
        ));
    };
    let profile = Profile::read(home);
    let id = profile.account_id()?;
    if let Some(secret) = host.accounts().secret(&id).await? {
        let blob = SecretObject::parse(&secret)?;
        let same_refresh = blob
            .text(creds::CREDENTIALS)
            .and_then(creds::refresh_token)
            .is_some_and(|stored| {
                creds::refresh_token(&credentials.text).is_some_and(|live| live == stored)
            });
        if same_refresh {
            return Ok(id);
        }
        if import_blocked(host, &id, &credentials.text, &profile).await? {
            return Err(PluginError::config(import_exists_reason()));
        }
    }
    let label = profile
        .email()
        .map(str::to_owned)
        .unwrap_or_else(|| id.to_string());
    let blob = creds::blob(&credentials, &profile);
    host.accounts()
        .upsert(
            host.accounts().account(id.clone(), label)?,
            blob.to_secret(),
        )
        .await?;
    Ok(id)
}

pub(crate) const IMPORT_EXISTS: &str = "hofund has a newer sign-in for this account (Claude Code's own, when it is signed in to it): kept, not replaced by this file's";

fn import_exists_reason() -> &'static str {
    IMPORT_EXISTS
}

pub(crate) async fn import_blocked(
    host: &HostContext,
    id: &AccountId,
    incoming: &str,
    profile: &Profile,
) -> Result<bool, PluginError> {
    let incoming_at = creds::expires_at(incoming).unwrap_or(0);
    let mut kept_at = 0_i64;
    let mut kept_refresh = false;
    if let Some(secret) = host.accounts().secret(id).await?
        && let Ok(blob) = SecretObject::parse(&secret)
        && let Some(text) = blob.text(creds::CREDENTIALS)
    {
        kept_at = creds::expires_at(text).unwrap_or(0);
        kept_refresh = creds::refresh_token(text).is_some();
    }
    if let Ok(dir) = account::account_dir(host, id)
        && let Some(dir_creds) =
            crate::keychain::read_sign_in(host, &dir, ReadContext::ConfigDir, false)
                .await
                .map(|(c, _)| c)
    {
        kept_at = kept_at.max(creds::expires_at(&dir_creds.text).unwrap_or(0));
        kept_refresh |= creds::refresh_token(&dir_creds.text).is_some();
    }
    if let Some(live_dir) = crate::cli::configured_dir()
        && let Some(live) = keychain::read_sign_in(host, &live_dir, ReadContext::AgentHome, false)
            .await
            .map(|(c, _)| c)
    {
        let live_profile = Profile::read(&live_dir);
        if live_profile.account_id().ok().as_ref() == Some(id) {
            kept_at = kept_at.max(creds::expires_at(&live.text).unwrap_or(0));
            kept_refresh |= creds::refresh_token(&live.text).is_some();
        }
    }
    let _ = profile;
    let kept_usable = kept_at > 0 || kept_refresh;
    Ok(kept_usable && incoming_at <= kept_at)
}
