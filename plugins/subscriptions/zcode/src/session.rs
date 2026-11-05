//! Stored account session material.

use std::path::Path;
use std::time::Duration;

use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::oauth::{SecretObject, TokenCache, same_secret};

use crate::creds::{self, LAPSED, LAPSED_REASON, PlanAuth};
use crate::team;

const LEASE_WAIT: Duration = Duration::from_secs(30);

pub(crate) struct Stored {
    pub auth: PlanAuth,
    pub api_key: SecretString,
}

/// Offline readiness for the daemon account tick (no vendor HTTP).
pub(crate) fn offline_status(secret: &SecretString) -> Result<(), PluginError> {
    let blob = SecretObject::parse(secret)
        .map_err(|_| PluginError::auth("the stored Z Code sign-in is unreadable"))?;
    if blob.flag(LAPSED) {
        return Err(PluginError::auth(LAPSED_REASON));
    }
    let auth = creds::parse_secret(secret)?;
    if auth.team() {
        if auth.team_token.is_some() {
            return Ok(());
        }
        return Err(PluginError::auth(
            "the stored Z Code team seat has no business sign-in token",
        ));
    }
    if auth.api_key.is_some() {
        return Ok(());
    }
    Err(PluginError::auth(
        "the stored Z Code sign-in has no GLM Coding Plan key",
    ))
}

pub(crate) async fn load(
    host: &HostContext,
    endpoints: &crate::Endpoints,
    account: &AccountId,
    device_file: &Path,
    team_keys: &TokenCache<SecretString>,
) -> Result<Stored, PluginError> {
    let Some(secret) = host.accounts().secret(account).await? else {
        return Err(PluginError::auth(
            "no Z Code sign-in is stored for this account",
        ));
    };
    parse_loaded(host, endpoints, account, &secret, device_file, team_keys).await
}

pub(crate) async fn parse_loaded(
    host: &HostContext,
    endpoints: &crate::Endpoints,
    account: &AccountId,
    secret: &SecretString,
    device_file: &Path,
    team_keys: &TokenCache<SecretString>,
) -> Result<Stored, PluginError> {
    let _ = SecretObject::parse(secret)?;
    let auth = creds::parse_secret(secret)?;
    let api_key = if auth.team() && auth.api_key.is_none() {
        resolve_team_project_key(host, endpoints, account, &auth, device_file, team_keys).await?
    } else {
        auth.api_key.clone().ok_or_else(|| {
            PluginError::auth("the stored Z Code sign-in has no GLM Coding Plan key")
        })?
    };
    Ok(Stored { auth, api_key })
}

async fn resolve_team_project_key(
    host: &HostContext,
    endpoints: &crate::Endpoints,
    account: &AccountId,
    auth: &PlanAuth,
    device_file: &Path,
    team_keys: &TokenCache<SecretString>,
) -> Result<SecretString, PluginError> {
    let endpoints = endpoints.clone();
    let auth = auth.clone();
    let device_file = device_file.to_path_buf();
    let account_id = account.clone();
    team_keys
        .get(host.clock(), account, || {
            let account_id = account_id.clone();
            async move {
                match team::resolve_project_key(host, &endpoints, &auth, &device_file).await {
                    Ok(key) => Ok((key, None)),
                    Err(error) if error.kind() == hofund_plugin_api::PluginErrorKind::Auth => {
                        team_keys.remove(&account_id);
                        mark_team_lapsed(host, &account_id, &auth).await?;
                        Err(error)
                    }
                    Err(error) => Err(error),
                }
            }
        })
        .await
}

/// Forgets a cached team project key after the plan or monitor API rejects it.
pub(crate) fn drop_team_key_cache(team_keys: &TokenCache<SecretString>, account: &AccountId) {
    team_keys.remove(account);
}

async fn mark_team_lapsed(
    host: &HostContext,
    account: &AccountId,
    auth: &PlanAuth,
) -> Result<(), PluginError> {
    let Some(refused) = auth.team_token.as_ref() else {
        return Ok(());
    };
    let lease = host.accounts().lease(account, LEASE_WAIT).await?;
    let Ok(blob) = SecretObject::parse(lease.secret()) else {
        return Ok(());
    };
    let same = blob
        .secret("team_token")
        .is_some_and(|token| same_secret(&token, refused));
    if !same {
        return Ok(());
    }
    lease
        .commit(None, blob.with_flag(LAPSED, true).to_secret())
        .await
}
