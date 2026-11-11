//! The stored GitHub sign-in, and the short-lived Copilot session token
//! traded for it.

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::{self, Value};
use hofund_plugin_sdk::oauth::{SecretObject, TokenCache, same_secret, secret_text};

use crate::discover::{self, Source, SourceKind};
use crate::quota::Entitlement;
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

/// The standalone CLI identity, whose token is sent directly.
pub(crate) const CLI_HEADERS: [(&str, &str); 4] = [
    ("Editor-Version", "copilot/1.0.88"),
    ("Copilot-Integration-Id", "copilot-developer-cli"),
    ("X-GitHub-Api-Version", "2026-07-01"),
    ("User-Agent", "copilot/1.0.88"),
];

/// A Copilot session token and the API base it is good for.
#[derive(Clone)]
pub(crate) struct Session {
    pub(crate) token: SecretString,
    pub(crate) base: String,
    pub(crate) direct: bool,
}

impl Session {
    pub(crate) fn headers(&self) -> &'static [(&'static str, &'static str)] {
        if self.direct {
            &CLI_HEADERS
        } else {
            &EDITOR_HEADERS
        }
    }
}

impl fmt::Debug for Session {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Session")
            .field("token", &"<redacted>")
            .field("base", &self.base)
            .field("direct", &self.direct)
            .finish()
    }
}

/// The GitHub sign-in stored for an account.
pub(crate) struct Stored {
    pub(crate) oauth: SecretString,
    pub(crate) site: Site,
    pub(crate) external: Option<Source>,
    pub(crate) entitlement: Entitlement,
}

fn entitlement(blob: &SecretObject) -> Entitlement {
    Entitlement {
        plan: blob.text("plan").map(str::to_owned),
        sku: blob.text("access_type_sku").map(str::to_owned),
    }
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
    let blob = SecretObject::parse(&secret)
        .map_err(|_| PluginError::auth("the stored Copilot sign-in is unreadable"))?;
    if let Some(source) = discover::source_from_blob(&blob) {
        let external = discover::read_external(host, &source).await?;
        return Ok(Stored {
            oauth: external.oauth,
            site: external.site,
            external: Some(external.source),
            entitlement: entitlement(&blob),
        });
    }
    read_blob(&secret)
}

/// Returns the external reference in `secret`, without reading its token.
pub(crate) fn external_source(secret: &SecretString) -> Option<Source> {
    SecretObject::parse(secret)
        .ok()
        .and_then(|blob| discover::source_from_blob(&blob))
}

/// Adds the enterprise host to a pre-host-aware bare account label, keeping
/// the existing account id and credential.
pub(crate) async fn migrate_legacy_name(
    host: &HostContext,
    account: &AccountId,
) -> Result<(), PluginError> {
    let Some(secret) = host.accounts().secret(account).await? else {
        return Ok(());
    };
    let Ok(blob) = SecretObject::parse(&secret) else {
        return Ok(());
    };
    if discover::source_from_blob(&blob).is_some() {
        return Ok(());
    }
    let Ok(site) = Site::parse(blob.text("host").unwrap_or(GITHUB)) else {
        return Ok(());
    };
    let Some(login) = blob.text("login") else {
        return Ok(());
    };
    if !site.is_enterprise() {
        return Ok(());
    }
    let lease = host.accounts().lease(account, LEASE_WAIT).await?;
    if lease.account().label() != login {
        return Ok(());
    }
    let label = discover::account_name(login, site.name());
    lease.commit(Some(label), secret).await
}

/// Reads a device-signed stored blob. A lapsed, unreadable, or incomplete one
/// is `Auth`. External references are read by [`stored`].
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
    Ok(Stored {
        oauth,
        site,
        external: None,
        entitlement: entitlement(&blob),
    })
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
    if stored
        .external
        .as_ref()
        .is_some_and(|source| source.kind == SourceKind::Cli)
    {
        return direct(host, account, &stored).await;
    }
    let reply = github_get(stored.site.session_url(), &stored.oauth)
        .fetch(host.http())
        .await?;
    match reply.status() {
        200..=299 => {
            let parsed = parse_session(&stored.site, reply.body());
            if parsed.is_ok() {
                unlapse(host, account, &stored).await?;
            }
            parsed
        }
        401 | 403 => {
            lapse(host, account, &stored.oauth).await?;
            if let Some(source) = &stored.external
                && let Some(cli) = discover::stand_in(host, source).await
            {
                let stored = Stored {
                    oauth: cli.oauth,
                    site: cli.site,
                    external: Some(cli.source),
                    entitlement: stored.entitlement.clone(),
                };
                return direct(host, account, &stored).await;
            }
            Err(PluginError::auth(LAPSED_REASON))
        }
        status => Err(PluginError::status_error(
            status,
            "GitHub did not issue a Copilot session token",
        )),
    }
}

