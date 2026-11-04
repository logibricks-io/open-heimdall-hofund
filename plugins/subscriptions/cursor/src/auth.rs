//! Where cursor-agent keeps its sign-in and how it is read.

use std::path::{Path, PathBuf};

use hofund_plugin_api::PluginError;
use hofund_plugin_sdk::json::Value;

/// Root field in auth.json for the access token.
pub(crate) const ACCESS_FIELD: &str = "accessToken";

const FILE_CAP: u64 = 256 * 1024;

/// Agent config directory from `CURSOR_CONFIG_DIR` or the resolved agent home.
pub(crate) fn config_dir(agent_home: &Path) -> PathBuf {
    configured_dir().unwrap_or_else(|| agent_home.to_path_buf())
}

/// `auth.json` for cursor-agent on this OS.
pub(crate) fn auth_path(agent_home: &Path) -> PathBuf {
    if cfg!(target_os = "linux") && configured_dir().is_none() {
        let base = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home_dir().join(".config"));
        return base.join("cursor").join("auth.json");
    }
    if cfg!(target_os = "windows") {
        let base = std::env::var("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home_dir().join("AppData").join("Roaming"));
        return base.join("Cursor").join("auth.json");
    }
    config_dir(agent_home).join("auth.json")
}

/// Default cursor-agent home for API calls when no agent path is passed in.
pub(crate) fn default_agent_home() -> PathBuf {
    if let Some(dir) = configured_dir() {
        return dir;
    }
    home_dir().join(".cursor")
}

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/"))
}

/// Directory from `CURSOR_CONFIG_DIR`, when set.
pub(crate) fn configured_dir() -> Option<PathBuf> {
    #[cfg(test)]
    {
        if let Some(path) = TEST_CONFIGURED_DIR.with(|slot| slot.borrow().clone()) {
            return Some(path);
        }
    }
    std::env::var("CURSOR_CONFIG_DIR")
        .ok()
        .map(|value| value.trim().into())
        .filter(|path: &PathBuf| !path.as_os_str().is_empty())
}

#[cfg(test)]
thread_local! {
    static TEST_CONFIGURED_DIR: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Overrides [`configured_dir`] on this test thread.
#[cfg(test)]
pub(crate) fn test_configured_dir(path: Option<PathBuf>) {
    TEST_CONFIGURED_DIR.with(|slot| *slot.borrow_mut() = path);
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

/// Reads the live agent token from auth.json under `home`.
pub(crate) fn read_agent_access(home: &Path) -> Option<String> {
    read_access_file(&auth_path(home))
}

/// Writes auth.json for agent sign-in export.
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
