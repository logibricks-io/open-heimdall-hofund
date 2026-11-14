//! Desktop PKCE device sign-in, including Qoder CN VPC instances.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hofund_plugin_api::{
    AccountId, HostContext, PluginError, SecretString, SignInChallenge, SignInCtx,
};
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::oauth::{Pkce, secret_text};
use hofund_plugin_sdk::{Call, uuid};

use crate::creds::{Credential, Site};
use crate::{Endpoints, import};

const GLOBAL_CLIENT: &str = "732aef47-9cf2-46a2-95fe-4cebb5d0d1fa";
const CN_CLIENT: &str = "e883ade2-e6e3-4d6d-adf7-f92ceff5fdcb";
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
#[cfg(not(test))]
const POLL_INTERVAL: Duration = Duration::from_secs(2);
#[cfg(test)]
const POLL_INTERVAL: Duration = Duration::ZERO;

#[derive(Clone)]
pub(crate) struct Pending {
    site: crate::site::Resolved,
    site_kind: Site,
    verifier: SecretString,
    nonce: String,
    machine_id: String,
    started: SystemTime,
}

pub(crate) fn start(
    ctx: &SignInCtx,
    host: &HostContext,
    endpoints: &Endpoints,
) -> Result<(Pending, SignInChallenge), PluginError> {
    let site_kind = match ctx.option("site") {
        Some("qoder-cn" | "cn" | "china") => Site::China,
        _ => Site::Global,
    };
    let instance = ctx.option("vpc").unwrap_or("");
    let site = endpoints.resolve_for(site_kind, instance)?;
    let pkce = Pkce::generate_with_random_bytes(64)?;
    let nonce = uuid::v4().map_err(|_| PluginError::internal("the system random source failed"))?;
    let machine_id =
        uuid::v4().map_err(|_| PluginError::internal("the system random source failed"))?;
    let client_id = client_id(site_kind);
    let url = hofund_plugin_sdk::oauth::authorize_url(
        &format!("{}/device/selectAccounts", site.web.trim_end_matches('/')),
        &[
            ("challenge", &pkce.challenge),
            ("challenge_method", "S256"),
            ("nonce", &nonce),
            ("machine_id", &machine_id),
            ("client_id", client_id),
        ],
    )?;
    Ok((
        Pending {
            site,
            site_kind,
            verifier: pkce.verifier,
            nonce,
            machine_id,
            started: host.clock().now(),
        },
        SignInChallenge::open_url(url)?,
    ))
}

pub(crate) async fn complete(
    host: &HostContext,
    pending: Pending,
) -> Result<AccountId, PluginError> {
    let device = poll(host, &pending).await?;
    let (chat, device_chat) = exchange(host, &pending, &device).await?;
    let user = user_info(host, &pending, secret_text(&device.token)).await;
    let uid = first_string(&device.value, &["user_id"])
        .or_else(|| first_string(&user, &["id", "user_id", "uid"]))
        .ok_or_else(|| PluginError::auth("Qoder sign-in named no user"))?;
    let email = user
        .get("email")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let name = user
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .or_else(|| device.value.get("user_name").and_then(Value::as_str))
        .unwrap_or("")
        .to_owned();
    let credential = Credential {
        uid,
        email,
        name,
        token: chat.token,
        refresh_token: chat.refresh,
        expires_at: chat.expires_at,
        machine_id: pending.machine_id,
        site: pending.site_kind,
        vpc: pending.site.vpc,
        device_chat,
        device_token: device.token,
        device_refresh_token: device.refresh,
    };
    import::store(&credential, host).await
}

struct DeviceToken {
    token: SecretString,
    refresh: SecretString,
    value: Value,
}

struct ChatToken {
    token: SecretString,
    refresh: SecretString,
    expires_at: i64,
}

async fn poll(host: &HostContext, pending: &Pending) -> Result<DeviceToken, PluginError> {
    loop {
        if host
            .clock()
            .now()
            .duration_since(pending.started)
            .unwrap_or_default()
            > SIGN_IN_TIMEOUT
        {
            return Err(PluginError::timeout("Qoder sign-in timed out; start again"));
        }
        let mut url = hofund_plugin_sdk::url::Url::parse(&format!(
            "{}/api/v1/deviceToken/poll",
            pending.site.openapi.trim_end_matches('/')
        ))
        .map_err(|_| PluginError::config("Qoder device poll URL is invalid"))?;
        url.query_pairs_mut()
            .append_pair("nonce", &pending.nonce)
            .append_pair("verifier", secret_text(&pending.verifier))
            .append_pair("challenge_method", "S256");
        let reply = Call::get(url.as_str())
            .header("Accept", "application/json")
            .fetch(host.http())
            .await;
        if let Ok(reply) = reply
            && reply.status() == 200
            && let Ok(value) = hofund_plugin_sdk::json::from_slice::<Value>(reply.body())
            && let Some(token) = value
                .get("token")
                .and_then(Value::as_str)
                .filter(|token| !token.is_empty())
        {
            return Ok(DeviceToken {
                token: SecretString::from(token.to_owned()),
                refresh: SecretString::from(
                    value
                        .get("refresh_token")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                ),
                value,
            });
        }
        hofund_plugin_sdk::sleep(POLL_INTERVAL).await;
    }
}

