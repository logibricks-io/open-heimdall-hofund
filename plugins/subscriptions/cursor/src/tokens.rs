//! Stored sign-in shape and JWT helpers.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hofund_plugin_api::{AccountId, PluginError, SecretString};
use hofund_plugin_sdk::oauth::{SecretObject, account_id_from, secret_text, unverified_claims};

/// Refresh marker: token is read from cursor-agent each time.
pub(crate) const AGENT_MARK: &str = "cursor-agent";

pub(crate) const ACCESS: &str = "access_token";
pub(crate) const REFRESH: &str = "refresh_token";
pub(crate) const EXPIRES_MS: &str = "expires_ms";
pub(crate) const PLAN: &str = "plan";
pub(crate) const LABEL: &str = "label";
/// Stored Cursor API key. The exchanged access token is never written here.
pub(crate) const API_KEY: &str = "api_key";
pub(crate) const AUTH_TYPE: &str = "auth_type";
pub(crate) const AUTH_TYPE_API: &str = "api";

/// How long before expiry a token is renewed through cursor-agent.
pub(crate) const REFRESH_MARGIN: Duration = Duration::from_secs(5 * 60);

pub(crate) fn blob_oauth(
    access: &SecretString,
    refresh: &SecretString,
    expires_ms: u64,
    label: &str,
    plan: Option<&str>,
) -> SecretObject {
    let mut blob = SecretObject::new()
        .with_secret(ACCESS, access)
        .with_secret(REFRESH, refresh)
        .with_number(EXPIRES_MS, expires_ms)
        .with_text(LABEL, label);
    if let Some(plan) = plan.filter(|p| !p.is_empty()) {
        blob = blob.with_text(PLAN, plan);
    }
    blob
}

pub(crate) fn blob_agent(label: &str, plan: Option<&str>) -> SecretObject {
    let mut blob = SecretObject::new()
        .with_text(REFRESH, AGENT_MARK)
        .with_text(LABEL, label);
    if let Some(plan) = plan.filter(|p| !p.is_empty()) {
        blob = blob.with_text(PLAN, plan);
    }
    blob
}

pub(crate) fn parse_blob(secret: &SecretString) -> Result<SecretObject, PluginError> {
    SecretObject::parse(secret)
}

pub(crate) fn is_agent(blob: &SecretObject) -> bool {
    blob.text(REFRESH).is_some_and(|mark| mark == AGENT_MARK)
}

/// A blob that stores a Cursor API key and no exchanged access token.
pub(crate) fn blob_api_key(key: &SecretString, label: &str, plan: Option<&str>) -> SecretObject {
    let mut blob = SecretObject::new()
        .with_secret(API_KEY, key)
        .with_text(AUTH_TYPE, AUTH_TYPE_API)
        .with_text(LABEL, label);
    if let Some(plan) = plan.filter(|p| !p.is_empty()) {
        blob = blob.with_text(PLAN, plan);
    }
    blob
}

pub(crate) fn is_api_key(blob: &SecretObject) -> bool {
    blob.secret(API_KEY).is_some_and(|key| {
        !hofund_plugin_sdk::oauth::secret_text(&key)
            .trim()
            .is_empty()
    })
}

pub(crate) fn api_key(blob: &SecretObject) -> Option<SecretString> {
    blob.secret(API_KEY)
        .filter(|key| !hofund_plugin_sdk::oauth::secret_text(key).trim().is_empty())
}

/// SHA-256 hex of an API key, used as the in-memory cache key.
pub(crate) fn key_hash(key: &str) -> String {
    let digest = hofund_plugin_sdk::digest::sha256(key.as_bytes());
    let mut name = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(name, "{byte:02x}").expect("write to a string");
    }
    name
}

pub(crate) fn expires_ms(blob: &SecretObject) -> Option<u64> {
    blob.number(EXPIRES_MS)
}

pub(crate) fn effective_expires_ms(blob: &SecretObject) -> Option<u64> {
    if let Some(ms) = expires_ms(blob).filter(|ms| *ms > 0) {
        return Some(ms);
    }
    blob.secret(ACCESS)
        .and_then(|token| jwt_expiry_ms(secret_text(&token)))
}

pub(crate) fn jwt_expiry_ms(token: &str) -> Option<u64> {
    let claims = unverified_claims(&SecretString::from(token.to_owned())).ok()?;
    claims
        .get("exp")
        .and_then(|value| value.as_f64())
        .filter(|exp| *exp > 0.0)
        .and_then(|exp| u64::try_from((exp * 1000.0) as i64).ok())
}

pub(crate) fn jwt_identity(token: &str) -> Option<(String, AccountId)> {
    let claims = unverified_claims(&SecretString::from(token.to_owned())).ok()?;
    let email = claims
        .get("email")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    if let Some(email) = email {
        return Some((
            email.clone(),
            account_id_from(&["cursor", &email.to_ascii_lowercase()]),
        ));
    }
    let sub = claims
        .get("sub")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    Some((sub.to_owned(), account_id_from(&["cursor", sub])))
}

pub(crate) fn ms_from_now(_now: SystemTime, ms: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(ms.max(0) as u64)
}

pub(crate) fn unix_ms(now: SystemTime) -> i64 {
    now.duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

pub(crate) fn near_expiry(now: SystemTime, expires_ms: Option<u64>) -> bool {
    let Some(expires_ms) = expires_ms else {
        return false;
    };
    let expires = ms_from_now(UNIX_EPOCH, i64::try_from(expires_ms).unwrap_or(i64::MAX));
    now + REFRESH_MARGIN >= expires
}
