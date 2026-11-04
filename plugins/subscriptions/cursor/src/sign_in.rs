//! Browser sign-in through loginDeepControl and auth poll.

use std::time::Duration;

use hofund_plugin_api::{HostContext, PluginError, SecretString};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::oauth::{Pkce, SecretObject};

use crate::connect;
use crate::endpoints::Endpoints;
use crate::tokens;

/// Poll state kept until sign-in completes.
#[derive(Clone)]
pub(crate) struct BrowserPending {
    pub verifier: SecretString,
    pub uuid: String,
}

/// Starts browser sign-in: returns the page URL and poll state.
pub(crate) fn start(pkce: &Pkce, uuid: &str, endpoints: &Endpoints) -> (String, BrowserPending) {
    let url = endpoints.login_deep_control(&pkce.challenge, uuid);
    (
        url,
        BrowserPending {
            verifier: pkce.verifier.clone(),
            uuid: uuid.to_owned(),
        },
    )
}

/// Polls until Cursor returns tokens or gives up.
pub(crate) async fn finish(
    host: &HostContext,
    endpoints: &Endpoints,
    pending: &BrowserPending,
    alive: impl Fn() -> bool,
) -> Result<SecretObject, PluginError> {
    let poll_url = endpoints.auth_poll(
        &pending.uuid,
        hofund_plugin_sdk::oauth::secret_text(&pending.verifier),
    );
    let mut errors = 0_u32;
    for round in 0..150_u32 {
        if !alive() {
            return Err(PluginError::config("Cursor sign-in was cancelled"));
        }
        let wait = Duration::from_millis(
            (1000.0 * 1.2_f64.powi(i32::try_from(round).unwrap_or(0))).min(10_000.0) as u64,
        );
        let reply = Call::get(&poll_url)
            .header("content-type", "application/json")
            .header("x-cursor-client-version", &endpoints.client_version)
            .header("x-cursor-client-type", "cli")
            .fetch(host.http())
            .await;
        let reply = match reply {
            Ok(reply) => reply,
            Err(_) => {
                errors += 1;
                if errors >= 3 {
                    return Err(PluginError::connect(
                        "could not reach Cursor during sign-in",
                    ));
                }
                hofund_plugin_sdk::sleep(wait).await;
                continue;
            }
        };
        if reply.status() == 404 {
            errors = 0;
            hofund_plugin_sdk::sleep(wait).await;
            continue;
        }
        if !(200..300).contains(&reply.status()) {
            errors += 1;
            if reply.status() == 403 || errors >= 3 {
                return Err(PluginError::auth(format!(
                    "Cursor refused sign-in (HTTP {})",
                    reply.status()
                )));
            }
            hofund_plugin_sdk::sleep(wait).await;
            continue;
        }
        let body: Value = hofund_plugin_sdk::json::from_slice(reply.body())
            .map_err(|_| PluginError::decode("Cursor sent unreadable sign-in JSON"))?;
        let access = body
            .get("accessToken")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| PluginError::auth("Cursor sent back no access token"))?;
        if !body
            .as_object()
            .is_some_and(|obj| obj.contains_key("refreshToken"))
        {
            return Err(PluginError::auth("Cursor sent back no refresh token field"));
        }
        let refresh = body
            .get("refreshToken")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .unwrap_or("none");
        let access_secret = SecretString::from(access.to_owned());
        let refresh_secret = SecretString::from(refresh.to_owned());
        let (label, plan) = connect::who_is(host, endpoints, &access_secret).await?;
        let expires = tokens::jwt_expiry_ms(access)
            .unwrap_or_else(|| tokens::unix_ms(host.clock().now()) as u64 + 30 * 24 * 3600 * 1000);
        return Ok(tokens::blob_oauth(
            &access_secret,
            &refresh_secret,
            expires,
            &label,
            plan.as_deref(),
        ));
    }
    Err(PluginError::auth("Cursor sign-in timed out"))
}