async fn exchange(
    host: &HostContext,
    pending: &Pending,
    device: &DeviceToken,
) -> Result<(ChatToken, bool), PluginError> {
    let body =
        hofund_plugin_sdk::json::json!({"clientId": client_id(pending.site_kind)}).to_string();
    let reply = Call::post(format!(
        "{}/api/v1/me/jobToken",
        pending.site.openapi.trim_end_matches('/')
    ))
    .header("Accept", "application/json")
    .header("Content-Type", "application/json")
    .sensitive_header(
        "Authorization",
        format!("Bearer {}", secret_text(&device.token)),
    )
    .bytes(body)
    .fetch(host.http())
    .await?;
    if reply.status() == 200 {
        let value: Value = hofund_plugin_sdk::json::from_slice(reply.body())
            .map_err(|_| PluginError::decode("Qoder job token response is invalid"))?;
        let token = required_token(&value, "Qoder job token response")?;
        let refresh = value
            .get("refresh_token")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        return Ok((
            ChatToken {
                token,
                refresh: SecretString::from(refresh),
                expires_at: job_expiry(host, &value),
            },
            false,
        ));
    }
    if pending.site_kind == Site::China && (400..500).contains(&reply.status()) {
        return Ok((
            ChatToken {
                token: device.token.clone(),
                refresh: device.refresh.clone(),
                expires_at: device_expiry(host, &device.value),
            },
            true,
        ));
    }
    Err(PluginError::status_error(
        reply.status(),
        "Qoder did not exchange the device token for a job token",
    ))
}

async fn user_info(host: &HostContext, pending: &Pending, token: &str) -> Value {
    let reply = Call::get(format!(
        "{}/api/v1/userinfo",
        pending.site.openapi.trim_end_matches('/')
    ))
    .header("Accept", "application/json")
    .sensitive_header("Authorization", format!("Bearer {token}"))
    .fetch(host.http())
    .await;
    reply
        .ok()
        .filter(|reply| (200..300).contains(&reply.status()))
        .and_then(|reply| hofund_plugin_sdk::json::from_slice(reply.body()).ok())
        .unwrap_or_else(|| hofund_plugin_sdk::json::json!({}))
}

fn required_token(value: &Value, context: &str) -> Result<SecretString, PluginError> {
    value
        .get("token")
        .or_else(|| value.get("device_token"))
        .or_else(|| value.get("access_token"))
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .map(|token| SecretString::from(token.to_owned()))
        .ok_or_else(|| PluginError::auth(format!("{context} has no token")))
}

pub(crate) fn refresh_token(value: &Value) -> Result<SecretString, PluginError> {
    required_token(value, "Qoder device token refresh")
}

pub(crate) fn refresh_secret(value: &Value) -> Result<SecretString, PluginError> {
    value
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .map(|token| SecretString::from(token.to_owned()))
        .ok_or_else(|| PluginError::auth("Qoder token refresh has no refresh token"))
}

pub(crate) fn job_expiry(host: &HostContext, value: &Value) -> i64 {
    for key in ["expire_time", "expires_at"] {
        if let Some(expiry) = value.get(key).and_then(when_said)
            && expiry > now_ms(host)
        {
            return expiry;
        }
    }
    if let Some(duration) = value.get("expires_in").and_then(number_i64)
        && duration > 0
    {
        return now_ms(host).saturating_add(duration);
    }
    now_ms(host).saturating_add(86_400_000)
}

pub(crate) fn device_expiry(host: &HostContext, value: &Value) -> i64 {
    if let Some(expiry) = value.get("expires_at").and_then(date_said) {
        return expiry;
    }
    if let Some(seconds) = value.get("expires_in").and_then(number_i64)
        && seconds > 0
    {
        return now_ms(host).saturating_add(seconds.saturating_mul(1000));
    }
    now_ms(host).saturating_add(86_400_000)
}

fn when_said(value: &Value) -> Option<i64> {
    if let Some(number) = number_i64(value) {
        if number <= 0 {
            return None;
        }
        return Some(if number < 1_000_000_000_000 {
            number.saturating_mul(1000)
        } else {
            number
        });
    }
    value.as_str().and_then(date_said_str)
}

fn date_said(value: &Value) -> Option<i64> {
    value.as_str().and_then(date_said_str)
}

fn date_said_str(value: &str) -> Option<i64> {
    hofund_plugin_sdk::chrono::DateTime::parse_from_rfc3339(value.trim())
        .ok()
        .map(|date| date.timestamp_millis())
}

fn number_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|number| i64::try_from(number).ok()))
        .or_else(|| value.as_str()?.trim().parse().ok())
}

fn now_ms(host: &HostContext) -> i64 {
    host.clock()
        .now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn client_id(site: Site) -> &'static str {
    match site {
        Site::Global => GLOBAL_CLIENT,
        Site::China => CN_CLIENT,
    }
}

fn first_string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    })
}
