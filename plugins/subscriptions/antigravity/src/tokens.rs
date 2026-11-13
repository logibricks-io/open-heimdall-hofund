//! Access tokens refresh in memory; a refused grant marks the stored row lapsed.

use std::time::{Duration, SystemTime};

use hofund_plugin_api::{AccountId, HostContext, PluginError, PluginErrorKind, SecretString};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::oauth::{
    SecretObject, TokenCache, detached, parse_token_set, refresh_secret_form, same_secret,
    token_error,
};

use crate::{Endpoints, client};

pub(crate) const LAPSED: &str = "lapsed";
pub(crate) const LAPSED_REASON: &str = "Google refused the stored sign-in; sign in again";
pub(crate) const REFRESH_MARGIN: Duration = Duration::from_secs(2 * 60);
const DEFAULT_EXPIRY: Duration = Duration::from_secs(60 * 60);
const LEASE_WAIT: Duration = Duration::from_secs(30);

pub(crate) async fn access(
    host: &HostContext,
    endpoints: &Endpoints,
    oauth: &client::OauthClient,
    cache: &TokenCache<SecretString>,
    account: &AccountId,
) -> Result<SecretString, PluginError> {
    cache
        .get(host.clock(), account, || {
            let host = host.clone();
            let token_url = endpoints.token.clone();
            let account = account.clone();
            let oauth = oauth.clone();
            async move {
                let job = refresh(host.clone(), token_url, oauth, account);
                detached(host.scheduler(), "antigravity-refresh", job).await
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

pub(crate) fn form(refresh_token: &SecretString, oauth: &client::OauthClient) -> String {
    refresh_secret_form(oauth.id(), oauth.secret(), refresh_token)
}

async fn refresh(
    host: HostContext,
    token_url: String,
    oauth: client::OauthClient,
    account: AccountId,
) -> Result<(SecretString, Option<SystemTime>), PluginError> {
    let secret =
        host.accounts().secret(&account).await?.ok_or_else(|| {
            PluginError::auth("no Antigravity sign-in is stored for this account")
        })?;
    let refresh_token = stored(&secret)?
        .secret("refresh_token")
        .ok_or_else(|| PluginError::auth("the stored Antigravity sign-in has no refresh token"))?;
    let reply = Call::post(token_url)
        .header("content-type", "application/x-www-form-urlencoded")
        .bytes(form(&refresh_token, &oauth))
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
