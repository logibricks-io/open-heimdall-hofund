//! Stored account session material.

use std::path::Path;
use std::time::SystemTime;

use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::oauth::SecretObject;

use crate::creds::{self, PlanAuth};
use crate::team;

pub(crate) struct Stored {
    pub auth: PlanAuth,
    pub api_key: SecretString,
}

impl Stored {
    pub(crate) fn ready(&self, _now: SystemTime) -> bool {
        self.auth.api_key.is_some() || self.auth.team()
    }
}

pub(crate) async fn load(
    host: &HostContext,
    endpoints: &crate::Endpoints,
    account: &AccountId,
    device_file: &Path,
) -> Result<Stored, PluginError> {
    let Some(secret) = host.accounts().secret(account).await? else {
        return Err(PluginError::auth(
            "no Z Code sign-in is stored for this account",
        ));
    };
    parse_loaded(host, endpoints, &secret, device_file).await
}

pub(crate) async fn parse_loaded(
    host: &HostContext,
    endpoints: &crate::Endpoints,
    secret: &SecretString,
    device_file: &Path,
) -> Result<Stored, PluginError> {
    let _ = SecretObject::parse(secret)?;
    let auth = creds::parse_secret(secret)?;
    let api_key = if auth.team() && auth.api_key.is_none() {
        team::resolve_project_key(host, endpoints, &auth, device_file).await?
    } else {
        auth.api_key.clone().ok_or_else(|| {
            PluginError::auth("the stored Z Code sign-in has no GLM Coding Plan key")
        })?
    };
    Ok(Stored { auth, api_key })
}
