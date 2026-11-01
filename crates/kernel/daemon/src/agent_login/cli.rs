//! CLI entry for agent sign-in switching.

use std::path::Path;

use crate::{DaemonError, sign_in::with_opened};

use super::{accounts_import, accounts_switch};

/// Runs `hofund accounts switch <agent> <reference>`.
///
/// # Errors
///
/// Returns [`DaemonError`] when the switch fails.
pub fn accounts_switch_in(
    path: &Path,
    agent: &str,
    reference: &str,
) -> Result<String, DaemonError> {
    let config =
        hofund_config::load_path(path).map_err(|error| DaemonError::new(error.to_string()))?;
    with_opened(path, async |opened| {
        let state_root = opened
            .iter()
            .find_map(|entry| entry.host.dirs().state_dir().ok())
            .ok_or_else(|| DaemonError::new("cannot resolve the daemon state directory"))?;
        accounts_switch(
            &config,
            &crate::RegistryAdapter,
            &state_root,
            opened,
            agent,
            reference,
        )
        .await
    })
}

/// Runs `hofund accounts import <file>`.
///
/// # Errors
///
/// Returns [`DaemonError`] when the file cannot be read or no subscription stores it.
pub fn accounts_import_in(path: &Path, file: &Path) -> Result<Vec<String>, DaemonError> {
    let text = std::fs::read_to_string(file)
        .map_err(|error| DaemonError::new(format!("cannot read {}: {error}", file.display())))?;
    with_opened(path, async |opened| accounts_import(opened, &text).await)
}
