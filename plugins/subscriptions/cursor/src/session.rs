//! Resolving the access token for an account.

use std::path::Path;

use crate::cli;
use crate::tokens::{self, is_agent};
use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};

/// Returns a bearer token for API calls.
pub(crate) async fn access_token(
    host: &HostContext,
    account: &AccountId,
    agent_home: &Path,
    program: &Path,
) -> Result<SecretString, PluginError> {
    let secret = host
        .accounts()
        .secret(account)
        .await?
        .ok_or_else(|| PluginError::auth("no Cursor sign-in is stored for this account"))?;
    let blob = tokens::parse_blob(&secret)?;
    if is_agent(&blob) {
        return agent_token(host, agent_home, program).await;
    }
    let access = blob
        .secret(tokens::ACCESS)
        .ok_or_else(|| PluginError::auth("the stored Cursor sign-in has no access token"))?;
    let now = host.clock().now();
    if tokens::near_expiry(now, tokens::expires_ms(&blob)) {
        crate::refresh::oauth_check(host, account, &access).await?;
    }
    Ok(access)
}

async fn agent_token(
    host: &HostContext,
    agent_home: &Path,
    program: &Path,
) -> Result<SecretString, PluginError> {
    let now = host.clock().now();
    let mut token = cli::live_access(host, agent_home, false)
        .await?
        .unwrap_or_default();
    if token.is_empty() || tokens::near_expiry(now, tokens::jwt_expiry_ms(&token)) {
        crate::refresh::agent_status(host, program, agent_home).await?;
        token = cli::live_access(host, agent_home, true)
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
