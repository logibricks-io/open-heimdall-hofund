//! WorkBuddy desktop auth file locations (CodeBuddyExtension data dir).

use std::path::{Path, PathBuf};

use crate::site::Site;

/// Relative path from the user home to CodeBuddyExtension data (matches agent sign-in).
#[cfg(target_os = "macos")]
pub(crate) const AGENT_HOME_DIR: &str = "Library/Application Support/CodeBuddyExtension/Data";
#[cfg(windows)]
pub(crate) const AGENT_HOME_DIR: &str = "AppData/Local/CodeBuddyExtension/Data";
#[cfg(not(any(windows, target_os = "macos")))]
pub(crate) const AGENT_HOME_DIR: &str = ".local/share/CodeBuddyExtension/Data";

/// Auth file under a CodeBuddyExtension data root (daemon agent home or OS default).
pub(crate) fn auth_path_in(data_root: &Path, site: Site) -> PathBuf {
    data_root.join("Public").join("auth").join(site.auth_file())
}
