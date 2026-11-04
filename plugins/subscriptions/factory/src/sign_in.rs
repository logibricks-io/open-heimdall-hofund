//! WorkOS device sign-in and API key import.

use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::form_body;
use hofund_plugin_sdk::json::{self, Value};
use hofund_plugin_sdk::oauth::device::{self, DeviceStart};
use hofund_plugin_sdk::oauth::secret_text;
use hofund_plugin_sdk::oauth::{SecretObject, account_id_from};

use crate::api;
use crate::tokens::{self, AuthKind, parse_register};
use crate::{Endpoints, WORKOS_CLIENT_ID};

/// A device authorization waiting for approval.
pub(crate) struct DevicePending {
    pub(crate) start: DeviceStart,
}

/// A finished sign-in ready to store.
pub(crate) struct SignedIn {
    pub(crate) id: AccountId,
    pub(crate) label: String,
    pub(crate) secret: SecretString,
}

fn workos_device_url(endpoints: &Endpoints) -> String {
    endpoints.workos_device.trim_end_matches('/').to_owned()
}

fn workos_token_url(endpoints: &Endpoints) -> String {
    endpoints.workos_token.trim_end_matches('/').to_owned()
}

/// Starts WorkOS device authorization.
pub(crate) async fn device_start(
    host: &HostContext,
    endpoints: &Endpoints,
) -> Result<DevicePending, PluginError> {
    let form = form_body(&[("client_id", WORKOS_CLIENT_ID)]);
    let reply = Call::post(workos_device_url(endpoints))
        .header("content-type", "application/x-www-form-urlencoded")
        .header("accept", "application/json")
        .bytes(form)
        .fetch(host.http())
        .await?;
    if !(200..300).contains(&reply.status()) {
        return Err(PluginError::status_error(
            reply.status(),
            "Factory sign-in: the device code request failed",
        ));
    }
    let start = device::parse_start(reply.body())?;
    Ok(DevicePending { start })
}

/// Verification page URL from a device authorization response.
#[must_use]
pub(crate) fn verification_page(pending: &DevicePending) -> String {
    pending
        .start
        .verification_uri_complete()
        .map(str::to_owned)
        .unwrap_or_else(|| pending.start.verification_uri().to_owned())
}

/// Polls WorkOS, registers with Factory, and returns stored credentials.
pub(crate) async fn device_finish(
    host: &HostContext,
    endpoints: &Endpoints,
    pending: &DevicePending,
) -> Result<SignedIn, PluginError> {
    let token_url = workos_token_url(endpoints);
    let start = &pending.start;
    let http = host.http();
    let set = device::poll(host.clock(), start, || {
        Call::post(token_url.clone())
            .header("content-type", "application/x-www-form-urlencoded")
            .header("accept", "application/json")
            .bytes(device::device_code_form(WORKOS_CLIENT_ID, start))
            .fetch(http)
    })
    .await?;
    register_workos(host, endpoints, set.access(), set.refresh()).await
}

async fn register_workos(
    host: &HostContext,
    endpoints: &Endpoints,
    access: &SecretString,
    refresh: Option<&SecretString>,
) -> Result<SignedIn, PluginError> {
    let refresh_text = refresh.map(secret_text).unwrap_or("");
    let body = json::json!({
        "accessToken": secret_text(access),
        "refreshToken": refresh_text,
    });
    let data = api::post_json(
        host,
        endpoints.api_base(),
        "/api/v1/auth/register",
        &body,
        None,
        false,
    )
    .await?;
    let parsed = parse_register(&data, refresh, host.clock().now())?;
    store_signed_in(parsed)
}

fn store_signed_in(parsed: tokens::ParsedAuth) -> Result<SignedIn, PluginError> {
    let label = parsed
        .email
        .clone()
        .or_else(|| parsed.name.clone())
        .unwrap_or_else(|| parsed.uid.clone().unwrap_or_else(|| "Factory".to_owned()));
    let key = parsed
        .account_id
        .clone()
        .or(parsed.email.clone())
        .or(parsed.uid.clone())
        .unwrap_or_else(|| label.clone());
    let id = account_id_from(&["factory", &key.to_ascii_lowercase()]);
    let mut blob = SecretObject::new()
        .with_number("v", 1)
        .with_text("auth_type", AuthKind::Oauth.as_str())
        .with_secret("access_token", &parsed.access)
        .with_secret("refresh_token", &parsed.refresh)
        .with_number("expires_ms", parsed.expires_ms);
    if let Some(uid) = parsed.uid {
        blob = blob.with_text("uid", &uid);
    }
    if let Some(email) = parsed.email {
        blob = blob.with_text("email", &email);
    }
    if let Some(name) = parsed.name {
        blob = blob.with_text("name", &name);
    }
    blob = blob.with_text("account_id", &key);
    Ok(SignedIn {
        id,
        label,
        secret: blob.to_secret(),
    })
}

/// Stores an API key after checking it with `/api/cli/whoami`.
pub(crate) async fn import_api_key(
    host: &HostContext,
    endpoints: &Endpoints,
    key: SecretString,
) -> Result<SignedIn, PluginError> {
    let trimmed = secret_text(&key).trim();
    if trimmed.is_empty() {
        return Err(PluginError::auth("Factory API key is empty"));
    }
    if !trimmed.starts_with("fk-") {
        return Err(PluginError::auth("Factory API keys start with fk-"));
    }
    let key = SecretString::from(trimmed.to_owned());
    let who = api::get(
        host,
        endpoints.api_base(),
        "/api/cli/whoami",
        Some(&key),
        true,
    )
    .await?;
    let email = first_string(&who, &["email"]);
    let uid = first_string(&who, &["userId"]);
    let region = first_string(&who, &["region"]);
    let label = email
        .clone()
        .or(uid.clone())
        .unwrap_or_else(|| "Factory API key".to_owned());
    let account_key = email
        .clone()
        .or(uid.clone())
        .unwrap_or_else(|| label.clone());
    let id = account_id_from(&["factory", "api", &account_key.to_ascii_lowercase()]);
    let mut blob = SecretObject::new()
        .with_number("v", 1)
        .with_text("auth_type", AuthKind::Api.as_str())
        .with_secret("api_key", &key)
        .with_text("account_id", &account_key);
    if let Some(ref addr) = email {
        blob = blob.with_text("email", addr);
    }
    if let Some(ref user) = uid {
        blob = blob.with_text("uid", user);
    }
    if let Some(ref reg) = region {
        blob = blob.with_text("region", reg);
    }
    Ok(SignedIn {
        id,
        label,
        secret: blob.to_secret(),
    })
}

fn first_string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}
