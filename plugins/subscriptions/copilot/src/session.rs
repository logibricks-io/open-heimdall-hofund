//! The stored GitHub sign-in, and the short-lived Copilot session token
//! traded for it.

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::{self, Value};
use hofund_plugin_sdk::oauth::{SecretObject, TokenCache, same_secret, secret_text};

use crate::site::{GITHUB, Site};

/// The blob flag set when GitHub refuses the stored token.
pub(crate) const LAPSED: &str = "lapsed";

/// Why an account with a refused token needs sign-in.
pub(crate) const LAPSED_REASON: &str = "GitHub refused the stored Copilot sign-in; sign in again";

/// A session token is traded again when less than this is left.
pub(crate) const MARGIN: Duration = Duration::from_secs(120);

/// How long marking a sign-in lapsed waits for the account lease.
const LEASE_WAIT: Duration = Duration::from_secs(30);

/// The editor identity Copilot expects on every call, as VS Code sends it.
pub(crate) const EDITOR_HEADERS: [(&str, &str); 4] = [
    ("Editor-Version", "vscode/1.140.0"),
    ("Editor-Plugin-Version", "copilot-chat/0.68.0"),
    ("Copilot-Integration-Id", "vscode-chat"),
    ("User-Agent", "GitHubCopilotChat/0.68.0"),
];

/// A Copilot session token and the API base it is good for.
#[derive(Clone)]
pub(crate) struct Session {
    pub(crate) token: SecretString,
    pub(crate) base: String,
}

impl fmt::Debug for Session {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Session")
            .field("token", &"<redacted>")
            .field("base", &self.base)
            .finish()
    }
}

/// The GitHub sign-in stored for an account.
pub(crate) struct Stored {
    pub(crate) oauth: SecretString,
    pub(crate) site: Site,
}

/// The `Authorization` value for a GitHub OAuth token: the `token` scheme,
/// as the Copilot token and user endpoints expect.
pub(crate) fn github_authorization(token: &SecretString) -> String {
    format!("token {}", secret_text(token))
}

/// A GET to a GitHub endpoint with the OAuth token and the editor headers.
pub(crate) fn github_get(url: String, oauth: &SecretString) -> Call {
    EDITOR_HEADERS.iter().fold(
        Call::get(url)
            .sensitive_header("authorization", github_authorization(oauth))
            .header("accept", "application/json"),
        |call, (name, value)| call.header(*name, *value),
    )
}

/// Reads the stored sign-in of `account`.
pub(crate) async fn stored(host: &HostContext, account: &AccountId) -> Result<Stored, PluginError> {
    let secret = host
        .accounts()
        .secret(account)
        .await?
        .ok_or_else(|| PluginError::auth("no Copilot sign-in is stored for this account"))?;
    read_blob(&secret)
}

/// Reads a stored blob. A lapsed, unreadable, or incomplete one is `Auth`.
pub(crate) fn read_blob(secret: &SecretString) -> Result<Stored, PluginError> {
    let blob = SecretObject::parse(secret)
        .map_err(|_| PluginError::auth("the stored Copilot sign-in is unreadable"))?;
    if blob.flag(LAPSED) {
        return Err(PluginError::auth(LAPSED_REASON));
    }
    let oauth = blob
        .secret("oauth_token")
        .ok_or_else(|| PluginError::auth("the stored Copilot sign-in has no token"))?;
    let site = Site::parse(blob.text("host").unwrap_or(GITHUB))
        .map_err(|_| PluginError::auth("the stored Copilot sign-in names an unknown host"))?;
    Ok(Stored { oauth, site })
}

/// The cached session of `account`, traded again within [`MARGIN`] of its
/// expiry. Concurrent callers share one trade.
pub(crate) async fn session(
    cache: &TokenCache<Session>,
    host: &HostContext,
    account: &AccountId,
) -> Result<Session, PluginError> {
    cache
        .get(host.clock(), account, || trade(host, account))
        .await
}

async fn trade(
    host: &HostContext,
    account: &AccountId,
) -> Result<(Session, Option<SystemTime>), PluginError> {
    let stored = stored(host, account).await?;
    let reply = github_get(stored.site.session_url(), &stored.oauth)
        .fetch(host.http())
        .await?;
    match reply.status() {
        200..=299 => parse_session(&stored.site, reply.body()),
        401 | 403 => {
            lapse(host, account, &stored.oauth).await?;
            Err(PluginError::auth(LAPSED_REASON))
        }
        status => Err(PluginError::status_error(
            status,
            "GitHub did not issue a Copilot session token",
        )),
    }
}

/// Reads a session token reply: `token`, `expires_at` in Unix seconds, and
/// `endpoints.api`, which must be a trusted origin for `site`.
pub(crate) fn parse_session(
    site: &Site,
    body: &[u8],
) -> Result<(Session, Option<SystemTime>), PluginError> {
    let Ok(Value::Object(object)) = json::from_slice::<Value>(body) else {
        return Err(PluginError::decode(
            "the Copilot session token reply is not a JSON object",
        ));
    };
    let token = object
        .get("token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| PluginError::auth("GitHub returned no Copilot session token"))?;
    // A missing expiry reads as already expired, so the next call trades again.
    let expires_at = object
        .get("expires_at")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let api = object
        .get("endpoints")
        .and_then(|endpoints| endpoints.get("api"))
        .and_then(Value::as_str)
        .filter(|api| !api.is_empty());
    let base = match api {
        Some(api) => site.trusted_api(api).ok_or_else(|| {
            PluginError::auth(
                "Copilot named an API host outside GitHub; the token is not sent there",
            )
        })?,
        None => site.api_base(),
    };
    Ok((
        Session {
            token: SecretString::from(token.to_owned()),
            base,
        },
        Some(UNIX_EPOCH + Duration::from_secs(expires_at)),
    ))
}

/// Marks the stored sign-in lapsed, unless another sign-in replaced the
/// token that was refused.
pub(crate) async fn lapse(
    host: &HostContext,
    account: &AccountId,
    refused: &SecretString,
) -> Result<(), PluginError> {
    let lease = host.accounts().lease(account, LEASE_WAIT).await?;
    let Ok(blob) = SecretObject::parse(lease.secret()) else {
        return Ok(());
    };
    let same = blob
        .secret("oauth_token")
        .is_some_and(|token| same_secret(&token, refused));
    if !same {
        return Ok(());
    }
    lease
        .commit(None, blob.with_flag(LAPSED, true).to_secret())
        .await
}
