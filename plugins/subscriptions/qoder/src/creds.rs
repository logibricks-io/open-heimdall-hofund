//! Stored Qoder credentials and site selection.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::json::{self, Value};
use hofund_plugin_sdk::oauth::{SecretObject, account_id_from};

pub(crate) const REFRESH_MARGIN: Duration = Duration::from_secs(5 * 60);
pub(crate) const CN_SITE: &str = "qoder-cn";

/// Which Qoder deployment an account uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Site {
    Global,
    China,
}

impl Site {
    pub(crate) fn from_stored(text: &str) -> Self {
        if text == CN_SITE {
            Self::China
        } else {
            Self::Global
        }
    }

    pub(crate) const fn plugin_id(self) -> &'static str {
        match self {
            Self::Global => "qoder",
            Self::China => CN_SITE,
        }
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Global => "",
            Self::China => CN_SITE,
        }
    }
}

/// One Qoder account as kept in the auth blob.
#[derive(Clone, Debug)]
pub(crate) struct Credential {
    pub uid: String,
    pub email: String,
    pub name: String,
    pub token: SecretString,
    pub refresh_token: SecretString,
    pub expires_at: i64,
    pub machine_id: String,
    pub site: Site,
}

#[derive(Clone, Debug)]
pub(crate) struct Stored {
    pub credential: Credential,
}

impl Stored {
    pub(crate) async fn load(host: &HostContext, account: &AccountId) -> Result<Self, PluginError> {
        let Some(secret) = host.accounts().secret(account).await? else {
            return Err(PluginError::auth(
                "no Qoder sign-in is stored for this account",
            ));
        };
        parse_secret(&secret)
    }

    pub(crate) fn fresh_at(&self, now: SystemTime) -> bool {
        if self.credential.expires_at <= 0 {
            return true;
        }
        let Ok(now_ms) = now.duration_since(UNIX_EPOCH) else {
            return false;
        };
        let now_ms = now_ms.as_millis() as i64;
        self.credential.expires_at.saturating_sub(now_ms) > REFRESH_MARGIN.as_millis() as i64
    }

    pub(crate) fn ready_at(&self, now: SystemTime) -> bool {
        if self.credential.expires_at <= 0 {
            return true;
        }
        let Ok(now_ms) = now.duration_since(UNIX_EPOCH) else {
            return false;
        };
        self.credential.expires_at > now_ms.as_millis() as i64
    }
}

pub(crate) fn parse_secret(secret: &SecretString) -> Result<Stored, PluginError> {
    let blob = SecretObject::parse(secret)?;
    let uid = blob
        .text("uid")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| PluginError::auth("the stored Qoder sign-in has no uid"))?
        .to_owned();
    let token = blob
        .secret("access_token")
        .ok_or_else(|| PluginError::auth("the stored Qoder sign-in has no access token"))?;
    let refresh = blob
        .secret("refresh_token")
        .ok_or_else(|| PluginError::auth("the stored Qoder sign-in has no refresh token"))?;
    let site = Site::from_stored(blob.text("site").unwrap_or(""));
    Ok(Stored {
        credential: Credential {
            uid,
            email: blob.text("email").unwrap_or("").to_owned(),
            name: blob.text("name").unwrap_or("").to_owned(),
            token,
            refresh_token: refresh,
            expires_at: blob.number("expires_at").unwrap_or(0) as i64,
            machine_id: blob.text("machine_id").unwrap_or("").to_owned(),
            site,
        },
    })
}

pub(crate) fn secret_blob(credential: &Credential) -> SecretObject {
    SecretObject::new()
        .with_number("v", 1)
        .with_text("uid", &credential.uid)
        .with_text("email", &credential.email)
        .with_text("name", &credential.name)
        .with_secret("access_token", &credential.token)
        .with_secret("refresh_token", &credential.refresh_token)
        .with_number("expires_at", credential.expires_at.max(0) as u64)
        .with_text("machine_id", &credential.machine_id)
        .with_text("site", credential.site.as_str())
}

pub(crate) fn account_id(credential: &Credential) -> AccountId {
    account_id_from(&[credential.site.plugin_id(), &credential.uid])
}

pub(crate) fn label(credential: &Credential) -> String {
    if !credential.email.is_empty() {
        credential.email.clone()
    } else if !credential.name.is_empty() {
        credential.name.clone()
    } else {
        credential.uid.clone()
    }
}

pub(crate) fn parse_json(text: &str) -> Result<Credential, PluginError> {
    let root: Value =
        json::from_str(text.trim()).map_err(|_| PluginError::auth("credential JSON is invalid"))?;
    let map = root
        .as_object()
        .ok_or_else(|| PluginError::auth("credential JSON must be an object"))?;
    let uid = str_field(map, "uid")?;
    let token = str_field(map, "token")?;
    let refresh = map
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| PluginError::auth("credential JSON has no refresh_token"))?;
    let site = map
        .get("site")
        .and_then(Value::as_str)
        .map(Site::from_stored)
        .unwrap_or(Site::Global);
    Ok(Credential {
        uid,
        email: optional_str(map, "email"),
        name: optional_str(map, "name"),
        token: SecretString::from(token.to_owned()),
        refresh_token: SecretString::from(refresh.to_owned()),
        expires_at: map.get("expires_at").and_then(json_number_i64).unwrap_or(0),
        machine_id: optional_str(map, "machine_id"),
        site,
    })
}

fn str_field(map: &json::Map<String, Value>, key: &str) -> Result<String, PluginError> {
    map.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| PluginError::auth(format!("credential JSON has no {key}")))
}

fn optional_str(map: &json::Map<String, Value>, key: &str) -> String {
    map.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

fn json_number_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().map(|n| n as i64))
        .or_else(|| value.as_f64().map(|n| n as i64))
}
