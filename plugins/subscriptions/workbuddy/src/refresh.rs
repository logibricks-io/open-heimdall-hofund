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
    if stored.credential.desktop {
        return Ok(stored);
    }
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

/// Renews OAuth tokens even when the access token is not near expiry (401 retry).
pub(crate) async fn force(
    host: &HostContext,
    endpoints: Arc<Endpoints>,
    account: &AccountId,
) -> Result<(), PluginError> {
    let stored = Stored::load(host, account).await?;
    if stored.credential.desktop {
        return Ok(());
    }
    refresh_stale(host, endpoints, account, &stored).await?;
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
    let mut call = Call::post(url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .header("User-Agent", stored.credential.site.user_agent())
        .sensitive_header(
            "X-Refresh-Token",
            secret_text(&stored.credential.refresh).to_owned(),
        )
        .header("X-Auth-Refresh-Source", "plugin")
        .header("X-Domain", creds::domain(&stored.credential, base));
    if !stored.credential.enterprise_id.is_empty() {
        call = call
            .header("X-Enterprise-Id", &stored.credential.enterprise_id)
            .header("X-Tenant-Id", &stored.credential.enterprise_id);
    }
    let reply = call.bytes(b"{}").fetch(host.http()).await?;
    match reply.status() {
        200..=299 => {}
        401 => {
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
    let code = root.get("code").and_then(json_i64).unwrap_or(0);
    if code != 0 {
        return Err(PluginError::auth(format!(
            "WorkBuddy refused the stored sign-in (code {code}); sign in again"
        )));
    }
    let data = root.get("data").unwrap_or(&root);
    let access = data
        .get("accessToken")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| PluginError::auth("WorkBuddy refresh sent back no access token"))?;
    let mut credential = stored.credential.clone();
    credential.access = SecretString::from(access.to_owned());
    if let Some(refresh) = data.get("refreshToken").and_then(Value::as_str)
        && !refresh.is_empty()
    {
        credential.refresh = SecretString::from(refresh.to_owned());
    }
    let now_ms = host
        .clock()
        .now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0);
    credential.expires_at =
        expiry(data, "expiresAt", "expiresIn", now_ms).unwrap_or(credential.expires_at);
    credential.refresh_expires_at = expiry(data, "refreshExpiresAt", "refreshExpiresIn", now_ms)
        .unwrap_or(credential.refresh_expires_at);
    if let Some(domain) = data.get("domain").and_then(Value::as_str) {
        credential.domain = domain.to_owned();
    }
    if let Some(token_type) = data.get("tokenType").and_then(Value::as_str) {
        credential.token_type = token_type.to_owned();
    }
    lease
        .commit(
            Some(credential.label.clone()),
            creds::secret_blob(&credential).to_secret(),
        )
        .await?;
    Ok(())
}

fn expiry(data: &Value, absolute: &str, relative: &str, now_ms: i64) -> Option<i64> {
    data.get(absolute)
        .and_then(json_i64)
        .filter(|value| *value > 0)
        .or_else(|| {
            data.get(relative)
                .and_then(json_i64)
                .filter(|value| *value > 0)
                .map(|value| now_ms + value.saturating_mul(1_000))
        })
}

fn json_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().map(|number| number as i64))
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
}
