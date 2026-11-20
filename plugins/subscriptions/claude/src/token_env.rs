//! Claude Code's own `settings.json` env for a setup-token account.
//!
//! Claude Code reads `CLAUDE_CODE_OAUTH_TOKEN` from the `env` object of its
//! user settings ahead of the sign-in it has stored. Switching to a
//! setup-token account writes the token there. Switching to a signed-in
//! account, or removing the setup-token account that is in use, takes that
//! value out again.
//!
//! A value this process did not write is left in place. The hash of the
//! value written last is kept in `claude-token-env.json` under the plugin
//! state directory. The token itself is never written there and never logged.
//! The value counts as ours only while that hash still matches.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use hofund_plugin_api::{AccountId, HostContext, PluginError};
use hofund_plugin_sdk::digest::sha256;
use hofund_plugin_sdk::json::{Map, Value};
use hofund_plugin_sdk::oauth::{SecretObject, constant_time_eq};

use crate::creds;

const ENV_KEY: &str = "CLAUDE_CODE_OAUTH_TOKEN";
const MARKER_FILE: &str = "claude-token-env.json";
const SETTINGS_FILE: &str = "settings.json";
const FILE_CAP: u64 = 1024 * 1024;

static GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

struct Marker {
    hash: String,
    dir: PathBuf,
}

struct Settings {
    present: bool,
    value: String,
    root: Value,
}

/// Writes `token` into `home`'s `settings.json` env when the current value is
/// missing or one this process wrote.
pub(crate) fn set_token(host: &HostContext, home: &Path, token: &str) -> Result<(), PluginError> {
    let _gate = gate();
    let settings = settings_path(home);
    let parsed = match read_settings(&settings) {
        Ok(parsed) => parsed,
        Err(error) => {
            return Err(PluginError::io(format!(
                "{error}; Claude Code is left on the account it is on"
            )));
        }
    };
    let marker = read_marker(host)?;
    if user_owns(&marker, &parsed) {
        return Err(PluginError::config(own_message(&settings)));
    }
    write_marker(host, &token_hash(token), home)?;
    if parsed.present && constant_time_eq(parsed.value.as_bytes(), token.as_bytes()) {
        tighten(&resolve_target(&settings)?)?;
        return Ok(());
    }
    let target = resolve_target(&settings)?;
    tighten(&target)?;
    let mut root = parsed.root;
    upsert_token(&mut root, token);
    write_atomic(&target, &render(&root)?, "Claude Code's settings.json")
}

/// Removes the env value when it is the one recorded for `home`.
///
/// An unreadable settings file is left as it is and the marker stays, so a
/// later switch can still tell the value apart from one the user set.
pub(crate) fn clear_ours(host: &HostContext, home: &Path) -> Result<(), PluginError> {
    let _gate = gate();
    clear_home(host, home)
}

/// Removes the env value when `account` is the setup-token account recorded
/// as in use. A user-set value is left in place.
pub(crate) async fn clear_removed(
    host: &HostContext,
    account: &AccountId,
) -> Result<(), PluginError> {
    let Some(secret) = host.accounts().secret(account).await? else {
        return Ok(());
    };
    let Ok(blob) = SecretObject::parse(&secret) else {
        return Ok(());
    };
    let Some(token) = crate::setup_token::token_from_blob(&blob) else {
        return Ok(());
    };
    let _gate = gate();
    let marker = read_marker(host)?;
    if marker.hash.is_empty()
        || marker.hash != token_hash(&token)
        || marker.dir.as_os_str().is_empty()
    {
        return Ok(());
    }
    clear_home(host, &marker.dir)
}

