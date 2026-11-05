//! WorkBuddy desktop auth file locations (CodeBuddyExtension data dir).

use std::path::{Path, PathBuf};

use hofund_plugin_api::PluginError;

use crate::site::Site;

const EXTENSION_DATA_ENV: &str = "HOFUND_CODEBUDDY_EXTENSION_DATA";

/// Relative path from the user home to CodeBuddyExtension data (matches agent sign-in).
#[cfg(target_os = "macos")]
pub(crate) const AGENT_HOME_DIR: &str = "Library/Application Support/CodeBuddyExtension/Data";
#[cfg(windows)]
pub(crate) const AGENT_HOME_DIR: &str = "AppData/Local/CodeBuddyExtension/Data";
#[cfg(not(any(windows, target_os = "macos")))]
pub(crate) const AGENT_HOME_DIR: &str = ".local/share/CodeBuddyExtension/Data";

/// Directory that holds `Public/auth/*.info` for both WorkBuddy builds.
pub(crate) fn extension_data_dir() -> Result<PathBuf, PluginError> {
    if let Ok(dir) = std::env::var(EXTENSION_DATA_ENV) {
        return Ok(PathBuf::from(dir));
    }
    let home = user_home()?;
    Ok(default_extension_data_dir(&home))
}

fn default_extension_data_dir(home: &Path) -> PathBuf {
    home.join(AGENT_HOME_DIR)
}

fn user_home() -> Result<PathBuf, PluginError> {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .map_err(|_| PluginError::io("cannot resolve the user home for WorkBuddy auth"))
}

/// Auth file under a CodeBuddyExtension data root (daemon agent home or OS default).
pub(crate) fn auth_path_in(data_root: &Path, site: Site) -> PathBuf {
    data_root.join("Public").join("auth").join(site.auth_file())
}
