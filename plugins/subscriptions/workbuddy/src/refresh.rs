//! OAuth refresh for browser-signed WorkBuddy accounts.

use std::sync::Arc;
use std::time::Duration;

use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::oauth::{SecretObject, same_secret, secret_text};

use crate::Endpoints;
use crate::creds::{self, Stored};

const LEASE_WAIT: Duration = Duration::from_secs(30);

pub(crate) async fn ensure_fresh(
    host: &HostContext,
    endpoints: Arc<Endpoints>,
    account: &AccountId,
) -> Result<Stored, PluginError> {
    let stored = Stored::load(host, account).await?;
    if stored.credential.desktop || stored.fresh_at(host.clock().now()) {
        return Ok(stored);
    }
    refresh_stale(host, endpoints, account, &stored).await?;
    Stored::load(host, account).await
}

pub(crate) async fn run(
    host: &HostContext,
    endpoints: Arc<Endpoints>,
    account: &AccountId,
) -> Result<(), PluginError> {
    let _ = ensure_fresh(host, endpoints, account).await?;
    Ok(())
}

async fn refresh_stale(
    host: &HostContext,
    endpoints: Arc<Endpoints>,
    account: &AccountId,
    stored: &Stored,
) -> Result<(), PluginError> {
    if secret_text(&stored.credential.refresh).is_empty() {
        return Err(PluginError::auth(
            "WorkBuddy sign-in has expired; sign in again",
        ));
    }
    let lease = host.accounts().lease(account, LEASE_WAIT).await?;
    let blob = SecretObject::parse(lease.secret())?;
    let current = blob
        .secret("refresh_token")
        .ok_or_else(|| PluginError::auth("the stored WorkBuddy sign-in has no refresh token"))?;
    if !same_secret(&current, &stored.credential.refresh) {
        return Ok(());
    }
    let base = endpoints.base_for(stored.credential.site);
    let url = format!("{base}/v2/plugin/auth/token/refresh");
    let reply = Call::post(url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .header("User-Agent", stored.credential.site.user_agent())
        .sensitive_header(
            "X-Refresh-Token",
            secret_text(&stored.credential.refresh).to_owned(),
        )
        .bytes(b"{}")
        .fetch(host.http())
        .await?;
    match reply.status() {
        200..=299 => {}
        401 | 403 => {
            return Err(PluginError::auth(
                "WorkBuddy refused the stored sign-in; sign in again",
            ));
        }
        status => {
            return Err(PluginError::status_error(
                status,
                "WorkBuddy did not refresh the access token",
            ));
        }
    }
    let root: Value = hofund_plugin_sdk::json::from_slice(reply.body())
        .map_err(|_| PluginError::decode("WorkBuddy refresh returned invalid JSON"))?;
    let access = root
        .get("accessToken")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| PluginError::auth("WorkBuddy refresh sent back no access token"))?;
    let mut credential = stored.credential.clone();
    credential.access = SecretString::from(access.to_owned());
    if let Some(refresh) = root.get("refreshToken").and_then(Value::as_str)
        && !refresh.is_empty()
    {
        credential.refresh = SecretString::from(refresh.to_owned());
    }
    if let Some(expires) = root.get("expiresAt").and_then(|v| v.as_i64()) {
        credential.expires_at = expires;
    }
    lease
        .commit(
            Some(credential.label.clone()),
            creds::secret_blob(&credential).to_secret(),
        )
        .await?;
    Ok(())
}
