//! Job-token and CN device-token refresh on the account's own hosts.

use std::sync::Arc;
use std::time::Duration;

use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::oauth::{SecretObject, same_secret, secret_text};

use crate::creds::{self, Stored};
use crate::{Endpoints, sign_in};

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

pub(crate) async fn force(
    host: &HostContext,
    endpoints: Arc<Endpoints>,
    account: &AccountId,
) -> Result<Stored, PluginError> {
    let stored = Stored::load(host, account).await?;
    refresh_stale(host, endpoints, account, &stored).await?;
    Stored::load(host, account).await
}

pub(crate) async fn force_device(
    host: &HostContext,
    endpoints: Arc<Endpoints>,
    account: &AccountId,
) -> Result<Stored, PluginError> {
    let stored = Stored::load(host, account).await?;
    if secret_text(&stored.credential.device_refresh_token).is_empty() {
        return Err(PluginError::auth(
            "Qoder account-page sign-in has expired; sign in again to see usage",
        ));
    }
    let site = endpoints.resolve(&stored.credential)?;
    let lease = host.accounts().lease(account, LEASE_WAIT).await?;
    let blob = SecretObject::parse(lease.secret())?;
    let current = blob
        .secret("device_refresh_token")
        .ok_or_else(|| PluginError::auth("the stored Qoder sign-in has no device refresh token"))?;
    if !same_secret(&current, &stored.credential.device_refresh_token) {
        return Stored::load(host, account).await;
    }
    let body = hofund_plugin_sdk::json::json!({
        "refresh_token": secret_text(&stored.credential.device_refresh_token),
    })
    .to_string();
    let reply = Call::post(format!(
        "{}/api/v1/deviceToken/refresh",
        site.openapi.trim_end_matches('/')
    ))
    .header("Accept", "application/json")
    .header("Content-Type", "application/json")
    .bytes(body)
    .fetch(host.http())
    .await?;
    if reply.status() == 401 || reply.status() == 403 {
        return Err(PluginError::status_error(
            reply.status(),
            "Qoder refused the account-page sign-in; chat is unchanged",
        ));
    }
    if reply.status() != 200 {
        return Err(PluginError::status_error(
            reply.status(),
            "Qoder did not refresh the device token",
        ));
    }
    let root: Value = hofund_plugin_sdk::json::from_slice(reply.body())
        .map_err(|_| PluginError::decode("Qoder device refresh returned invalid JSON"))?;
    let access = sign_in::refresh_token(&root)?;
    let refresh = sign_in::refresh_secret(&root)?;
    let mut credential = stored.credential.clone();
    credential.device_token = access.clone();
    credential.device_refresh_token = refresh.clone();
    if credential.device_chat {
        credential.token = access;
        credential.refresh_token = refresh;
        credential.expires_at = sign_in::device_expiry(host, &root);
    }
    lease
        .commit(
            Some(creds::label(&credential)),
            creds::secret_blob(&credential).to_secret(),
        )
        .await?;
    Stored::load(host, account).await
}

async fn refresh_stale(
    host: &HostContext,
    endpoints: Arc<Endpoints>,
    account: &AccountId,
    stored: &Stored,
) -> Result<(), PluginError> {
    if secret_text(&stored.credential.refresh_token).is_empty() {
        return Err(PluginError::auth(
            "Qoder sign-in has expired; sign in again",
        ));
    }
    let site = endpoints.resolve(&stored.credential)?;
    let lease = host.accounts().lease(account, LEASE_WAIT).await?;
    let blob = SecretObject::parse(lease.secret())?;
    let current_refresh = blob
        .secret("refresh_token")
        .ok_or_else(|| PluginError::auth("the stored Qoder sign-in has no refresh token"))?;
    if !same_secret(&current_refresh, &stored.credential.refresh_token) {
        return Ok(());
    }
    let path = if stored.credential.device_chat {
        "/api/v1/deviceToken/refresh"
    } else {
        "/api/v1/jobToken/refresh"
    };
    let body = hofund_plugin_sdk::json::json!({
        "refresh_token": secret_text(&stored.credential.refresh_token),
    })
    .to_string();
    let reply = Call::post(format!("{}{path}", site.openapi.trim_end_matches('/')))
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .bytes(body)
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
                "Qoder did not refresh the sign-in token",
            ));
        }
    }
    let root: Value = hofund_plugin_sdk::json::from_slice(reply.body())
        .map_err(|_| PluginError::decode("Qoder refresh returned invalid JSON"))?;
    let access = if stored.credential.device_chat {
        sign_in::refresh_token(&root)?
    } else {
        root.get("token")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(|value| SecretString::from(value.to_owned()))
            .ok_or_else(|| PluginError::auth("Qoder refresh sent back no access token"))?
    };
    let refresh = sign_in::refresh_secret(&root)?;
    let expires_at = if stored.credential.device_chat {
        sign_in::device_expiry(host, &root)
    } else {
        sign_in::job_expiry(host, &root)
    };
    let mut credential = stored.credential.clone();
    credential.token = access.clone();
    credential.refresh_token = refresh.clone();
    credential.expires_at = expires_at;
    if credential.device_chat {
        credential.device_token = access;
        credential.device_refresh_token = refresh;
    }
    lease
        .commit(
            Some(creds::label(&credential)),
            creds::secret_blob(&credential).to_secret(),
        )
        .await?;
    Ok(())
}
