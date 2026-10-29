//! The access token: read it, and refresh it under the account lease.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hofund_plugin_api::{AccountId, HostContext, PluginError, PluginErrorKind, SecretString};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::oauth::{
    SecretObject, detached, parse_token_set, same_secret, token_error, unverified_claims,
};

use crate::{CLIENT_ID, Endpoints, REFRESH_SCOPE};

/// The secret field set once ChatGPT refuses the refresh token.
pub(crate) const LAPSED: &str = "lapsed";

/// Why a lapsed account needs a new sign-in.
pub(crate) const LAPSED_REASON: &str = "ChatGPT refused the stored sign-in; sign in again";

/// How close to expiry an access token is refreshed, as Codex CLI does.
pub(crate) const REFRESH_MARGIN: Duration = Duration::from_secs(5 * 60);

/// How long a refresh waits for another holder of the account lease.
const LEASE_WAIT: Duration = Duration::from_secs(30);

/// The token to send and the ChatGPT account it belongs to.
#[derive(Debug)]
pub(crate) struct Access {
    pub(crate) token: SecretString,
    pub(crate) account: Option<String>,
}

/// When the stored access token is replaced.
pub(crate) enum Need {
    /// Only when it is within [`REFRESH_MARGIN`] of expiry.
    IfExpiring,
    /// When it is still this token, which the backend called expired.
    Replace(SecretString),
    /// Always: an explicit refresh.
    Always,
}

/// Seconds since the Unix epoch, saturating at zero.
pub(crate) fn unix_seconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Says `token` is far enough from expiry to send. A token without a
/// readable `exp` counts as fresh, as in Codex CLI.
pub(crate) fn fresh(token: &SecretString, now: SystemTime) -> bool {
    let exp = unverified_claims(token)
        .ok()
        .and_then(|claims| claims.get("exp").and_then(|exp| exp.as_u64()))
        .unwrap_or(0);
    if exp == 0 {
        return true;
    }
    UNIX_EPOCH + Duration::from_secs(exp) > now + REFRESH_MARGIN
}

fn stored(secret: &SecretString) -> Result<SecretObject, PluginError> {
    let blob = SecretObject::parse(secret)?;
    if blob.flag(LAPSED) {
        return Err(PluginError::auth(LAPSED_REASON));
    }
    Ok(blob)
}

fn access_of(blob: &SecretObject) -> Result<SecretString, PluginError> {
    blob.secret("access_token")
        .ok_or_else(|| PluginError::auth("the stored ChatGPT sign-in has no access token"))
}

fn account_of(blob: &SecretObject) -> Option<String> {
    blob.text("account_id").map(str::to_owned)
}

/// The access token to send for `account`, refreshed first when `need`
/// asks for it.
pub(crate) async fn access(
    host: &HostContext,
    endpoints: &Endpoints,
    account: &AccountId,
    need: Need,
) -> Result<Access, PluginError> {
    let secret = host
        .accounts()
        .secret(account)
        .await?
        .ok_or_else(|| PluginError::auth("no ChatGPT sign-in is stored for this account"))?;
    let blob = stored(&secret)?;
    let token = access_of(&blob)?;
    let refresh = match &need {
        Need::IfExpiring => !fresh(&token, host.clock().now()),
        Need::Replace(stale) => same_secret(&token, stale),
        Need::Always => true,
    };
    if !refresh {
        return Ok(Access {
            token,
            account: account_of(&blob),
        });
    }
    // Run detached: a rotated refresh token must be stored even when the
    // request that needed it is cancelled.
    let job = rotate(
        host.clone(),
        endpoints.token.clone(),
        account.clone(),
        token,
    );
    detached(host.scheduler(), "codex-refresh", job).await
}

/// [`rotate`] for the unit tests.
#[cfg(test)]
pub(crate) async fn rotate_for_test(
    host: HostContext,
    token_url: String,
    account: AccountId,
    stale: SecretString,
) -> Result<Access, PluginError> {
    rotate(host, token_url, account, stale).await
}

/// Refreshes `account` unless another caller already replaced `stale`.
async fn rotate(
    host: HostContext,
    token_url: String,
    account: AccountId,
    stale: SecretString,
) -> Result<Access, PluginError> {
    let lease = host.accounts().lease(&account, LEASE_WAIT).await?;
    let blob = stored(lease.secret())?;
    let current = access_of(&blob)?;
    if !same_secret(&current, &stale) {
        // Another caller refreshed while this one waited for the lease.
        return Ok(Access {
            token: current,
            account: account_of(&blob),
        });
    }
    let refresh_token = blob
        .secret("refresh_token")
        .ok_or_else(|| PluginError::auth("the stored ChatGPT sign-in has no refresh token"))?;
    let body = SecretObject::new()
        .with_text("client_id", CLIENT_ID)
        .with_text("grant_type", "refresh_token")
        .with_secret("refresh_token", &refresh_token)
        .with_text("scope", REFRESH_SCOPE)
        .to_body();
    let reply = Call::post(token_url).json(body).fetch(host.http()).await?;
    if !(200..300).contains(&reply.status()) {
        let error = token_error(reply.status(), reply.body());
        if error.kind() == PluginErrorKind::Auth {
            lease
                .commit(None, blob.with_flag(LAPSED, true).to_secret())
                .await?;
        }
        return Err(error);
    }
    let now = host.clock().now();
    let set = parse_token_set(reply.body(), now)
        .map_err(|error| PluginError::decode(error.message()))?
        .or_refresh(Some(&refresh_token));
    let account_id = account_of(&blob);
    let mut next = blob
        .with_secret("access_token", set.access())
        .with_number("last_refresh", unix_seconds(now));
    if let Some(refresh) = set.refresh() {
        next = next.with_secret("refresh_token", refresh);
    }
    if let Some(id_token) = set.id_token() {
        next = next.with_secret("id_token", id_token);
    }
    lease.commit(None, next.to_secret()).await?;
    Ok(Access {
        token: set.access().clone(),
        account: account_id,
    })
}
