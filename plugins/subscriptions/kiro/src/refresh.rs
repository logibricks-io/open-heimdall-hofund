//! Refresh Kiro sign-ins through the desktop or OIDC refresh endpoints.

use std::sync::Arc;
use std::time::Duration;

use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::{self, Value};
use hofund_plugin_sdk::oauth::{SecretObject, same_secret, secret_text};

use crate::Endpoints;
use crate::creds::{self, Stored};

const LEASE_WAIT: Duration = Duration::from_secs(30);

fn method_is(method: &str, tag: &str) -> bool {
    method.eq_ignore_ascii_case(tag)
}

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
    if method_is(&stored.credential.method, "apikey") {
        return Ok(());
    }
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
    let root =
        if method_is(&stored.credential.method, "idc") && !stored.credential.client_id.is_empty() {
            refresh_idc(host, stored).await?
        } else if method_is(&stored.credential.method, "external-idp") {
            return Err(PluginError::config(
                "Kiro external-idp refresh is not implemented offline",
            ));
        } else {
            refresh_social(host, stored, endpoints).await?
        };
    let access = root
        .get("accessToken")
        .or_else(|| root.get("access_token"))
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| PluginError::auth("Kiro refresh sent back no access token"))?;
    let refresh = root
        .get("refreshToken")
        .or_else(|| root.get("refresh_token"))
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .map(SecretString::from)
        .unwrap_or_else(|| stored.credential.refresh.clone());
    let expires_in = root
        .get("expiresIn")
        .or_else(|| root.get("expires_in"))
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
    if !credential.ide_token_path.is_empty() {
        let path = std::path::Path::new(&credential.ide_token_path);
        let _ = creds::write_token_file(path, &credential);
    }
    Ok(())
}

async fn refresh_social(
    host: &HostContext,
    stored: &Stored,
    endpoints: &Endpoints,
) -> Result<Value, PluginError> {
    let url = endpoints.refresh_url(&stored.credential.region);
    let body = json::json!({"refreshToken": secret_text(&stored.credential.refresh)});
    let reply = Call::post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .header("user-agent", "Kiro-Desktop/0.2.13 (darwin; arm64)")
        .bytes(json::to_vec(&body).expect("json"))
        .fetch(host.http())
        .await?;
    refresh_status(reply.status(), reply.body())
}

async fn refresh_idc(host: &HostContext, stored: &Stored) -> Result<Value, PluginError> {
    let url = format!(
        "https://oidc.{}.amazonaws.com/token",
        stored.credential.region
    );
    let body = json::json!({
        "clientId": stored.credential.client_id,
        "clientSecret": secret_text(&stored.credential.client_secret),
        "refreshToken": secret_text(&stored.credential.refresh),
        "grantType": "refresh_token",
    });
    let reply = Call::post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .bytes(json::to_vec(&body).expect("json"))
        .fetch(host.http())
        .await?;
    refresh_status(reply.status(), reply.body())
}

fn refresh_status(status: u16, body: &[u8]) -> Result<Value, PluginError> {
    if status == 401 || status == 403 {
        return Err(PluginError::auth(
            "Kiro refused to refresh the sign-in; sign in again",
        ));
    }
    if !(200..300).contains(&status) {
        return Err(PluginError::status_error(
            status,
            "Kiro did not refresh the sign-in",
        ));
    }
    json::from_slice(body).map_err(|_| PluginError::decode("Kiro refresh reply is not JSON"))
}
