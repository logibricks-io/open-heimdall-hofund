//! Per-account proxy env for Claude Code (Bun) spawns.

use std::ffi::OsString;

use hofund_plugin_api::{PluginError, SignInCtx};

use crate::launcher;

/// Account secret key for the proxy picked at sign-in.
pub(crate) const PROXY_KEY: &str = "proxy";

/// Whether a child can reach hofund's loopback SOCKS bridge.
#[must_use]
pub(crate) fn loopback_ok(command: &launcher::ClaudeCommand) -> bool {
    match command {
        launcher::ClaudeCommand::Native(_) => true,
        launcher::ClaudeCommand::Wsl(tool) => tool.host.is_empty(),
    }
}

/// Applies [`hofund_plugin_sdk::netproxy::env_for_bun_with`] for `choice`.
pub(crate) fn bun_proxy_env(
    choice: Option<&str>,
    command: &launcher::ClaudeCommand,
    env: &[(OsString, OsString)],
) -> Result<Vec<(OsString, OsString)>, PluginError> {
    hofund_plugin_sdk::netproxy::env_for_bun_with(choice.unwrap_or(""), env, loopback_ok(command))
}

/// Proxy choice on a sign-in context (`proxy` option).
#[must_use]
pub(crate) fn sign_in_choice(ctx: &SignInCtx) -> Option<&str> {
    ctx.option("proxy")
}
