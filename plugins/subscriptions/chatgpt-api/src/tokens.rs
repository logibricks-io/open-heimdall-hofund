//! Access token read and refresh under the account lease.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::form_body;
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::oauth::{
    SecretObject, detached, parse_token_set, same_secret, secret_text, unverified_claims,
};

use crate::Endpoints;
use crate::sign_in::{DIRECT_SCOPE, RESOURCE, plan_from_token};

pub(crate) const LAPSED: &str = "lapsed";

pub(crate) const LAPSED_REASON: &str = "the ChatGPT sign-in has expired — sign in again";

pub(crate) const REFRESH_MARGIN: Duration = Duration::from_secs(3 * 60);

const LEASE_WAIT: Duration = Duration::from_secs(30);

const DEAD: [&str; 6] = [
    "invalid_grant",
    "invalid_refresh_token",
    "token_expired",
    "refresh_token_expired",
    "refresh_token_invalidated",
    "refresh_token_reused",
];

#[derive(Debug)]
pub(crate) struct Access {
    pub(crate) token: SecretString,
}

pub(crate) enum Need {
    IfExpiring,
    Replace(SecretString),
    Always,
}

pub(crate) fn unix_seconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn expiry_of(blob: &SecretObject) -> Option<u64> {
    blob.number("expires_at")
}

pub(crate) fn fresh(blob: &SecretObject, now: SystemTime) -> bool {
    let Some(exp) = expiry_of(blob) else {
        return fresh_jwt(blob, now);
    };
    UNIX_EPOCH + Duration::from_secs(exp) > now + REFRESH_MARGIN
}

fn fresh_jwt(blob: &SecretObject, now: SystemTime) -> bool {
    let token = match blob.secret("access_token") {
        Some(token) => token,
        None => return false,
    };
    let exp = unverified_claims(&token)
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

pub(crate) async fn access(
    host: &HostContext,
    endpoints: &Endpoints,
    account: &AccountId,
    need: Need,
) -> Result<Access, PluginError> {
    let secret =
        host.accounts().secret(account).await?.ok_or_else(|| {
            PluginError::auth("no ChatGPT API sign-in is stored for this account")
        })?;
    let blob = stored(&secret)?;
    let token = blob
        .secret("access_token")
        .ok_or_else(|| PluginError::auth("the stored ChatGPT API sign-in has no access token"))?;
    let refresh = match &need {
        Need::IfExpiring => !fresh(&blob, host.clock().now()),
        Need::Replace(stale) => same_secret(&token, stale),
        Need::Always => true,
    };
    if !refresh {
        return Ok(Access { token });
    }
    let job = rotate(host.clone(), endpoints.clone(), account.clone(), token);
    detached(host.scheduler(), "chatgpt-api-refresh", job).await
}

#[cfg(test)]
pub(crate) async fn rotate_for_test(
    host: HostContext,
    endpoints: Endpoints,
    account: AccountId,
    stale: SecretString,
) -> Result<Access, PluginError> {
    rotate(host, endpoints, account, stale).await
}

async fn rotate(
    host: HostContext,
    endpoints: Endpoints,
    account: AccountId,
    stale: SecretString,
) -> Result<Access, PluginError> {
    let lease = host.accounts().lease(&account, LEASE_WAIT).await?;
    let blob = stored(lease.secret())?;
    let current = blob
        .secret("access_token")
        .ok_or_else(|| PluginError::auth("the stored ChatGPT API sign-in has no access token"))?;
    if !same_secret(&current, &stale) {
        return Ok(Access { token: current });
    }
    let client_id = blob
        .text("client_id")
        .ok_or_else(|| PluginError::auth("the stored ChatGPT API sign-in has no client id"))?;
    let refresh_token = blob
        .secret("refresh_token")
        .ok_or_else(|| PluginError::auth("the stored ChatGPT API sign-in has no refresh token"))?;
    let form = form_body(&[
        ("grant_type", "refresh_token"),
        ("client_id", client_id),
        ("refresh_token", secret_text(&refresh_token)),
        ("resource", RESOURCE),
    ]);
    let reply = Call::post(&endpoints.token)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("accept", "application/json")
        .bytes(form)
        .fetch(host.http())
        .await?;
    if !(200..300).contains(&reply.status()) {
        if let Some(code) = oauth_code(reply.body()) {
            if DEAD.contains(&code.as_str()) {
                lease
                    .commit(None, blob.with_flag(LAPSED, true).to_secret())
                    .await?;
                return Err(PluginError::auth(LAPSED_REASON));
            }
            if code == "invalid_client" {
                return Err(PluginError::auth(format!(
                    "ChatGPT API: OpenAI no longer knows the client hofund registered for this account ({code}) — sign in again"
                )));
            }
        }
        if fresh(&blob, host.clock().now()) {
            return Ok(Access { token: current });
        }
        return Err(token_error(reply.status(), reply.body()));
    }
    let now = host.clock().now();
    let set = parse_token_set(reply.body(), now)
        .map_err(|error| PluginError::decode(error.message()))?
        .or_refresh(Some(&refresh_token));
    verify_scope(reply.body())?;
    let mut next = blob.with_secret("access_token", set.access()).with_number(
        "expires_at",
        set.expires_at().map(unix_seconds).unwrap_or(0),
    );
    if let Some(refresh) = set.refresh() {
        next = next.with_secret("refresh_token", refresh);
    }
    if let Some(id_token) = set.id_token() {
        next = next.with_secret("id_token", id_token);
        let plan = plan_from_token(id_token);
        if !plan.is_empty() {
            next = next.with_text("plan", &plan);
        }
    }
    next = next.with_flag(LAPSED, false);
    lease.commit(None, next.to_secret()).await?;
    Ok(Access {
        token: set.access().clone(),
    })
}

fn verify_scope(body: &[u8]) -> Result<(), PluginError> {
    let value: Value = hofund_plugin_sdk::json::from_slice(body)
        .map_err(|_| PluginError::decode("ChatGPT API token refresh: unreadable answer"))?;
    let scope = value
        .get("scope")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if scope.is_empty() || scope.split_whitespace().any(|part| part == DIRECT_SCOPE) {
        return Ok(());
    }
    Err(PluginError::auth(format!(
        "OpenAI did not grant hofund the use of the subscription through its API ({DIRECT_SCOPE})"
    )))
}

pub(crate) fn token_error(status: u16, body: &[u8]) -> PluginError {
    let code = oauth_code(body);
    let message = match code.as_deref() {
        Some(code) => {
            format!("ChatGPT API token endpoint refused the request: HTTP {status}, {code}")
        }
        None => format!("ChatGPT API token endpoint refused the request: HTTP {status}"),
    };
    if code
        .as_deref()
        .is_some_and(|c| DEAD.contains(&c) || c == "invalid_client")
    {
        PluginError::auth(message)
    } else if status == 429 {
        PluginError::quota_status(status, message)
    } else if matches!(status, 400 | 401) {
        PluginError::auth(message)
    } else {
        PluginError::status_error(status, message)
    }
}

fn oauth_code(body: &[u8]) -> Option<String> {
    let value: Value = hofund_plugin_sdk::json::from_slice(body).ok()?;
    if let Some(code) = value.get("code").and_then(Value::as_str) {
        return Some(code.to_owned());
    }
    if let Some(code) = value.get("error").and_then(Value::as_str) {
        return Some(code.to_owned());
    }
    value
        .get("error")
        .and_then(|err| err.get("code"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}
