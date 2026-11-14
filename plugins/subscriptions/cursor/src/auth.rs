//! Where cursor-agent keeps its sign-in and how it is read.

use std::path::{Path, PathBuf};

#[cfg(test)]
use std::cell::RefCell;

use hofund_plugin_api::PluginError;
use hofund_plugin_sdk::json::Value;

/// Root field in auth.json for the access token.
pub(crate) const ACCESS_FIELD: &str = "accessToken";

const FILE_CAP: u64 = 256 * 1024;

/// `auth.json` under the agent home or platform default.
pub(crate) fn auth_path(agent_home: &Path) -> PathBuf {
    agent_home.join("auth.json")
}

/// Default cursor-agent config directory for API calls when no agent path is passed in.
pub(crate) fn default_agent_home() -> PathBuf {
    configured_dir().unwrap_or_else(cli_config_dir)
}

fn cli_config_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        let trimmed = xdg.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed).join("cursor");
        }
    }
    home_dir().join(".cursor")
}

fn auth_file_dir() -> PathBuf {
    if let Some(dir) = configured_dir() {
        return dir;
    }
    match std::env::consts::OS {
        "windows" => {
            let base = std::env::var("APPDATA")
                .map(PathBuf::from)
                .unwrap_or_else(|_| home_dir().join("AppData").join("Roaming"));
            base.join("Cursor")
        }
        "macos" => home_dir().join(".cursor"),
        _ => {
            let base = std::env::var("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| home_dir().join(".config"));
            base.join("cursor")
        }
    }
}

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/"))
}

/// Directory from `CURSOR_CONFIG_DIR`, when set.
pub(crate) fn configured_dir() -> Option<PathBuf> {
    test_config_dir().or_else(|| {
        std::env::var("CURSOR_CONFIG_DIR")
            .ok()
            .map(|value| value.trim().into())
            .filter(|path: &PathBuf| !path.as_os_str().is_empty())
    })
}

#[cfg(test)]
thread_local! {
    static TEST_CONFIG_DIR: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

#[cfg(test)]
fn test_config_dir() -> Option<PathBuf> {
    TEST_CONFIG_DIR.with(|cell| cell.borrow().clone())
}

/// Simulates `CURSOR_CONFIG_DIR` in the daemon environment during tests.
#[cfg(test)]
pub(crate) fn set_test_config_dir(dir: Option<PathBuf>) {
    TEST_CONFIG_DIR.with(|cell| *cell.borrow_mut() = dir);
}

#[cfg(not(test))]
fn test_config_dir() -> Option<PathBuf> {
    None
}

/// Reads `accessToken` from `path`, when the file is small enough.
pub(crate) fn read_access_file(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > FILE_CAP {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    parse_access_text(&text)
}

/// Parses auth.json text for `accessToken`.
pub(crate) fn parse_access_text(text: &str) -> Option<String> {
    let root = hofund_plugin_sdk::json::from_str::<Value>(text).ok()?;
    root.get(ACCESS_FIELD)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
}

/// Parses auth.json text for `refreshToken`.
pub(crate) fn parse_refresh_text(text: &str) -> Option<String> {
    let root = hofund_plugin_sdk::json::from_str::<Value>(text).ok()?;
    root.get("refreshToken")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
}

/// Reads the live agent token from auth.json.
pub(crate) fn read_agent_access(home: &Path) -> Option<String> {
    read_access_file(&auth_path(home))
        .or_else(|| read_access_file(&auth_file_dir().join("auth.json")))
}

/// Writes auth.json for agent sign-in export.
#[expect(dead_code, reason = "documented cursor-agent subprocess auth staging")]
pub(crate) fn write_auth_file(config: &Path, access: &str) -> Result<(), PluginError> {
    std::fs::create_dir_all(config)
        .map_err(|_| PluginError::io("cannot create the Cursor config directory"))?;
    let path = config.join("auth.json");
    let body = format!(r#"{{"{ACCESS_FIELD}":"{}"}}"#, escape_json(access));
    write_private_file(&path, body.as_bytes())
}

fn escape_json(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(unix)]
fn write_private_file(path: &Path, body: &[u8]) -> Result<(), PluginError> {
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|_| PluginError::io("cannot write Cursor auth.json"))?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|_| PluginError::io("cannot restrict Cursor auth.json"))?;
    file.write_all(body)
        .map_err(|_| PluginError::io("cannot write Cursor auth.json"))
}

#[cfg(not(unix))]
fn write_private_file(path: &Path, body: &[u8]) -> Result<(), PluginError> {
    std::fs::write(path, body).map_err(|_| PluginError::io("cannot write Cursor auth.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_auth_file_uses_xdg_config_cursor() {
        if std::env::consts::OS == "linux" {
            let dir = auth_file_dir();
            assert!(
                dir.ends_with("cursor"),
                "Linux auth dir should end with cursor, got {}",
                dir.display()
            );
        }
    }
}
