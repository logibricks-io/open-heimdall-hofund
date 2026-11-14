//! Devin CLI credentials.toml and stored account secrets.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::digest;
use hofund_plugin_sdk::oauth::{SecretObject, account_id_from, secret_text};

const FILE_CAP: u64 = 64 * 1024;

/// Parsed sign-in from credentials.toml.
#[derive(Clone, Debug)]
pub(crate) struct Credential {
    pub key: SecretString,
    pub api_server: String,
    pub devin_api: String,
    pub label: String,
    pub plan: String,
    pub org_id: String,
    pub team_id: String,
}

/// Stored account metadata.
#[derive(Clone, Debug)]
pub(crate) struct Stored {
    pub credential: Credential,
    pub cli_path: Option<PathBuf>,
    pub needs_sign_in: bool,
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

    pub(crate) async fn live(host: &HostContext, account: &AccountId) -> Result<Self, PluginError> {
        let stored = Self::load(host, account).await?;
        if stored.cli_path.is_none() {
            return Ok(stored);
        }
        let lease = host
            .accounts()
            .lease(account, std::time::Duration::from_secs(30))
            .await?;
        let stored = parse_secret(lease.secret())?;
        let Some(path) = stored.cli_path.clone() else {
            return Ok(stored);
        };
        let Ok(mut latest) = read_credentials(&path) else {
            return Ok(stored);
        };
        latest.label = stored.credential.label;
        latest.plan = stored.credential.plan;
        latest.org_id = stored.credential.org_id;
        latest.team_id = stored.credential.team_id;
        latest.api_server = live_api_server(&latest.api_server);
        Ok(Stored {
            credential: latest,
            cli_path: Some(path),
            needs_sign_in: stored.needs_sign_in,
        })
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
        .map(str::to_owned)
        .unwrap_or_else(|| default_label(&key));
    let cli_path = if blob.text("cli") == Some("true") {
        blob.text("cli_path")
            .map(PathBuf::from)
            .or_else(|| default_credentials_path().ok())
    } else {
        None
    };
    Ok(Stored {
        credential: Credential {
            key,
            api_server,
            devin_api,
            label,
            plan: blob.text("plan").unwrap_or_default().to_owned(),
            org_id: blob.text("org_id").unwrap_or_default().to_owned(),
            team_id: blob.text("team_id").unwrap_or_default().to_owned(),
        },
        cli_path,
        needs_sign_in: blob.flag("needs_sign_in"),
    })
}

pub(crate) async fn mark_needs_sign_in(
    host: &HostContext,
    account: &AccountId,
) -> Result<(), PluginError> {
    let lease = host
        .accounts()
        .lease(account, std::time::Duration::from_secs(30))
        .await?;
    let blob = SecretObject::parse(lease.secret())?;
    lease
        .commit(None, blob.with_flag("needs_sign_in", true).to_secret())
        .await
}

pub(crate) fn secret_blob(credential: &Credential) -> SecretObject {
    let mut blob = SecretObject::new()
        .with_number("v", 1)
        .with_secret("api_key", &credential.key)
        .with_text("api_server", &credential.api_server)
        .with_text("devin_api", &credential.devin_api)
        .with_text("label", &credential.label);
    for (key, value) in [
        ("plan", &credential.plan),
        ("org_id", &credential.org_id),
        ("team_id", &credential.team_id),
    ] {
        if !value.is_empty() {
            blob = blob.with_text(key, value);
        }
    }
    blob
}

pub(crate) fn cli_secret_blob(credential: &Credential, path: &Path) -> SecretObject {
    secret_blob(credential)
        .with_text("cli", "true")
        .with_text("cli_path", &path.to_string_lossy())
}

/// Label-based account id for seeded accounts and tests.
#[allow(dead_code)]
pub(crate) fn account_id(credential: &Credential) -> AccountId {
    account_id_from(&["devin", &credential.label.to_ascii_lowercase()])
}

pub(crate) fn agent_account_id(home: &Path) -> AccountId {
    account_id_from(&[
        "devin",
        "agent",
        &hash_hex(home.to_string_lossy().as_bytes()),
    ])
}

#[allow(dead_code)]
pub(crate) fn cli_account_id() -> AccountId {
    account_id_from(&["devin", "cli"])
}

fn stable_key(key: &SecretString) -> String {
    hash_hex(secret_text(key).as_bytes())
}

fn hash_hex(data: &[u8]) -> String {
    digest::sha256(data)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn default_label(key: &SecretString) -> String {
    format!("Devin {}", &stable_key(key)[..8])
}

fn live_api_server(file_server: &str) -> String {
    std::env::var("WINDSURF_API_SERVER_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| file_server.to_owned())
        .trim_end_matches('/')
        .to_owned()
}

pub(crate) fn parse_credentials_toml(text: &str) -> Result<Credential, PluginError> {
    let mut windsurf_api_key = None;
    let mut api_server_url = None;
    let mut devin_api_url = None;
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = parse_toml_value(value.trim())?;
        match key {
            "windsurf_api_key" => windsurf_api_key = Some(value),
            "api_server_url" => api_server_url = Some(value),
            "devin_api_url" => devin_api_url = Some(value),
            _ => {}
        }
    }
    let key = windsurf_api_key
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| PluginError::auth("Devin credentials file has no windsurf_api_key"))?;
    let api_server = api_server_url
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "https://server.codeium.com".to_owned());
    let devin_api = devin_api_url
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "https://api.devin.ai".to_owned());
    let key = SecretString::from(key.to_owned());
    Ok(Credential {
        label: default_label(&key),
        key,
        api_server,
        devin_api,
        plan: String::new(),
        org_id: String::new(),
        team_id: String::new(),
    })
}

