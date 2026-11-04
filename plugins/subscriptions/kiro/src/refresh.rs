//! Refresh Kiro social sign-ins through the desktop refresh endpoint.

use std::sync::Arc;
use std::time::Duration;

use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::{self, Value};
use hofund_plugin_sdk::oauth::{SecretObject, same_secret, secret_text};

use crate::Endpoints;
use crate::creds::{self, Stored};

const LEASE_WAIT: Duration = Duration::from_secs(30);

pub(crate) async fn ensure_fresh(
    host: &HostContext,
    account: &AccountId,
    endpoints: Arc<Endpoints>,
) -> Result<Stored, PluginError> {
    let stored = Stored::load(host, account).await?;
    if stored.fresh_at(host.clock().now()) {
        return Ok(stored);
    }
    refresh_stale(host, account, &stored, &endpoints).await?;
    Stored::load(host, account).await
}

pub(crate) async fn refresh_stale(
    host: &HostContext,
    account: &AccountId,
    stored: &Stored,
    endpoints: &Endpoints,
) -> Result<(), PluginError> {
    if secret_text(&stored.credential.refresh).is_empty() {
        return Err(PluginError::auth(
            "Kiro sign-in has expired; sign in again with the Kiro IDE or kiro-cli",
        ));
    }
    let lease = host.accounts().lease(account, LEASE_WAIT).await?;
    let blob = SecretObject::parse(lease.secret())?;
    let current_refresh = blob
        .secret("refresh_token")
        .ok_or_else(|| PluginError::auth("the stored Kiro sign-in has no refresh token"))?;
    if !same_secret(&current_refresh, &stored.credential.refresh) {
        return Ok(());
    }
    let url = endpoints.refresh_url(&stored.credential.region);
    let body = json::json!({"refreshToken": secret_text(&stored.credential.refresh)});
    let reply = Call::post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .header("user-agent", "Kiro-Desktop/0.2.13 (darwin; arm64)")
        .bytes(json::to_vec(&body).expect("json"))
        .fetch(host.http())
        .await?;
    if reply.status() == 401 || reply.status() == 403 {
        return Err(PluginError::auth(
            "Kiro refused to refresh the sign-in; sign in again",
        ));
    }
    if !(200..300).contains(&reply.status()) {
        return Err(PluginError::status_error(
            reply.status(),
            "Kiro did not refresh the sign-in",
        ));
    }
    let root: Value = json::from_slice(reply.body())
        .map_err(|_| PluginError::decode("Kiro refresh reply is not JSON"))?;
    let access = root
        .get("accessToken")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| PluginError::auth("Kiro refresh sent back no access token"))?;
    let refresh = root
        .get("refreshToken")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .map(SecretString::from)
        .unwrap_or_else(|| stored.credential.refresh.clone());
    let expires_in = root
        .get("expiresIn")
        .and_then(Value::as_u64)
        .unwrap_or(3600);
    let profile = root
        .get("profileArn")
        .and_then(Value::as_str)
        .unwrap_or(stored.credential.profile_arn.as_str());
    let mut credential = stored.credential.clone();
    credential.access = SecretString::from(access.to_owned());
    credential.refresh = refresh;
    credential.profile_arn = profile.to_owned();
    credential.expires_at = host
        .clock()
        .now()
        .checked_add(Duration::from_secs(expires_in));
    lease
        .commit(
            Some(credential.label.clone()),
            creds::secret_blob(&credential).to_secret(),
        )
        .await?;
    Ok(())
}