fn clear_home(host: &HostContext, home: &Path) -> Result<(), PluginError> {
    let marker = read_marker(host)?;
    if marker.hash.is_empty() || !same_home(&marker.dir, home) {
        return Ok(());
    }
    let settings = settings_path(home);
    let parsed = read_settings(&settings)?;
    if !ours(&marker, &parsed) {
        return Ok(());
    }
    let target = resolve_target(&settings)?;
    let mut root = parsed.root;
    delete_token(&mut root);
    write_atomic(&target, &render(&root)?, "Claude Code's settings.json")?;
    remove_marker(host)
}

fn same_home(recorded: &Path, home: &Path) -> bool {
    !recorded.as_os_str().is_empty() && recorded == home
}

fn gate() -> std::sync::MutexGuard<'static, ()> {
    GATE.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn settings_path(home: &Path) -> PathBuf {
    home.join(SETTINGS_FILE)
}

fn marker_path(host: &HostContext) -> Result<PathBuf, PluginError> {
    Ok(host.dirs().state_dir()?.join(MARKER_FILE))
}

fn token_hash(token: &str) -> String {
    sha256(token.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn own_message(path: &Path) -> String {
    format!(
        "Claude Code's {} sets CLAUDE_CODE_OAUTH_TOKEN of your own, which Claude Code uses over any account hofund switches it to; hofund leaves it as it is. Take it out there to switch Claude Code here.",
        path.display()
    )
}

fn unreadable(path: &Path) -> PluginError {
    PluginError::io(format!("{} can't be read", path.display()))
}

fn user_owns(marker: &Marker, settings: &Settings) -> bool {
    settings.present && !settings.value.is_empty() && !ours(marker, settings)
}

fn ours(marker: &Marker, settings: &Settings) -> bool {
    settings.present && !marker.hash.is_empty() && token_hash(&settings.value) == marker.hash
}

fn read_marker(host: &HostContext) -> Result<Marker, PluginError> {
    let path = marker_path(host)?;
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Marker {
                hash: String::new(),
                dir: PathBuf::new(),
            });
        }
        Err(_) => return Err(unreadable(&path)),
    };
    let value: Value =
        hofund_plugin_sdk::json::from_slice(&bytes).map_err(|_| unreadable(&path))?;
    let Some(hash) = value.get("hash").and_then(Value::as_str) else {
        return Err(unreadable(&path));
    };
    let dir = value
        .get("dir")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_default();
    Ok(Marker {
        hash: hash.to_owned(),
        dir,
    })
}

fn write_marker(host: &HostContext, hash: &str, home: &Path) -> Result<(), PluginError> {
    let path = marker_path(host)?;
    let body = hofund_plugin_sdk::json::json!({
        "hash": hash,
        "dir": home,
    });
    write_atomic(&path, &render(&body)?, "the setup-token marker")
}

fn remove_marker(host: &HostContext) -> Result<(), PluginError> {
    let path = marker_path(host)?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(PluginError::io("cannot write the setup-token marker")),
    }
}

fn read_settings(path: &Path) -> Result<Settings, PluginError> {
    let target = resolve_target(path)?;
    let meta = match std::fs::metadata(&target) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Settings {
                present: false,
                value: String::new(),
                root: Value::Object(Map::new()),
            });
        }
        Err(_) => return Err(unreadable(path)),
    };
    if !meta.is_file() || meta.len() > FILE_CAP {
        return Err(unreadable(path));
    }
    let bytes = std::fs::read(&target).map_err(|_| unreadable(path))?;
    let text = String::from_utf8(bytes).map_err(|_| unreadable(path))?;
    let text = text.trim_start_matches('\u{feff}');
    if text.trim().is_empty() {
        return Ok(Settings {
            present: false,
            value: String::new(),
            root: Value::Object(Map::new()),
        });
    }
    let stripped = strip_jsonc(text);
    let root: Value = hofund_plugin_sdk::json::from_str(&stripped).map_err(|_| unreadable(path))?;
    if !root.is_object() {
        return Err(unreadable(path));
    }
    let mut present = false;
    let mut value = String::new();
    if let Some(env) = root.get("env").and_then(Value::as_object)
        && let Some(raw) = env.get(ENV_KEY)
    {
        present = true;
        value = match raw {
            Value::String(text) => text.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        };
    }
    Ok(Settings {
        present,
        value,
        root,
    })
}

