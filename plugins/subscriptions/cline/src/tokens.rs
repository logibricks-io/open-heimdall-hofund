//! Stored credentials, bearer strings, and refresh.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hofund_plugin_api::{AccountId, HostContext, PluginError, PluginErrorKind, SecretString};
use hofund_plugin_sdk::json::{self, Value};
use hofund_plugin_sdk::oauth::secret_text;
use hofund_plugin_sdk::oauth::{SecretObject, detached, same_secret};
use hofund_plugin_sdk::time::parse_rfc3339;

use crate::Endpoints;
use crate::api::{self, bearer_oauth};

/// How long before expiry an OAuth access token is refreshed.
pub(crate) const REFRESH_MARGIN: Duration = Duration::from_secs(5 * 60);

/// After a transient refresh failure, an unexpired token may still be used.
pub(crate) const RETRYABLE_GRACE: Duration = Duration::from_secs(30);

/// Secret flag when refresh is permanently refused.
pub(crate) const LAPSED: &str = "lapsed";

/// Why a lapsed account needs a new sign-in.
pub(crate) const LAPSED_REASON: &str = "Cline refused the stored sign-in; sign in again";

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

/// Parsed token pair from register or refresh.
pub(crate) struct ParsedAuth {
    pub(crate) access: SecretString,
    pub(crate) refresh: SecretString,
    pub(crate) expires_ms: u64,
    pub(crate) uid: Option<String>,
    pub(crate) email: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) account_id: Option<String>,
}

