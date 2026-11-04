//! Devin CLI credentials.toml and stored account secrets.

use std::path::{Path, PathBuf};

use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::oauth::{SecretObject, account_id_from};

const FILE_CAP: u64 = 64 * 1024;

/// Parsed sign-in from credentials.toml.
#[derive(Clone, Debug)]
pub(crate) struct Credential {
    pub key: SecretString,
    pub api_server: String,
    pub devin_api: String,
    pub label: String,
}

/// Stored account metadata.
#[derive(Clone, Debug)]
pub(crate) struct Stored {
    pub credential: Credential,
}

impl Stored {
    pub(crate) async fn load(host: &HostContext, account: &AccountId) -> Result<Self, PluginError> {
        let Some(secret) = host.accounts().secret(account).await? else {
            return Err(PluginError::auth(
                "no Devin sign-in is stored for this account",
            ));
        };
        parse_secret(&secret)
    }
}

pub(crate) fn parse_secret(secret: &SecretString) -> Result<Stored, PluginError> {
    let blob = SecretObject::parse(secret)?;
    let key = blob
        .secret("api_key")
        .ok_or_else(|| PluginError::auth("the stored Devin sign-in has no API key"))?;
    let api_server = blob
        .text("api_server")
        .filter(|value| !value.is_empty())
        .unwrap_or("https://server.codeium.com")
        .to_owned();
    let devin_api = blob
        .text("devin_api")
        .filter(|value| !value.is_empty())
        .unwrap_or("https://api.devin.ai")
        .to_owned();
    let label = blob
        .text("label")
        .filter(|value| !value.is_empty())
        .unwrap_or("Devin")
        .to_owned();
    Ok(Stored {
        credential: Credential {
            key,
            api_server,
            devin_api,
            label,
        },
    })
}

pub(crate) fn secret_blob(credential: &Credential) -> SecretObject {
    SecretObject::new()
        .with_number("v", 1)
        .with_secret("api_key", &credential.key)
        .with_text("api_server", &credential.api_server)
        .with_text("devin_api", &credential.devin_api)
        .with_text("label", &credential.label)
}

pub(crate) fn account_id(credential: &Credential) -> AccountId {
    account_id_from(&["devin", &credential.label])
}

pub(crate) fn read_credentials(path: &Path) -> Result<Credential, PluginError> {
    let meta = std::fs::metadata(path)
        .map_err(|_| PluginError::auth("Devin has no credentials file at that path"))?;
    if !meta.is_file() || meta.len() > FILE_CAP {
        return Err(PluginError::auth(
            "Devin credentials file is missing or too large",
        ));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|_| PluginError::auth("Devin credentials file could not be read"))?;
    parse_credentials_toml(&text)
}

pub(crate) fn parse_credentials_toml(text: &str) -> Result<Credential, PluginError> {
    let mut key = String::new();
    let mut api_server = String::new();
    let mut devin_api = String::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or(line).trim();
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        let name = name.trim();
        let value = parse_toml_string(value.trim());
        match name {
            "windsurf_api_key" => key = value,
            "api_server_url" => api_server = value,
            "devin_api_url" => devin_api = value,
            _ => {}
        }
    }
    if key.is_empty() {
        return Err(PluginError::auth(
            "Devin credentials file has no windsurf_api_key",
        ));
    }
    if api_server.is_empty() {
        api_server = "https://server.codeium.com".to_owned();
    }
    if devin_api.is_empty() {
        devin_api = "https://api.devin.ai".to_owned();
    }
    Ok(Credential {
        key: SecretString::from(key),
        api_server,
        devin_api,
        label: "Devin".to_owned(),
    })
}

fn parse_toml_string(raw: &str) -> String {
    let raw = raw.trim();
    if raw.starts_with('"') && raw.ends_with('"') && raw.len() >= 2 {
        raw[1..raw.len() - 1]
            .replace("\\\"", "\"")
            .replace("\\\\", "\\")
    } else {
        raw.to_owned()
    }
}

pub(crate) fn default_credentials_path() -> Result<PathBuf, PluginError> {
    let base = std::env::var("XDG_DATA_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|home| PathBuf::from(home).join(".local").join("share"))
        })
        .ok_or_else(|| PluginError::io("cannot resolve the Devin credentials directory"))?;
    Ok(base.join("devin").join("credentials.toml"))
}

pub(crate) fn write_credentials(path: &Path, credential: &Credential) -> Result<(), PluginError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|_| PluginError::io("cannot create the Devin credentials directory"))?;
    }
    let body = format!(
        "windsurf_api_key = \"{}\"\napi_server_url = \"{}\"\ndevin_api_url = \"{}\"\n",
        escape_toml(hofund_plugin_sdk::oauth::secret_text(&credential.key)),
        escape_toml(&credential.api_server),
        escape_toml(&credential.devin_api),
    );
    std::fs::write(path, body).map_err(|_| PluginError::io("cannot write Devin credentials"))?;
    Ok(())
}

fn escape_toml(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}