fn upsert_token(root: &mut Value, token: &str) {
    if !root.is_object() {
        *root = Value::Object(Map::new());
    }
    let object = root.as_object_mut().expect("settings object");
    let env = object
        .entry(String::from("env"))
        .or_insert_with(|| Value::Object(Map::new()));
    if !env.is_object() {
        *env = Value::Object(Map::new());
    }
    env.as_object_mut()
        .expect("env object")
        .insert(ENV_KEY.to_owned(), Value::String(token.to_owned()));
}

fn delete_token(root: &mut Value) {
    if let Some(env) = root.get_mut("env").and_then(Value::as_object_mut) {
        env.remove(ENV_KEY);
    }
}

fn render(value: &Value) -> Result<Vec<u8>, PluginError> {
    let mut bytes = hofund_plugin_sdk::json::to_vec_pretty(value)
        .map_err(|_| PluginError::io("cannot write Claude Code's settings.json"))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn resolve_target(path: &Path) -> Result<PathBuf, PluginError> {
    let mut current = path.to_path_buf();
    for _ in 0..40 {
        match std::fs::symlink_metadata(&current) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(current),
            Err(_) => return Err(unreadable(path)),
            Ok(meta) if meta.file_type().is_symlink() => {
                let dest = std::fs::read_link(&current).map_err(|_| unreadable(path))?;
                current = if dest.is_absolute() {
                    dest
                } else {
                    current
                        .parent()
                        .map(|parent| parent.join(&dest))
                        .unwrap_or(dest)
                };
            }
            Ok(_) => return Ok(current),
        }
    }
    Err(unreadable(path))
}

fn tighten(path: &Path) -> Result<(), PluginError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(unreadable(path)),
            Ok(meta) => {
                let perm = meta.permissions().mode() & 0o777;
                if perm & 0o077 != 0 {
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(perm & !0o077))
                        .map_err(|_| unreadable(path))?;
                }
                Ok(())
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

fn write_atomic(path: &Path, bytes: &[u8], what: &str) -> Result<(), PluginError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        && !parent.exists()
    {
        creds::private_dir(parent)?;
    }
    let mut name = path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_else(|| std::ffi::OsString::from("settings.json"));
    name.push(format!(".{}.tmp", TEMP_SEQ.fetch_add(1, Ordering::Relaxed)));
    let temporary = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join(name);
    let failed = |_| PluginError::io(format!("cannot write {what}"));
    let write_result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
    })();
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temporary);
        return Err(failed(error));
    }
    Ok(())
}

/// Drops `//` and `/* */` comments and trailing commas outside strings.
fn strip_jsonc(input: &str) -> String {
    let mut without_comments = String::new();
    let mut chars = input.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;
    while let Some(character) = chars.next() {
        if in_string {
            without_comments.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        match character {
            '"' => {
                in_string = true;
                without_comments.push(character);
            }
            '/' if chars.peek() == Some(&'/') => {
                chars.next();
                for next in chars.by_ref() {
                    if next == '\n' {
                        without_comments.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut previous = '\0';
                for next in chars.by_ref() {
                    if previous == '*' && next == '/' {
                        break;
                    }
                    previous = next;
                }
            }
            _ => without_comments.push(character),
        }
    }
    let mut normalized = String::new();
    let mut chars = without_comments.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;
    while let Some(character) = chars.next() {
        if in_string {
            normalized.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        match character {
            '"' => {
                in_string = true;
                normalized.push(character);
            }
            ',' => {
                let rest: String = chars.clone().collect();
                if !rest.trim_start().starts_with(['}', ']']) {
                    normalized.push(',');
                }
            }
            other => normalized.push(other),
        }
    }
    normalized
}
