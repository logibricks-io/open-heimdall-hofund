//! ChatGPT sign-in by device code, as Codex CLI `--device-auth` does: ask for a
//! code on another machine's browser, poll until it is entered, then trade the
//! authorization code at the token endpoint with the device callback redirect.

use std::time::Duration;

use hofund_plugin_api::{HostContext, PluginError, SecretString};
use hofund_plugin_sdk::json::{self, Map, Value};
use hofund_plugin_sdk::{Call, sleep};

use crate::sign_in;
use crate::{CLIENT_ID, CODEX_VERSION};
use crate::upstream;

/// Sign-in context option selecting the device-code flow.
pub(crate) const SITE_OPTION: &str = "site";

/// The site value that starts device sign-in.
pub(crate) const DEVICE_SITE: &str = "device";

/// How long ChatGPT keeps a device code valid.
pub(crate) const DEVICE_WAIT: Duration = Duration::from_secs(15 * 60);

/// Failed polls in a row that end the sign-in.
const MAX_POLL_MISSES: u32 = 5;

const USERCODE_PATH: &str = "/api/accounts/deviceauth/usercode";
const TOKEN_PATH: &str = "/api/accounts/deviceauth/token";
const DEVICE_PAGE_PATH: &str = "/codex/device";
const CALLBACK_PATH: &str = "/deviceauth/callback";

/// A device authorization in progress.
pub(crate) struct DevicePending {
    pub(crate) device_auth_id: String,
    pub(crate) user_code: String,
    pub(crate) interval: Duration,
    pub(crate) poll_url: String,
    pub(crate) oauth_token: String,
    pub(crate) issuer: String,
}

/// Starts device sign-in: asks for a user code and returns the pending state.
///
/// # Errors
///
/// Returns [`PluginError::Auth`] when ChatGPT turns device codes away (HTTP
/// 404 on the user-code endpoint).
pub(crate) async fn start(
    host: &HostContext,
    issuer: &str,
    oauth_token: &str,
) -> Result<DevicePending, PluginError> {
    let base = issuer.trim_end_matches('/');
    let url = format!("{base}{USERCODE_PATH}");
    let body = json::json!({ "client_id": CLIENT_ID });
    let reply = device_post(url, body).fetch(host.http()).await?;
    if reply.status() == 404 {
        return Err(PluginError::auth(
            "ChatGPT doesn't take a device code for this sign-in now; use the browser sign-in",
        ));
    }
    if !(200..300).contains(&reply.status()) {
        return Err(PluginError::status_error(
            reply.status(),
            format!(
                "ChatGPT gave no sign-in code ({})",
                String::from_utf8_lossy(reply.body())
            ),
        ));
    }
    let Ok(Value::Object(object)) = json::from_slice::<Value>(reply.body()) else {
        return Err(PluginError::decode("ChatGPT's sign-in code didn't read"));
    };
    let device_auth_id = field(&object, "device_auth_id")?;
    let user_code = field(&object, "user_code")
        .or_else(|_| field(&object, "usercode"))
        .map_err(|_| PluginError::decode("ChatGPT gave no sign-in code"))?;
    let interval = poll_interval(&object);
    Ok(DevicePending {
        device_auth_id,
        user_code,
        interval,
        poll_url: format!("{base}{TOKEN_PATH}"),
        oauth_token: oauth_token.to_owned(),
        issuer: base.to_owned(),
    })
}

/// The page where the user enters the code.
pub(crate) fn device_page(issuer: &str) -> String {
    format!("{}{DEVICE_PAGE_PATH}", issuer.trim_end_matches('/'))
}

/// Polls until the code is entered, then trades it for tokens.
pub(crate) async fn finish(
    host: &HostContext,
    pending: &DevicePending,
) -> Result<sign_in::SignedIn, PluginError> {
    let clock = host.clock();
    let deadline = clock.monotonic().saturating_add(DEVICE_WAIT);
    let interval = pending.interval;
    let mut misses = 0_u32;
    loop {
        if clock.monotonic() >= deadline {
            return Err(PluginError::auth("the code expired; start again"));
        }
        sleep(interval).await;
        if clock.monotonic() >= deadline {
            return Err(PluginError::auth("the code expired; start again"));
        }
        let body = json::json!({
            "device_auth_id": pending.device_auth_id,
            "user_code": pending.user_code,
        });
        let reply = match device_post(pending.poll_url.clone(), body)
            .fetch(host.http())
            .await
        {
            Ok(reply) => reply,
            Err(error) => {
                misses = misses.saturating_add(1);
                if misses >= MAX_POLL_MISSES {
                    return Err(PluginError::connect(format!(
                        "couldn't ask ChatGPT whether the code was entered: {error}"
                    )));
                }
                continue;
            }
        };
        match reply.status() {
            403 | 404 => {
                misses = 0;
                continue;
            }
            200..=299 => {
                let trade = parse_trade(reply.body())?;
                let redirect_uri = format!("{}{CALLBACK_PATH}", pending.issuer);
                return sign_in::exchange(
                    host,
                    &pending.oauth_token,
                    &trade.code,
                    &trade.verifier,
                    &redirect_uri,
                )
                .await;
            }
            status => {
                return Err(PluginError::status_error(
                    status,
                    format!(
                        "ChatGPT refused the sign-in ({status}): {}",
                        String::from_utf8_lossy(reply.body())
                    ),
                ));
            }
        }
    }
}

struct Trade {
    code: SecretString,
    verifier: SecretString,
}

fn parse_trade(body: &[u8]) -> Result<Trade, PluginError> {
    let Ok(Value::Object(object)) = json::from_slice::<Value>(body) else {
        return Err(PluginError::decode(
            "ChatGPT finished the sign-in but sent no code",
        ));
    };
    let code = field_secret(&object, "authorization_code")?;
    let verifier = field_secret(&object, "code_verifier")?;
    Ok(Trade { code, verifier })
}

fn field(object: &Map<String, Value>, name: &str) -> Result<String, PluginError> {
    object
        .get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| PluginError::decode(format!("ChatGPT's sign-in code has no {name}")))
}

fn field_secret(
    object: &Map<String, Value>,
    name: &str,
) -> Result<SecretString, PluginError> {
    field(object, name).map(SecretString::from)
}

fn poll_interval(object: &Map<String, Value>) -> Duration {
    let secs = object.get("interval").and_then(|value| match value {
        Value::String(text) => text.trim().parse::<u64>().ok(),
        Value::Number(number) => number.as_u64(),
        _ => None,
    });
    match secs {
        Some(0) | None => Duration::from_secs(5),
        Some(secs) => Duration::from_secs(secs.max(1)),
    }
}

fn device_post(url: String, body: Value) -> Call {
    let bytes = json::to_vec(&body).unwrap_or_default();
    Call::post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .header("originator", "codex_cli_rs")
        .header("version", CODEX_VERSION)
        .sensitive_header(
            "user-agent",
            upstream::user_agent(std::env::consts::OS, std::env::consts::ARCH),
        )
        .bytes(bytes)
}
