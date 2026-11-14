//! One-shot migration from the legacy sibling `qoder.json` file.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use hofund_plugin_api::{HostContext, PluginError, SecretString};
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::uuid;

use crate::creds::{self, Credential, Site, Stored};
use crate::import;

static MIGRATE: Mutex<()> = Mutex::new(());

pub(crate) const UNREADABLE: &str = "Qoder: unreadable legacy sign-in";

/// Path to the legacy file beside the config directory (`hofund.toml` / the reference `providers.json`).
pub(crate) fn legacy_path(config_dir: &Path) -> PathBuf {
    config_dir.join("qoder.json")
}

#[cfg(test)]
std::thread_local! {
    static TEST_DISCOVERY_OFF: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(crate) struct TestDiscoveryOffGuard;

#[cfg(test)]
impl TestDiscoveryOffGuard {
    pub(crate) fn on() -> Self {
        TEST_DISCOVERY_OFF.set(Some(true));
        Self
    }
}

#[cfg(test)]
impl Drop for TestDiscoveryOffGuard {
    fn drop(&mut self) {
        TEST_DISCOVERY_OFF.set(None);
    }
}

fn discovery_blocked() -> bool {
    #[cfg(test)]
    if let Some(off) = TEST_DISCOVERY_OFF.get() {
        return off;
    }
    discovery_blocked_from_env(std::env::var("HOFUND_NO_ACCOUNTS").ok().as_deref())
}

fn discovery_blocked_from_env(value: Option<&str>) -> bool {
    matches!(
        value.map(str::trim),
        Some(value) if !value.is_empty() && value != "0" && !value.eq_ignore_ascii_case("false")
    )
}

/// Moves the legacy credential into the account store once, or removes the file when redundant.
pub(crate) async fn run(config_dir: &Path, host: &HostContext) -> Result<(), PluginError> {
    if discovery_blocked() {
        return Ok(());
    }
    let path = legacy_path(config_dir);
    if std::fs::metadata(&path).is_err() {
        return Ok(());
    }
    let credential = {
        let _guard = MIGRATE.lock().unwrap_or_else(|error| error.into_inner());
        if std::fs::metadata(&path).is_err() {
            return Ok(());
        }
        let bytes = std::fs::read(&path).map_err(|error| PluginError::io(error.to_string()))?;
        let credential = parse_legacy(&bytes)?;
        if credential.uid.is_empty() || secret_text(&credential.token).is_empty() {
            return Err(PluginError::auth(UNREADABLE));
        }
        let mut credential = credential;
        if credential.machine_id.is_empty() {
            credential.machine_id =
                uuid::v4().map_err(|_| PluginError::internal("the system random source failed"))?;
        }
        credential.site = Site::Global;
        credential
    };
    if same_uid_present(host, &credential.uid).await? {
        std::fs::remove_file(&path).map_err(|error| PluginError::io(error.to_string()))?;
        return Ok(());
    }
    import::store(&credential, host).await?;
    std::fs::remove_file(&path).map_err(|error| PluginError::io(error.to_string()))?;
    Ok(())
}

async fn same_uid_present(host: &HostContext, uid: &str) -> Result<bool, PluginError> {
    for account in host.accounts().list().await? {
        let Ok(Stored { credential }) = Stored::load(host, account.id()).await else {
            continue;
        };
        if credential.site == Site::Global && credential.uid == uid {
            return Ok(true);
        }
    }
    Ok(false)
}

fn parse_legacy(bytes: &[u8]) -> Result<Credential, PluginError> {
    creds::parse_json(std::str::from_utf8(bytes).map_err(|_| PluginError::auth(UNREADABLE))?)
        .or_else(|_| parse_legacy_relaxed(bytes))
}

fn parse_legacy_relaxed(bytes: &[u8]) -> Result<Credential, PluginError> {
    use hofund_plugin_sdk::json;

    let root: Value = json::from_slice(bytes).map_err(|_| PluginError::auth(UNREADABLE))?;
    let map = root
        .as_object()
        .ok_or_else(|| PluginError::auth(UNREADABLE))?;
    let uid = map
        .get("uid")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| PluginError::auth(UNREADABLE))?
        .to_owned();
    let token = map
        .get("token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| PluginError::auth(UNREADABLE))?;
    let refresh = map
        .get("refresh_token")
        .and_then(Value::as_str)
        .unwrap_or("");
    Ok(Credential {
        uid,
        email: optional_text(map, "email"),
        name: optional_text(map, "name"),
        token: SecretString::from(token.to_owned()),
        refresh_token: SecretString::from(refresh.to_owned()),
        expires_at: map.get("expires_at").and_then(json_number_i64).unwrap_or(0),
        machine_id: optional_text(map, "machine_id"),
        site: Site::Global,
        vpc: optional_text(map, "vpc"),
        device_chat: map
            .get("device_chat")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        device_token: SecretString::from(optional_text(map, "device_token")),
        device_refresh_token: SecretString::from(optional_text(map, "device_refresh")),
        pat_auth: false,
        pat: SecretString::from(String::new()),
    })
}

fn optional_text(map: &hofund_plugin_sdk::json::Map<String, Value>, key: &str) -> String {
    map.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

fn json_number_i64(value: &hofund_plugin_sdk::json::Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().map(|n| n as i64))
        .or_else(|| value.as_f64().map(|n| n as i64))
}

fn secret_text(secret: &SecretString) -> &str {
    hofund_plugin_sdk::oauth::secret_text(secret)
}
