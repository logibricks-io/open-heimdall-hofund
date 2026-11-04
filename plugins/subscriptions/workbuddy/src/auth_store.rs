//! WorkBuddy desktop auth file locations (CodeBuddyExtension data dir).

use std::path::{Path, PathBuf};

use hofund_plugin_api::PluginError;

use crate::site::Site;

const EXTENSION_DATA_ENV: &str = "HOFUND_CODEBUDDY_EXTENSION_DATA";

#[cfg(test)]
static TEST_EXTENSION_DATA: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// Overrides the extension data directory in unit tests.
#[cfg(test)]
pub(crate) fn set_test_extension_data(path: Option<PathBuf>) {
    *TEST_EXTENSION_DATA
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = path;
}

/// Directory that holds `Public/auth/*.info` for both WorkBuddy builds.
pub(crate) fn extension_data_dir() -> Result<PathBuf, PluginError> {
    #[cfg(test)]
    if let Some(path) = TEST_EXTENSION_DATA
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
    {
        return Ok(path);
    }
    if let Ok(dir) = std::env::var(EXTENSION_DATA_ENV) {
        return Ok(PathBuf::from(dir));
    }
    let home = user_home()?;
    Ok(default_extension_data_dir(&home))
}

fn default_extension_data_dir(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/CodeBuddyExtension/Data")
    } else if cfg!(windows) {
        home.join("AppData/Local/CodeBuddyExtension/Data")
    } else {
        home.join(".local/share/CodeBuddyExtension/Data")
    }
}

fn user_home() -> Result<PathBuf, PluginError> {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .map_err(|_| PluginError::io("cannot resolve the user home for WorkBuddy auth"))
}

/// Path to one build's desktop auth file (`workbuddy-desktop.info` or `-ai`).
pub(crate) fn auth_path(site: Site) -> Result<PathBuf, PluginError> {
    Ok(extension_data_dir()?
        .join("Public")
        .join("auth")
        .join(site.auth_file()))
}
