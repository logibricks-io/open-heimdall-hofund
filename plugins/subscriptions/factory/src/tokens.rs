//! Stored credentials, bearer strings, and refresh.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::form_body;
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::oauth::secret_text;
use hofund_plugin_sdk::oauth::{
    SecretObject, detached, parse_token_set, same_secret, token_error, unverified_claims,
};

use crate::Endpoints;
use crate::WORKOS_CLIENT_ID;
use crate::api;

/// How long before expiry a request refreshes an OAuth access token.
pub(crate) const REFRESH_MARGIN: Duration = Duration::from_secs(2 * 60);

/// Secret flag when refresh is permanently refused.
pub(crate) const LAPSED: &str = "lapsed";

/// Why a lapsed account needs a new sign-in.
pub(crate) const LAPSED_REASON: &str = "Factory refused the stored sign-in; sign in again";

const LEASE_WAIT: Duration = Duration::from_secs(30);

/// OAuth or API key authentication.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AuthKind {
    Oauth,
    Api,
}

impl AuthKind {
    #[must_use]
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Oauth => "oauth",
            Self::Api => "api",
        }
    }

    fn parse(text: &str) -> Self {
        if text.eq_ignore_ascii_case("api") {
            Self::Api
        } else {
            Self::Oauth
        }
    }
}

#[derive(Debug)]
pub(crate) struct Bearer {
    pub(crate) token: SecretString,
    pub(crate) region: Option<String>,
    pub(crate) active_org: Option<String>,
    pub(crate) prem_base_host: Option<String>,
    pub(crate) oauth: bool,
}

fn bearer_from_blob(blob: &SecretObject) -> Result<Bearer, PluginError> {
    Ok(Bearer {
        token: bearer_of(blob)?,
        region: blob.text("region").map(str::to_owned),
        active_org: blob.text("active_organization_id").map(str::to_owned),
        prem_base_host: blob.text("prem_base_host").map(str::to_owned),
        oauth: kind(blob) == AuthKind::Oauth,
    })
}

pub(crate) fn expires_ms_from_access(access: &SecretString, now: SystemTime) -> u64 {
    if let Ok(claims) = unverified_claims(access)
        && let Some(exp) = claims.get("exp").and_then(Value::as_u64)
    {
        return exp.saturating_mul(1000);
    }
    default_expiry_ms(now)
}

fn stored(secret: &SecretString) -> Result<SecretObject, PluginError> {
    let blob = SecretObject::parse(secret)?;
    if blob.flag(LAPSED) {
        return Err(PluginError::auth(LAPSED_REASON));
    }
    Ok(blob)
}

pub(crate) fn kind(blob: &SecretObject) -> AuthKind {
    blob.text("auth_type")
        .map(AuthKind::parse)
        .unwrap_or(AuthKind::Oauth)
}

/// Reads the bearer Factory expects for this account secret.
pub(crate) fn bearer_of(blob: &SecretObject) -> Result<SecretString, PluginError> {
    match kind(blob) {
        AuthKind::Api => blob
            .secret("api_key")
            .ok_or_else(|| PluginError::auth("the stored Factory sign-in has no API key")),
        AuthKind::Oauth => {
            let access = blob.secret("access_token").ok_or_else(|| {
                PluginError::auth("the stored Factory sign-in has no access token")
            })?;
            Ok(access)
        }
    }
}

fn expires_ms(blob: &SecretObject) -> u64 {
    blob.number("expires_ms").unwrap_or(0)
}

fn expired(blob: &SecretObject, now: SystemTime) -> bool {
    let ms = expires_ms(blob);
    if ms == 0 {
        return false;
    }
    let Ok(at) = ms_to_system(ms) else {
        return false;
    };
    at <= now
}

fn ms_to_system(ms: u64) -> Result<SystemTime, ()> {
    UNIX_EPOCH.checked_add(Duration::from_millis(ms)).ok_or(())
}

fn needs_refresh(blob: &SecretObject, now: SystemTime) -> bool {
    let ms = expires_ms(blob);
    if ms == 0 {
        return true;
    }
    let Ok(at) = ms_to_system(ms) else {
        return true;
    };
    at <= now + REFRESH_MARGIN
}

/// Returns a bearer ready to send, refreshing OAuth tokens when near expiry.
pub(crate) async fn bearer(
    host: &HostContext,
    endpoints: &Endpoints,
    account: &AccountId,
) -> Result<Bearer, PluginError> {
    let secret = host
        .accounts()
        .secret(account)
        .await?
        .ok_or_else(|| PluginError::auth("no Factory sign-in is stored for this account"))?;
    let blob = stored(&secret)?;
    if kind(&blob) == AuthKind::Api {
        return bearer_from_blob(&blob);
    }
    if !needs_refresh(&blob, host.clock().now()) {
        return bearer_from_blob(&blob);
    }
    let access = blob
        .secret("access_token")
        .ok_or_else(|| PluginError::auth("the stored Factory sign-in has no access token"))?;
    let job = refresh_oauth(
        host.clone(),
        endpoints.clone(),
        account.clone(),
        access,
        true,
    );
    detached(host.scheduler(), "factory-refresh", job).await
}