/// Uses a CLI token directly after checking its account and API base.
async fn direct(
    host: &HostContext,
    account: &AccountId,
    stored: &Stored,
) -> Result<(Session, Option<SystemTime>), PluginError> {
    let reply = github_get(stored.site.copilot_user_url(), &stored.oauth)
        .fetch(host.http())
        .await?;
    match reply.status() {
        200..=299 => {
            let base = direct_base(&stored.site, reply.body())?;
            unlapse(host, account, stored).await?;
            Ok((
                Session {
                    token: stored.oauth.clone(),
                    base,
                    direct: true,
                },
                Some(host.clock().now() + Duration::from_secs(30 * 60)),
            ))
        }
        401 => {
            lapse(host, account, &stored.oauth).await?;
            Err(PluginError::auth(
                "GitHub refused the Copilot CLI sign-in; run `copilot login` again",
            ))
        }
        403 => Err(PluginError::auth(
            "GitHub refused Copilot access for the CLI sign-in; run `copilot login` again",
        )),
        status => Err(PluginError::status_error(
            status,
            "GitHub did not accept the Copilot CLI sign-in",
        )),
    }
}

fn direct_base(site: &Site, body: &[u8]) -> Result<String, PluginError> {
    let Ok(Value::Object(object)) = json::from_slice::<Value>(body) else {
        return Err(PluginError::decode(
            "the Copilot CLI user reply is not a JSON object",
        ));
    };
    match object
        .get("endpoints")
        .and_then(|endpoints| endpoints.get("api"))
        .and_then(Value::as_str)
        .filter(|api| !api.is_empty())
    {
        Some(api) => site.trusted_api(api).ok_or_else(|| {
            PluginError::auth(
                "Copilot named an API host outside GitHub; the token is not sent there",
            )
        }),
        None => Ok(site.api_base()),
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
            direct: false,
        },
        Some(UNIX_EPOCH + Duration::from_secs(expires_at)),
    ))
}

/// Marks the stored sign-in lapsed, unless another sign-in replaced the
/// token that was refused. An external account is marked only while its
/// reference is still stored; its own files are never changed.
pub(crate) async fn lapse(
    host: &HostContext,
    account: &AccountId,
    refused: &SecretString,
) -> Result<(), PluginError> {
    let lease = host.accounts().lease(account, LEASE_WAIT).await?;
    let Ok(blob) = SecretObject::parse(lease.secret()) else {
        return Ok(());
    };
    let same = discover::source_from_blob(&blob).is_some()
        || blob
            .secret("oauth_token")
            .is_some_and(|token| same_secret(&token, refused));
    if !same {
        return Ok(());
    }
    lease
        .commit(None, blob.with_flag(LAPSED, true).to_secret())
        .await
}

/// Clears a previous external refusal after the same reference works again.
async fn unlapse(
    host: &HostContext,
    account: &AccountId,
    stored: &Stored,
) -> Result<(), PluginError> {
    let Some(source) = &stored.external else {
        return Ok(());
    };
    let lease = host.accounts().lease(account, LEASE_WAIT).await?;
    let Ok(blob) = SecretObject::parse(lease.secret()) else {
        return Ok(());
    };
    if discover::source_from_blob(&blob).as_ref() != Some(source) || !blob.flag(LAPSED) {
        return Ok(());
    }
    lease
        .commit(None, blob.with_flag(LAPSED, false).to_secret())
        .await
}

/// Keeps a successful entitlement reading only while the credential it was
/// read with is still the stored one.
pub(crate) async fn update_entitlement(
    host: &HostContext,
    account: &AccountId,
    stored: &Stored,
    refreshed: &Entitlement,
) -> Result<(), PluginError> {
    if refreshed.is_empty() {
        return Ok(());
    }
    let lease = host.accounts().lease(account, LEASE_WAIT).await?;
    let Ok(mut blob) = SecretObject::parse(lease.secret()) else {
        return Ok(());
    };
    let same = match &stored.external {
        Some(source) => discover::source_from_blob(&blob).as_ref() == Some(source),
        None => blob
            .secret("oauth_token")
            .is_some_and(|token| same_secret(&token, &stored.oauth)),
    };
    if !same {
        return Ok(());
    }
    if let Some(plan) = &refreshed.plan {
        blob = blob.with_text("plan", plan);
    }
    if let Some(sku) = &refreshed.sku {
        blob = blob.with_text("access_type_sku", sku);
    }
    lease.commit(None, blob.to_secret()).await
}
