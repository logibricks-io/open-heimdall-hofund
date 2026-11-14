//! Resolving the access token for an account.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::cli::KeychainCache;
use crate::refresh::StatusCoalescer;
use crate::tokens::{self, is_agent};
use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};

/// Returns a bearer token for API calls.
#[allow(
    clippy::too_many_arguments,
    reason = "callers pass agent home, CLI program, keychain, and status coalescer explicitly"
)]
pub(crate) async fn access_token(
    host: &HostContext,
    endpoints: &crate::endpoints::Endpoints,
    account: &AccountId,
    agent_home: &Path,
    program: Option<PathBuf>,
    keychain: &Mutex<KeychainCache>,
    keychain_enabled: bool,
    status: &StatusCoalescer,
) -> Result<SecretString, PluginError> {
    let secret = host
        .accounts()
        .secret(account)
        .await?
        .ok_or_else(|| PluginError::auth("no Cursor sign-in is stored for this account"))?;
    let blob = tokens::parse_blob(&secret)?;
    if tokens::is_api_key(&blob) {
        return crate::exchange::access_token(host, endpoints, account).await;
    }
    if is_agent(&blob) {
        return crate::refresh::renew_agent(
            host,
            account,
            program.as_deref(),
            agent_home,
            keychain,
            keychain_enabled,
            status,
        )
        .await;
    }
    let access = blob
        .secret(tokens::ACCESS)
        .ok_or_else(|| PluginError::auth("the stored Cursor sign-in has no access token"))?;
    let now = host.clock().now();
    if tokens::near_expiry(now, tokens::effective_expires_ms(&blob)) {
        crate::refresh::oauth_check(host, endpoints, account).await?;
        let secret = host
            .accounts()
            .secret(account)
            .await?
            .ok_or_else(|| PluginError::auth("no Cursor sign-in is stored for this account"))?;
        let blob = tokens::parse_blob(&secret)?;
        return blob
            .secret(tokens::ACCESS)
            .ok_or_else(|| PluginError::auth("the stored Cursor sign-in has no access token"));
    }
    Ok(access)
}
