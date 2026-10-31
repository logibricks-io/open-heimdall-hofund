//! The access token: refreshed on demand and held in memory only.
//!
//! Google does not rotate the refresh token, so a refresh never writes the
//! stored secret. Only a refused refresh does, to mark the sign-in lapsed.

use std::time::{Duration, SystemTime};

use hofund_plugin_api::{AccountId, HostContext, PluginError, PluginErrorKind, SecretString};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::oauth::{
    SecretObject, TokenCache, detached, parse_token_set, refresh_secret_form, same_secret,
    token_error,
};

use crate::{CLIENT_ID, CLIENT_SECRET, Endpoints};

/// The flag a refused refresh sets on the stored secret.
pub(crate) const LAPSED: &str = "lapsed";

/// What a lapsed sign-in is told.
pub(crate) const LAPSED_REASON: &str = "Google refused the stored sign-in; sign in again";

/// How long before expiry a token is refreshed, as Gemini CLI does.
pub(crate) const REFRESH_MARGIN: Duration = Duration::from_secs(2 * 60);

/// How long an access token lasts when the reply omits `expires_in`.
const DEFAULT_EXPIRY: Duration = Duration::from_secs(60 * 60);

const LEASE_WAIT: Duration = Duration::from_secs(30);

/// The account's access token, refreshed when it is within the margin of
/// expiry. Concurrent callers share one refresh.
pub(crate) async fn access(
    host: &HostContext,
    endpoints: &Endpoints,
    cache: &TokenCache<SecretString>,
    account: &AccountId,
) -> Result<SecretString, PluginError> {
    cache
        .get(host.clock(), account, || {
            let host = host.clone();
            let token_url = endpoints.token.clone();
            let account = account.clone();
            async move {
                // A refused refresh must mark the sign-in lapsed even when
                // the caller that started it is cancelled.
                let job = refresh(host.clone(), token_url, account);
                detached(host.scheduler(), "gemini-refresh", job).await
            }
        })
        .await
}

fn stored(secret: &SecretString) -> Result<SecretObject, PluginError> {
    let blob = SecretObject::parse(secret)?;
    if blob.flag(LAPSED) {
        return Err(PluginError::auth(LAPSED_REASON));
    }
    Ok(blob)
}

/// The refresh request body: Gemini CLI's client and its secret.
pub(crate) fn form(refresh_token: &SecretString) -> String {
    refresh_secret_form(CLIENT_ID, CLIENT_SECRET, refresh_token)
}

async fn refresh(
    host: HostContext,
    token_url: String,
    account: AccountId,
) -> Result<(SecretString, Option<SystemTime>), PluginError> {
    let secret = host
        .accounts()
        .secret(&account)
        .await?
        .ok_or_else(|| PluginError::auth("no Gemini CLI sign-in is stored for this account"))?;
    let refresh_token = stored(&secret)?
        .secret("refresh_token")
        .ok_or_else(|| PluginError::auth("the stored Gemini CLI sign-in has no refresh token"))?;
    let reply = Call::post(token_url)
        .header("content-type", "application/x-www-form-urlencoded")
        .bytes(form(&refresh_token))
        .fetch(host.http())
        .await?;
    if !(200..300).contains(&reply.status()) {
        let error = token_error(reply.status(), reply.body());
        if error.kind() == PluginErrorKind::Auth {
            mark_lapsed(&host, &account, &refresh_token).await?;
        }
        return Err(error);
    }
    let now = host.clock().now();
    let set =
        parse_token_set(reply.body(), now).map_err(|error| PluginError::decode(error.message()))?;
    let expires_at = Some(set.expires_at().unwrap_or(now + DEFAULT_EXPIRY));
    Ok((set.access().clone(), expires_at))
}

/// [`refresh`] for the unit tests.
#[cfg(test)]
pub(crate) async fn refresh_for_test(
    host: HostContext,
    token_url: String,
    account: AccountId,
) -> Result<(SecretString, Option<SystemTime>), PluginError> {
    refresh(host, token_url, account).await
}

/// Marks the sign-in lapsed, unless a new sign-in replaced the refresh
/// token that was refused.
pub(crate) async fn mark_lapsed(
    host: &HostContext,
    account: &AccountId,
    refused: &SecretString,
) -> Result<(), PluginError> {
    let lease = host.accounts().lease(account, LEASE_WAIT).await?;
    let blob = SecretObject::parse(lease.secret())?;
    let same = blob
        .secret("refresh_token")
        .is_some_and(|current| same_secret(&current, refused));
    if same {
        lease
            .commit(None, blob.with_flag(LAPSED, true).to_secret())
            .await?;
    }
    Ok(())
}