fn parse_toml_value(raw: &str) -> Result<String, PluginError> {
    if raw.starts_with('"') {
        if !raw.ends_with('"') || raw.len() < 2 {
            return Err(PluginError::decode(
                "Devin credentials file has an unterminated string",
            ));
        }
        return Ok(unescape_toml_string(&raw[1..raw.len() - 1]));
    }
    Ok(raw.to_owned())
}

fn unescape_toml_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('n') => out.push('\n'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(ch);
        }
    }
    out
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

pub(crate) fn default_credentials_path() -> Result<PathBuf, PluginError> {
    let base = if cfg!(windows) {
        std::env::var("APPDATA").ok().map(PathBuf::from)
    } else {
        std::env::var("XDG_DATA_HOME")
            .ok()
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var("HOME")
                    .ok()
                    .map(|home| PathBuf::from(home).join(".local").join("share"))
            })
    }
    .ok_or_else(|| PluginError::io("cannot resolve the Devin credentials directory"))?;
    Ok(base.join("devin").join("credentials.toml"))
}

/// Path to credentials.toml for an agent home directory.
pub(crate) fn agent_credentials_path(home: &Path) -> PathBuf {
    if home
        .file_name()
        .is_some_and(|name| name == std::ffi::OsStr::new("devin"))
    {
        home.join("credentials.toml")
    } else {
        home.join("devin").join("credentials.toml")
    }
}

pub(crate) fn write_credentials(path: &Path, credential: &Credential) -> Result<(), PluginError> {
    if let Some(parent) = path.parent() {
        private_dir(parent)?;
    }
    let body = format!(
        "windsurf_api_key = {}\napi_server_url = {}\ndevin_api_url = {}\n",
        toml_string(secret_text(&credential.key)),
        toml_string(&credential.api_server),
        toml_string(&credential.devin_api),
    );
    write_atomic(path, body.as_bytes())
}

fn toml_string(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
    )
}

fn private_dir(dir: &Path) -> Result<(), PluginError> {
    std::fs::create_dir_all(dir)
        .map_err(|_| PluginError::io("cannot create the Devin credentials directory"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| PluginError::io("cannot restrict the Devin credentials directory"))?;
    }
    Ok(())
}

fn write_private(path: &Path, text: &str) -> Result<(), PluginError> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let failed = |_| PluginError::io("cannot write the Devin credentials file");
    let mut file = options.open(path).map_err(failed)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(failed)?;
    }
    file.write_all(text.as_bytes()).map_err(failed)?;
    file.sync_all().map_err(failed)
}

fn write_atomic(path: &Path, text: &[u8]) -> Result<(), PluginError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        private_dir(parent)?;
    }
    let mut temporary_name = path
        .file_name()
        .map_or_else(|| std::ffi::OsString::from("devin.tmp"), ToOwned::to_owned);
    temporary_name.push(".tmp");
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let temporary = parent.join(temporary_name);
    write_private(
        &temporary,
        std::str::from_utf8(text)
            .map_err(|_| PluginError::io("Devin credentials are not valid UTF-8"))?,
    )?;
    std::fs::rename(&temporary, path)
        .map_err(|_| PluginError::io("cannot write the Devin credentials file"))
}
