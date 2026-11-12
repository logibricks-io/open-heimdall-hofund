//! Job-token refresh against the Qoder openapi host.

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
    if stored.fresh_at(host.clock().now()) {
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

/// Renews the job token even when the access token is not near expiry (401 retry).
pub(crate) async fn force(
    host: &HostContext,
    endpoints: Arc<Endpoints>,
    account: &AccountId,
) -> Result<(), PluginError> {
    let stored = Stored::load(host, account).await?;
    refresh_stale(host, endpoints, account, &stored).await
}

async fn refresh_stale(
    host: &HostContext,
    endpoints: Arc<Endpoints>,
    account: &AccountId,
    stored: &Stored,
) -> Result<(), PluginError> {
    if secret_text(&stored.credential.refresh_token).is_empty() {
        return Err(PluginError::auth(
            "Qoder sign-in has expired; import credentials again",
        ));
    }
    let lease = host.accounts().lease(account, LEASE_WAIT).await?;
    let blob = SecretObject::parse(lease.secret())?;
    let current_refresh = blob
        .secret("refresh_token")
        .ok_or_else(|| PluginError::auth("the stored Qoder sign-in has no refresh token"))?;
    if !same_secret(&current_refresh, &stored.credential.refresh_token) {
        return Ok(());
    }
    let url = format!(
        "{}/api/v1/jobToken/refresh",
        endpoints.openapi_for(stored.credential.site)
    );
    let body = hofund_plugin_sdk::json::json!({
        "refresh_token": secret_text(&stored.credential.refresh_token),
    })
    .to_string();
    let reply = Call::post(url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .bytes(body.into_bytes())
        .fetch(host.http())
        .await?;
    match reply.status() {
        200..=299 => {}
        401 | 403 => {
            let blob = SecretObject::parse(lease.secret())?;
            lease
                .commit(None, blob.with_flag(creds::LAPSED, true).to_secret())
                .await?;
            return Err(PluginError::auth(creds::LAPSED_REASON));
        }
        status => {
            return Err(PluginError::status_error(
                status,
                "Qoder did not refresh the job token",
            ));
        }
    }
    let root: Value = hofund_plugin_sdk::json::from_slice(reply.body())
        .map_err(|_| PluginError::decode("Qoder refresh returned invalid JSON"))?;
    let access = root
        .get("token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| PluginError::auth("Qoder refresh sent back no access token"))?;
    let refresh = root
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| PluginError::auth("Qoder refresh sent back no refresh token"))?;
    let expires = root
        .get("expires_at")
        .and_then(|value| value.as_i64().or_else(|| value.as_f64().map(|n| n as i64)))
        .unwrap_or_else(|| default_expires_at(host));
    let mut credential = stored.credential.clone();
    credential.token = SecretString::from(access.to_owned());
    credential.refresh_token = SecretString::from(refresh.to_owned());
    credential.expires_at = expires;
    lease
        .commit(
            Some(creds::label(&credential)),
            creds::secret_blob(&credential).to_secret(),
        )
        .await?;
    Ok(())
}

fn default_expires_at(host: &HostContext) -> i64 {
    let Ok(now) = host.clock().now().duration_since(std::time::UNIX_EPOCH) else {
        return 0;
    };
    now.as_millis() as i64 + 3_600_000
}
