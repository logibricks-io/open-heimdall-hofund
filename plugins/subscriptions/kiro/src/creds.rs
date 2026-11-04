//! Kiro IDE / CLI shaped token files and stored account secrets.

use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::oauth::{SecretObject, account_id_from};
use hofund_plugin_sdk::time::parse_rfc3339;

pub(crate) const REFRESH_MARGIN: Duration = Duration::from_secs(120);
const FILE_CAP: u64 = 64 * 1024;
pub(crate) const TOKEN_FILE: &str = "kiro-auth-token.json";

#[derive(Clone, Debug)]
pub(crate) struct Credential {
    pub access: SecretString,
    pub refresh: SecretString,
    pub expires_at: Option<std::time::SystemTime>,
    pub region: String,
    pub profile_arn: String,
    pub label: String,
}

#[derive(Clone, Debug)]
pub(crate) struct Stored {
    pub credential: Credential,
}

impl Stored {
    pub(crate) async fn load(host: &HostContext, account: &AccountId) -> Result<Self, PluginError> {
        let Some(secret) = host.accounts().secret(account).await? else {
            return Err(PluginError::auth(
                "no Kiro sign-in is stored for this account",
            ));
        };
        parse_secret(&secret)
    }

    pub(crate) fn fresh_at(&self, now: std::time::SystemTime) -> bool {
        match self.credential.expires_at {
            None => true,
            Some(at) => at > now + REFRESH_MARGIN,
        }
    }
}

pub(crate) fn parse_secret(secret: &SecretString) -> Result<Stored, PluginError> {
    let blob = SecretObject::parse(secret)?;
    let access = blob
        .secret("access_token")
        .ok_or_else(|| PluginError::auth("the stored Kiro sign-in has no access token"))?;
    let refresh = blob
        .secret("refresh_token")
        .unwrap_or_else(|| SecretString::from(String::new()));
    let region = blob
        .text("region")
        .filter(|value| !value.is_empty())
        .unwrap_or("us-east-1")
        .to_owned();
    let profile_arn = blob.text("profile_arn").unwrap_or("").to_owned();
    let label = blob
        .text("label")
        .filter(|value| !value.is_empty())
        .unwrap_or("Kiro")
        .to_owned();
    let expires_at = blob
        .number("expires_at")
        .and_then(|secs| UNIX_EPOCH.checked_add(Duration::from_secs(secs)));
    Ok(Stored {
        credential: Credential {
            access,
            refresh,
            expires_at,
            region,
            profile_arn,
            label,
        },
    })
}

pub(crate) fn secret_blob(credential: &Credential) -> SecretObject {
    let expires = credential
        .expires_at
        .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    SecretObject::new()
        .with_number("v", 1)
        .with_secret("access_token", &credential.access)
        .with_secret("refresh_token", &credential.refresh)
        .with_text("region", &credential.region)
        .with_text("profile_arn", &credential.profile_arn)
        .with_text("label", &credential.label)
        .with_number("expires_at", expires)
}

pub(crate) fn account_id(credential: &Credential) -> AccountId {
    account_id_from(&["kiro", &credential.label])
}

pub(crate) fn read_token_file(path: &Path) -> Result<Credential, PluginError> {
    let meta = std::fs::metadata(path)
        .map_err(|_| PluginError::auth("Kiro has no token file at that path"))?;
    if !meta.is_file() || meta.len() > FILE_CAP {
        return Err(PluginError::auth("Kiro token file is missing or too large"));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|_| PluginError::auth("Kiro token file could not be read"))?;
    parse_token_json(&text)
}

pub(crate) fn parse_token_json(text: &str) -> Result<Credential, PluginError> {
    let root: Value = hofund_plugin_sdk::json::from_str(text)
        .map_err(|_| PluginError::decode("Kiro token file is not JSON"))?;
    let access = field_secret(&root, &["accessToken", "access_token"])?;
    let refresh = field_secret(&root, &["refreshToken", "refresh_token"])
        .unwrap_or_else(|_| SecretString::from(String::new()));
    let region = root
        .get("region")
        .and_then(Value::as_str)
        .unwrap_or("us-east-1")
        .to_owned();
    let profile_arn = root
        .get("profileArn")
        .or_else(|| root.get("profile_arn"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let label = root
        .get("email")
        .or_else(|| root.get("userEmail"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| "Kiro".to_owned());
    let expires_at = root
        .get("expiresAt")
        .or_else(|| root.get("expires_at"))
        .and_then(Value::as_str)
        .and_then(parse_rfc3339);
    Ok(Credential {
        access,
        refresh,
        expires_at,
        region,
        profile_arn,
        label,
    })
}

fn field_secret(root: &Value, keys: &[&str]) -> Result<SecretString, PluginError> {
    for key in keys {
        if let Some(text) = root
            .get(*key)
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
        {
            return Ok(SecretString::from(text.to_owned()));
        }
    }
    Err(PluginError::auth("Kiro token file has no access token"))
}

pub(crate) fn default_token_path(home: &Path) -> PathBuf {
    if home.join(TOKEN_FILE).is_file() {
        return home.join(TOKEN_FILE);
    }
    home.join(".aws").join("sso").join("cache").join(TOKEN_FILE)
}

pub(crate) fn write_token_file(path: &Path, credential: &Credential) -> Result<(), PluginError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|_| PluginError::io("cannot create the Kiro token directory"))?;
    }
    let expires = credential
        .expires_at
        .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
        .map(|d| hofund_plugin_sdk::time::format_rfc3339(UNIX_EPOCH + d))
        .unwrap_or_default();
    let body = format!(
        "{{\"accessToken\":\"{}\",\"refreshToken\":\"{}\",\"expiresAt\":\"{}\",\"region\":\"{}\",\"profileArn\":\"{}\"}}\n",
        escape_json(hofund_plugin_sdk::oauth::secret_text(&credential.access)),
        escape_json(hofund_plugin_sdk::oauth::secret_text(&credential.refresh)),
        escape_json(&expires),
        escape_json(&credential.region),
        escape_json(&credential.profile_arn),
    );
    std::fs::write(path, body).map_err(|_| PluginError::io("cannot write Kiro token file"))?;
    Ok(())
}

fn escape_json(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}