/// Bearer token and metadata for outbound calls.
#[derive(Debug)]
pub(crate) struct Bearer {
    pub(crate) token: SecretString,
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

/// Reads the bearer Cline expects for this account secret.
pub(crate) fn bearer_of(blob: &SecretObject) -> Result<SecretString, PluginError> {
    match kind(blob) {
        AuthKind::Api => blob
            .secret("api_key")
            .ok_or_else(|| PluginError::auth("the stored Cline sign-in has no API key")),
        AuthKind::Oauth => {
            let access = blob
                .secret("access_token")
                .ok_or_else(|| PluginError::auth("the stored Cline sign-in has no access token"))?;
            Ok(SecretString::from(bearer_oauth(secret_text(&access))))
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
        .ok_or_else(|| PluginError::auth("no Cline sign-in is stored for this account"))?;
    let blob = stored(&secret)?;
    if kind(&blob) == AuthKind::Api {
        return Ok(Bearer {
            token: bearer_of(&blob)?,
        });
    }
    if !needs_refresh(&blob, host.clock().now()) {
        return Ok(Bearer {
            token: bearer_of(&blob)?,
        });
    }
    let access = blob
        .secret("access_token")
        .ok_or_else(|| PluginError::auth("the stored Cline sign-in has no access token"))?;
    let job = refresh_oauth(host.clone(), endpoints.clone(), account.clone(), access);
    detached(host.scheduler(), "cline-refresh", job).await
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
        .ok_or_else(|| PluginError::auth("no Cline sign-in is stored for this account"))?;
    let blob = stored(&secret)?;
    if kind(&blob) == AuthKind::Api {
        return Ok(());
    }
    let access = blob
        .secret("access_token")
        .ok_or_else(|| PluginError::auth("the stored Cline sign-in has no access token"))?;
    refresh_oauth(host.clone(), endpoints.clone(), account.clone(), access)
        .await
        .map(|_| ())
}

async fn refresh_oauth(
    host: HostContext,
    endpoints: Endpoints,
    account: AccountId,
    stale: SecretString,
) -> Result<Bearer, PluginError> {
    let lease = host.accounts().lease(&account, LEASE_WAIT).await?;
    let blob = stored(lease.secret())?;
    if kind(&blob) == AuthKind::Api {
        return Ok(Bearer {
            token: bearer_of(&blob)?,
        });
    }
    let current = blob
        .secret("access_token")
        .ok_or_else(|| PluginError::auth("the stored Cline sign-in has no access token"))?;
    if !same_secret(&current, &stale) {
        return Ok(Bearer {
            token: bearer_of(&blob)?,
        });
    }
    let refresh_token = blob
        .secret("refresh_token")
        .ok_or_else(|| PluginError::auth("the stored Cline sign-in has no refresh token"))?;
    let body = json::json!({
        "refreshToken": secret_text(&refresh_token),
        "grantType": "refresh_token",
    });
    let reply = match api::post_json(&host, &endpoints, "/auth/refresh", &body, None, true).await {
        Ok(data) => data,
        Err(error) if error.kind() == PluginErrorKind::Auth => {
            lease
                .commit(None, blob.with_flag(LAPSED, true).to_secret())
                .await?;
            return Err(error);
        }
        Err(_error)
            if !expired(&blob, host.clock().now())
                && ms_remaining(&blob, host.clock().now()) > RETRYABLE_GRACE =>
        {
            return Ok(Bearer {
                token: bearer_of(&blob)?,
            });
        }
        Err(error) => return Err(error),
    };
    let parsed = parse_register(&host, &reply, Some(&refresh_token))?;
    let mut next = blob
        .with_secret("access_token", &parsed.access)
        .with_secret("refresh_token", &parsed.refresh)
        .with_number("expires_ms", parsed.expires_ms);
    if let Some(uid) = parsed.uid {
        next = next.with_text("uid", &uid);
    }
    if let Some(email) = parsed.email {
        next = next.with_text("email", &email);
    }
    if let Some(name) = parsed.name {
        next = next.with_text("name", &name);
    }
    lease.commit(None, next.to_secret()).await?;
    Ok(Bearer {
        token: SecretString::from(bearer_oauth(secret_text(&parsed.access))),
    })
}

fn ms_remaining(blob: &SecretObject, now: SystemTime) -> Duration {
    let ms = expires_ms(blob);
    if ms == 0 {
        return Duration::from_secs(3600);
    }
    let Ok(at) = ms_to_system(ms) else {
        return Duration::ZERO;
    };
    at.duration_since(now).unwrap_or(Duration::ZERO)
}

/// Reads Cline's register or refresh answer.
pub(crate) fn parse_register(
    host: &HostContext,
    data: &Value,
    fallback_refresh: Option<&SecretString>,
) -> Result<ParsedAuth, PluginError> {
    let access = data
        .get("accessToken")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| PluginError::decode("Cline sent back no access token"))?;
    let refresh = data
        .get("refreshToken")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(SecretString::from)
        .or_else(|| fallback_refresh.cloned())
        .ok_or_else(|| PluginError::decode("Cline sent back no refresh token"))?;
    let ui = data.get("userInfo").and_then(Value::as_object);
    let uid = ui
        .and_then(|map| map.get("clineUserId").or_else(|| map.get("subject")))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned);
    let email = ui
        .and_then(|map| map.get("email"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned);
    let name = ui
        .and_then(|map| map.get("name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned);
    let account_id = email.clone().or(uid.clone());
    let expires_ms = expires_at_ms(host, data.get("expiresAt"));
    Ok(ParsedAuth {
        access: SecretString::from(access.to_owned()),
        refresh,
        expires_ms,
        uid,
        email,
        name,
        account_id,
    })
}

fn expires_at_ms(host: &HostContext, value: Option<&Value>) -> u64 {
    match value {
        Some(Value::String(text)) => parse_rfc3339(text)
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or_else(|| default_expiry_ms(host)),
        Some(Value::Number(number)) => {
            let raw = number
                .as_u64()
                .or_else(|| number.as_f64().map(|n| n as u64))
                .unwrap_or_else(|| default_expiry_ms(host));
            normalize_epoch_ms(raw)
        }
        _ => default_expiry_ms(host),
    }
}

fn normalize_epoch_ms(raw: u64) -> u64 {
    if raw > 0 && raw < 1_000_000_000_000 {
        raw.saturating_mul(1000)
    } else {
        raw
    }
}

fn default_expiry_ms(host: &HostContext) -> u64 {
    host.clock()
        .now()
        .checked_add(Duration::from_secs(3600))
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
pub(crate) async fn refresh_for_test(
    host: HostContext,
    endpoints: Endpoints,
    account: AccountId,
    stale: SecretString,
) -> Result<Bearer, PluginError> {
    refresh_oauth(host, endpoints, account, stale).await
}