/// Explicit refresh for the subscription `refresh` hook.
pub(crate) async fn refresh_account(
    host: &HostContext,
    endpoints: &Endpoints,
    account: &AccountId,
) -> Result<(), PluginError> {
    let secret = host
        .accounts()
        .secret(account)
        .await?
        .ok_or_else(|| PluginError::auth("no Factory sign-in is stored for this account"))?;
    let blob = stored(&secret)?;
    if kind(&blob) == AuthKind::Api {
        return Ok(());
    }
    let access = blob
        .secret("access_token")
        .ok_or_else(|| PluginError::auth("the stored Factory sign-in has no access token"))?;
    refresh_oauth(
        host.clone(),
        endpoints.clone(),
        account.clone(),
        access,
        false,
    )
    .await
    .map(|_| ())
}

async fn refresh_oauth(
    host: HostContext,
    endpoints: Endpoints,
    account: AccountId,
    stale: SecretString,
    allow_unexpired_grace: bool,
) -> Result<Bearer, PluginError> {
    let lease = host.accounts().lease(&account, LEASE_WAIT).await?;
    let blob = stored(lease.secret())?;
    if kind(&blob) == AuthKind::Api {
        return bearer_from_blob(&blob);
    }
    let current = blob
        .secret("access_token")
        .ok_or_else(|| PluginError::auth("the stored Factory sign-in has no access token"))?;
    if !same_secret(&current, &stale) {
        return bearer_from_blob(&blob);
    }
    let refresh_token = blob
        .secret("refresh_token")
        .ok_or_else(|| PluginError::auth("the stored Factory sign-in has no refresh token"))?;
    let url = endpoints.workos_token.trim_end_matches('/').to_owned();
    let form = form_body(&[
        ("grant_type", "refresh_token"),
        ("client_id", WORKOS_CLIENT_ID),
        ("refresh_token", secret_text(&refresh_token)),
    ]);
    let reply = match Call::post(url)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("accept", "application/json")
        .bytes(form)
        .fetch(host.http())
        .await
    {
        Ok(reply) => reply,
        Err(_error) if allow_unexpired_grace && !expired(&blob, host.clock().now()) => {
            return bearer_from_blob(&blob);
        }
        Err(error) => return Err(error),
    };
    if (400..500).contains(&reply.status()) && reply.status() != 429 {
        let error = token_error(reply.status(), reply.body());
        lease
            .commit(None, blob.with_flag(LAPSED, true).to_secret())
            .await?;
        return Err(error);
    }
    if !(200..300).contains(&reply.status()) {
        if allow_unexpired_grace && !expired(&blob, host.clock().now()) {
            return bearer_from_blob(&blob);
        }
        return Err(PluginError::status_error(
            reply.status(),
            "WorkOS did not refresh the Factory sign-in",
        ));
    }
    let set = parse_token_set(reply.body(), host.clock().now())
        .map_err(|err| PluginError::auth(err.to_string()))?
        .or_refresh(Some(&refresh_token));
    let access = set.access().clone();
    let refresh = set
        .refresh()
        .cloned()
        .unwrap_or_else(|| refresh_token.clone());
    let expires_ms = set
        .expires_at()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_else(|| expires_ms_from_access(&access, host.clock().now()));
    let mut next = blob
        .with_secret("access_token", &access)
        .with_secret("refresh_token", &refresh)
        .with_number("expires_ms", expires_ms);
    let who_base = endpoints.api_base_for_region(next.text("region"));
    if let Ok(who) = api::whoami(
        &host,
        who_base,
        &access,
        next.text("active_organization_id"),
    )
    .await
        && let Some(org) = who
            .get("orgId")
            .and_then(Value::as_str)
            .filter(|org| !org.is_empty())
    {
        next = next.with_text("active_organization_id", org);
        next = optional_text(next, "region", who.get("region").and_then(Value::as_str));
        next = optional_text(
            next,
            "prem_base_host",
            who.get("premBaseHostV2").and_then(Value::as_str),
        );
    }
    lease.commit(None, next.to_secret()).await?;
    bearer_from_blob(&next)
}

fn optional_text(blob: SecretObject, key: &str, value: Option<&str>) -> SecretObject {
    match value.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => blob.with_text(key, value),
        None => blob.without(key),
    }
}

fn default_expiry_ms(now: SystemTime) -> u64 {
    now.checked_add(Duration::from_secs(3600))
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
pub(crate) async fn refresh_for_test(
    host: HostContext,
    endpoints: Endpoints,
    account: AccountId,
    stale: SecretString,
) -> Result<Bearer, PluginError> {
    refresh_oauth(host, endpoints, account, stale, true).await
}
