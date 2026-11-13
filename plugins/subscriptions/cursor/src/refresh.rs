//! Refreshing sign-ins: cursor-agent status and stored OAuth checks.

use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::oauth::secret_text;
use hofund_plugin_sdk::timeout;

use crate::cli::{self, KeychainCache, STATUS_WAIT};
use crate::connect;
use crate::endpoints::Endpoints;
use crate::tokens::{self, REFRESH};

/// How long a renew waits for the per-account lease.
const LEASE_WAIT: Duration = Duration::from_secs(30);

/// Runs `cursor-agent status` so the CLI refreshes an almost-expired token.
pub(crate) async fn agent_status(
    host: &HostContext,
    program: &Path,
    dir: &Path,
) -> Result<(), PluginError> {
    let request = cli::request(program, &["status"], dir)?;
    let child = host.spawn_process().spawn(&request).await?;
    match timeout(STATUS_WAIT, host.spawn_process().wait(child)).await {
        Some(Ok(0)) => Ok(()),
        Some(Ok(_)) => Err(PluginError::auth(
            "cursor-agent status did not finish successfully",
        )),
        Some(Err(error)) => Err(error),
        None => Err(PluginError::timeout(
            "cursor-agent status did not finish before the time limit",
        )),
    }
}

/// Renews a stored OAuth access token when it is near expiry.
pub(crate) async fn renew_agent(
    host: &HostContext,
    account: &AccountId,
    program: Option<&Path>,
    agent_home: &Path,
    keychain: &Mutex<KeychainCache>,
    keychain_enabled: bool,
) -> Result<SecretString, PluginError> {
    let _lease = host.accounts().lease(account, LEASE_WAIT).await?;
    let now = host.clock().now();
    let mut token = cli::live_access(host, agent_home, false, keychain, keychain_enabled)
        .await?
        .unwrap_or_default();
    if token.is_empty() || tokens::near_expiry(now, tokens::jwt_expiry_ms(&token)) {
        let program = program.ok_or_else(|| {
            PluginError::config(
                "cursor-agent is not installed: it is not on PATH; install it or set `command`",
            )
        })?;
        agent_status(host, program, agent_home).await?;
        token = cli::live_access(host, agent_home, true, keychain, keychain_enabled)
            .await?
            .unwrap_or_default();
    }
    if token.is_empty() {
        return Err(PluginError::auth(
            "cursor-agent is not signed in; run cursor-agent login",
        ));
    }
    if let Some(exp) = tokens::jwt_expiry_ms(&token) {
        let expires = tokens::ms_from_now(
            std::time::UNIX_EPOCH,
            i64::try_from(exp).unwrap_or(i64::MAX),
        );
        if now >= expires {
            return Err(PluginError::auth(
                "cursor-agent's sign-in has run out; sign in again",
            ));
        }
    }
    Ok(SecretString::from(token))
}

/// Renews a stored OAuth access token when it is near expiry.
pub(crate) async fn oauth_check(
    host: &HostContext,
    endpoints: &Endpoints,
    account: &AccountId,
) -> Result<(), PluginError> {
    let lease = host.accounts().lease(account, LEASE_WAIT).await?;
    let blob = tokens::parse_blob(lease.secret())?;
    if tokens::is_agent(&blob) || tokens::is_api_key(&blob) {
        return Ok(());
    }
    let now = host.clock().now();
    let expires = tokens::effective_expires_ms(&blob);
    if !tokens::near_expiry(now, expires) {
        return Ok(());
    }
    let refresh = blob
        .secret(REFRESH)
        .ok_or_else(|| PluginError::auth("the stored Cursor sign-in has no refresh token"))?;
    if secret_text(&refresh) == "none" {
        return Err(PluginError::auth(
            "the stored Cursor sign-in has no refresh token",
        ));
    }
    let url = endpoints.oauth_refresh();
    let body = format!(
        r#"{{"refreshToken":"{}"}}"#,
        escape_json(secret_text(&refresh))
    );
    let reply = Call::post(url)
        .header("content-type", "application/json")
        .header("x-cursor-client-version", &endpoints.client_version)
        .header("x-cursor-client-type", "cli")
        .bytes(body.into_bytes())
        .fetch(host.http())
        .await?;
    if !(200..300).contains(&reply.status()) {
        return Err(PluginError::auth(format!(
            "Cursor refused to renew the sign-in (HTTP {})",
            reply.status()
        )));
    }
    let value: Value = hofund_plugin_sdk::json::from_slice(reply.body())
        .map_err(|_| PluginError::decode("Cursor sent unreadable refresh JSON"))?;
    let access = value
        .get("accessToken")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| PluginError::auth("Cursor sent back no access token"))?;
    let access_secret = SecretString::from(access.to_owned());
    let next_refresh = value
        .get("refreshToken")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .map(SecretString::from)
        .unwrap_or(refresh);
    let label = blob.text(tokens::LABEL).unwrap_or("Cursor").to_owned();
    let plan = blob.text(tokens::PLAN);
    let expires_ms = tokens::jwt_expiry_ms(access)
        .unwrap_or_else(|| tokens::unix_ms(host.clock().now()) as u64 + 30 * 24 * 3600 * 1000);
    let updated = tokens::blob_oauth(&access_secret, &next_refresh, expires_ms, &label, plan);
    lease.commit(Some(label), updated.to_secret()).await?;
    let _ = connect::who_is(host, endpoints, &access_secret).await;
    Ok(())
}

fn escape_json(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}
