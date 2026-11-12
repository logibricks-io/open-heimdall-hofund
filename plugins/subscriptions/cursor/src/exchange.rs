//! Cursor API-key exchange (`POST /auth/exchange_user_api_key`).
//!
//! The account store holds the key. The exchanged access token lives only in
//! this process, cached by SHA-256 of the key, and is never written back.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::oauth::secret_text;

use crate::endpoints::Endpoints;
use crate::tokens;

/// How long a renew waits for the per-account lease.
const LEASE_WAIT: Duration = Duration::from_secs(30);

/// Budget for one exchange call, matching the reference (`index.mjs:356`).
const EXCHANGE_WAIT: Duration = Duration::from_secs(15);

/// Environment variable read at import or sign-in only.
pub(crate) const CURSOR_API_KEY_ENV: &str = "CURSOR_API_KEY";

struct Cached {
    token: SecretString,
    exp: Option<u64>,
}

static CACHE: OnceLock<Mutex<HashMap<String, Cached>>> = OnceLock::new();

fn cache() -> MutexGuard<'static, HashMap<String, Cached>> {
    CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
thread_local! {
    static TEST_API_KEY: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// Overrides `CURSOR_API_KEY` for one test thread. `None` clears the override
/// and does not fall through to the process environment.
#[cfg(test)]
pub(crate) fn set_test_api_key(key: Option<&str>) {
    TEST_API_KEY.with(|cell| {
        *cell.borrow_mut() = key.map(|value| value.to_owned());
    });
}

#[cfg(test)]
fn test_api_key() -> Option<String> {
    TEST_API_KEY.with(|cell| cell.borrow().clone())
}

/// Reads `CURSOR_API_KEY` (or the test hook). Never used on the chat path.
#[cfg(test)]
pub(crate) fn read_env_api_key() -> Option<SecretString> {
    test_api_key()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .map(SecretString::from)
}

/// Reads `CURSOR_API_KEY`. Never used on the chat path.
#[cfg(not(test))]
pub(crate) fn read_env_api_key() -> Option<SecretString> {
    std::env::var(CURSOR_API_KEY_ENV)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .map(SecretString::from)
}

/// Exchanges `key` and updates the memory cache. No account lease (the
/// account may not exist yet).
pub(crate) async fn exchange_key(
    host: &HostContext,
    endpoints: &Endpoints,
    key: &SecretString,
) -> Result<SecretString, PluginError> {
    let trimmed = secret_text(key).trim();
    if trimmed.is_empty() {
        return Err(PluginError::auth("Cursor API key is empty"));
    }
    let token = post_exchange(host, endpoints, trimmed).await?;
    remember(trimmed, &token);
    Ok(token)
}

/// Returns a bearer token for a stored API-key account.
///
/// Takes `accounts().lease`, rereads the key, and exchanges only when the
/// memory cache is missing or the cached token is within [`tokens::REFRESH_MARGIN`].
/// The cache is updated inside the lease. A lease timeout is `Io`.
pub(crate) async fn access_token(
    host: &HostContext,
    endpoints: &Endpoints,
    account: &AccountId,
) -> Result<SecretString, PluginError> {
    let lease = host.accounts().lease(account, LEASE_WAIT).await?;
    let blob = tokens::parse_blob(lease.secret())?;
    let key = tokens::api_key(&blob)
        .ok_or_else(|| PluginError::auth("the stored Cursor sign-in has no API key"))?;
    let trimmed = secret_text(&key).trim().to_owned();
    if trimmed.is_empty() {
        return Err(PluginError::auth(
            "the stored Cursor sign-in has no API key",
        ));
    }
    let now = host.clock().now();
    if let Some(token) = cached(&trimmed, now) {
        return Ok(token);
    }
    let token = post_exchange(host, endpoints, &trimmed).await?;
    remember(&trimmed, &token);
    Ok(token)
}

fn cached(key: &str, now: std::time::SystemTime) -> Option<SecretString> {
    let cache = cache();
    let entry = cache.get(&tokens::key_hash(key))?;
    if tokens::near_expiry(now, entry.exp) {
        return None;
    }
    Some(entry.token.clone())
}

fn remember(key: &str, token: &SecretString) {
    let exp = tokens::jwt_expiry_ms(secret_text(token));
    cache().insert(
        tokens::key_hash(key),
        Cached {
            token: token.clone(),
            exp,
        },
    );
}

async fn post_exchange(
    host: &HostContext,
    endpoints: &Endpoints,
    key: &str,
) -> Result<SecretString, PluginError> {
    let url = endpoints.exchange_user_api_key();
    let reply = Call::post(url)
        .header("content-type", "application/json")
        .sensitive_header("authorization", format!("Bearer {key}"))
        .header("x-cursor-client-version", &endpoints.client_version)
        .header("x-cursor-client-type", "cli")
        .header("x-ghost-mode", "true")
        .read_timeout(EXCHANGE_WAIT)
        .bytes(b"{}")
        .fetch(host.http())
        .await;
    let reply = match reply {
        Ok(reply) => reply,
        Err(error) => {
            return Err(PluginError::status_error(
                502,
                format!("couldn't reach Cursor's API: {error}"),
            ));
        }
    };
    if reply.status() >= 500 {
        return Err(PluginError::status_error(
            502,
            format!(
                "exchange_user_api_key: HTTP {status}",
                status = reply.status()
            ),
        ));
    }
    if !(200..300).contains(&reply.status()) {
        return Err(PluginError::status_error(
            401,
            "Cursor didn't take this API key",
        ));
    }
    let value: Value = hofund_plugin_sdk::json::from_slice(reply.body()).unwrap_or(Value::Null);
    let access = value
        .get("accessToken")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| PluginError::status_error(401, "Cursor didn't take this API key"))?;
    Ok(SecretString::from(access.to_owned()))
}
