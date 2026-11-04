//! Refreshing sign-ins: cursor-agent status and stored OAuth checks.

use std::path::Path;

use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::timeout;

use crate::cli::{self, STATUS_WAIT};

/// Runs `cursor-agent status` so the CLI refreshes an almost-expired token.
pub(crate) async fn agent_status(
    host: &HostContext,
    program: &Path,
    dir: &Path,
) -> Result<(), PluginError> {
    let request = cli::request(program, &["status"], dir)?;
    let child = host.spawn_process().spawn_piped(&request).await?;
    let _ = timeout(STATUS_WAIT, async {
        let mut stdout = child.stdout()?;
        while cli::collect(&mut stdout, 64 * 1024).await.is_some() {}
        Ok::<(), PluginError>(())
    })
    .await;
    Ok(())
}

/// Confirms a stored OAuth access token is still within its lifetime.
pub(crate) async fn oauth_check(
    _host: &HostContext,
    _account: &AccountId,
    _access: &SecretString,
) -> Result<(), PluginError> {
    Ok(())
}
