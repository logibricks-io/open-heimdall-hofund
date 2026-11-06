//! The access token: read it, and refresh it under the account lease.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::oauth::{SecretObject, detached, same_secret, unverified_claims};

/// The secret field set once ChatGPT refuses the refresh token.
pub(crate) const LAPSED: &str = "lapsed";

/// Why a lapsed account needs a new sign-in.
pub(crate) const LAPSED_REASON: &str = "ChatGPT refused the stored sign-in; sign in again";

/// Adopted or repaired access is still inside the refresh margin; caller should retry refresh.
pub(crate) const REFRESH_NEEDED: &str =
    "ChatGPT access token still needs refresh after Codex store sync";

/// How close to expiry an access token is refreshed, as Codex CLI does.
pub(crate) const REFRESH_MARGIN: Duration = Duration::from_secs(5 * 60);

/// The token to send and the ChatGPT account it belongs to.
#[derive(Debug)]
pub(crate) struct Access {
    pub(crate) token: SecretString,
    pub(crate) account: Option<String>,
}

/// When the stored access token is replaced.
#[derive(Clone)]
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

pub(crate) fn stored(secret: &SecretString) -> Result<SecretObject, PluginError> {
    let blob = SecretObject::parse(secret)?;
    if blob.flag(LAPSED) {
        return Err(PluginError::auth(LAPSED_REASON));
    }
    Ok(blob)
}

pub(crate) fn access_of(blob: &SecretObject) -> Result<SecretString, PluginError> {
    blob.secret("access_token")
        .ok_or_else(|| PluginError::auth("the stored ChatGPT sign-in has no access token"))
}

pub(crate) fn account_of(blob: &SecretObject) -> Option<String> {
    blob.text("account_id").map(str::to_owned)
}

/// The access token to send for `account`, refreshed first when `need`
/// asks for it.
pub(crate) async fn access(
    host: &HostContext,
    endpoints: &Endpoints,
    account: &AccountId,
    need: Need,
    skip_keychain: bool,
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
    let stale = token.clone();
    let job = reconcile_rotate(
        host.clone(),
        endpoints.token.clone(),
        account.clone(),
        stale,
        need,
        skip_keychain,
        None,
    );
    detached(host.scheduler(), "codex-refresh", job).await
}

/// [`reconcile_rotate`] for the unit tests.
#[cfg(test)]
pub(crate) async fn rotate_for_test(
    host: HostContext,
    token_url: String,
    account: AccountId,
    stale: SecretString,
    skip_keychain: bool,
) -> Result<Access, PluginError> {
    reconcile_rotate(
        host,
        token_url,
        account,
        stale,
        Need::Always,
        skip_keychain,
        None,
    )
    .await
}

async fn reconcile_rotate(
    host: HostContext,
    token_url: String,
    account: AccountId,
    stale: SecretString,
    need: Need,
    skip_keychain: bool,
    fake: Option<&crate::store::FakeKeychain>,
) -> Result<Access, PluginError> {
    crate::reconcile_rotate::reconcile_rotate(
        host,
        token_url,
        account,
        stale,
        need,
        skip_keychain,
        fake,
    )
    .await
}

use crate::Endpoints;
