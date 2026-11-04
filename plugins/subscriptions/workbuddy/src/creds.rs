//! WorkBuddy desktop auth and stored secrets.

use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::json::{self, Value};
use hofund_plugin_sdk::oauth::{SecretObject, account_id_from, secret_text};

use crate::auth_store;
use crate::site::Site;

#[derive(Clone, Debug)]
pub(crate) struct Credential {
    pub uid: String,
    pub access: SecretString,
    pub refresh: SecretString,
    pub expires_at: i64,
    pub refresh_expires_at: i64,
    pub domain: String,
    pub token_type: String,
    pub label: String,
    pub site: Site,
    pub desktop: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct Stored {
    pub credential: Credential,
}

impl Stored {
    pub(crate) async fn load(host: &HostContext, account: &AccountId) -> Result<Self, PluginError> {
        let Some(secret) = host.accounts().secret(account).await? else {
            return Err(PluginError::auth(
                "no WorkBuddy sign-in is stored for this account",
            ));
        };
        let mut stored = parse_secret(&secret)?;
        if stored.credential.desktop {
            stored.credential = hydrate_desktop(&stored.credential)?;
        }
        Ok(stored)
    }

    pub(crate) fn fresh_at(&self, now: std::time::SystemTime) -> bool {
        if self.credential.expires_at <= 0 {
            return !self.credential.desktop;
        }
        let Ok(now_ms) = now.duration_since(std::time::UNIX_EPOCH) else {
            return false;
        };
        now_ms.as_millis() as i64 + 60_000 < self.credential.expires_at
    }
}

fn hydrate_desktop(stored: &Credential) -> Result<Credential, PluginError> {
    let path = auth_store::auth_path(stored.site)?;
    let text = std::fs::read_to_string(&path).map_err(|error| {
        PluginError::auth(format!(
            "WorkBuddy desktop auth at {} can't be read ({error})",
            path.display()
        ))
    })?;
    let mut fresh = parse_desktop_info(&text, stored.site)?;
    fresh.label = stored.label.clone();
    Ok(fresh)
}

pub(crate) fn parse_secret(secret: &SecretString) -> Result<Stored, PluginError> {
    let blob = SecretObject::parse(secret)?;
    let uid = blob
        .text("uid")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| PluginError::auth("the stored WorkBuddy sign-in has no uid"))?;
    let access = blob
        .secret("access_token")
        .ok_or_else(|| PluginError::auth("the stored WorkBuddy sign-in has no access token"))?;
    let site = match blob.text("site").unwrap_or("workbuddy") {
        "workbuddy-ai" => Site::International,
        _ => Site::China,
    };
    Ok(Stored {
        credential: Credential {
            uid: uid.to_owned(),
            access,
            refresh: blob
                .secret("refresh_token")
                .unwrap_or_else(|| SecretString::from(String::new())),
            expires_at: blob.number("expires_at").unwrap_or(0) as i64,
            refresh_expires_at: blob.number("refresh_expires_at").unwrap_or(0) as i64,
            domain: blob.text("domain").unwrap_or("").to_owned(),
            token_type: blob.text("token_type").unwrap_or("Bearer").to_owned(),
            label: blob.text("label").unwrap_or(uid).to_owned(),
            site,
            desktop: blob.flag("desktop"),
        },
    })
}

pub(crate) fn secret_blob(credential: &Credential) -> SecretObject {
    SecretObject::new()
        .with_number("v", 1)
        .with_text("uid", &credential.uid)
        .with_secret("access_token", &credential.access)
        .with_secret("refresh_token", &credential.refresh)
        .with_number("expires_at", credential.expires_at.max(0) as u64)
        .with_number(
            "refresh_expires_at",
            credential.refresh_expires_at.max(0) as u64,
        )
        .with_text("domain", &credential.domain)
        .with_text("token_type", &credential.token_type)
        .with_text("label", &credential.label)
        .with_text("site", credential.site.plugin_id())
        .with_flag("desktop", credential.desktop)
}

pub(crate) fn account_id(credential: &Credential) -> AccountId {
    account_id_from(&[credential.site.plugin_id(), &credential.uid])
}

pub(crate) fn parse_desktop_info(text: &str, site: Site) -> Result<Credential, PluginError> {
    let root: Value = json::from_str(text.trim())
        .map_err(|_| PluginError::auth("WorkBuddy auth file is invalid JSON"))?;
    let account = root.get("account").and_then(Value::as_object);
    let auth = root.get("auth").and_then(Value::as_object);
    let uid = account
        .and_then(|map| map.get("uid"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| PluginError::auth("WorkBuddy auth file has no account uid"))?;
    let access = auth
        .and_then(|map| map.get("accessToken"))
        .and_then(string_value)
        .ok_or_else(|| PluginError::auth("WorkBuddy auth file has no access token"))?;
    if access.contains("$wbEncrypted") {
        return Err(PluginError::auth(
            "WorkBuddy desktop keeps its sign-in encrypted; use browser sign-in instead",
        ));
    }
    let refresh = auth
        .and_then(|map| map.get("refreshToken"))
        .and_then(string_value)
        .unwrap_or_default();
    let expires_at = auth
        .and_then(|map| map.get("expiresAt"))
        .and_then(json_i64)
        .unwrap_or(0);
    let refresh_expires_at = auth
        .and_then(|map| map.get("refreshExpiresAt"))
        .and_then(json_i64)
        .unwrap_or(0);
    let domain = auth
        .and_then(|map| map.get("domain"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let site = if domain.is_empty() {
        site
    } else {
        Site::from_domain(&domain)
    };
    let label = account
        .and_then(|map| map.get("nickname"))
        .and_then(string_value)
        .unwrap_or_else(|| uid.to_owned());
    Ok(Credential {
        uid: uid.to_owned(),
        access: SecretString::from(access),
        refresh: SecretString::from(refresh),
        expires_at,
        refresh_expires_at,
        domain,
        token_type: auth
            .and_then(|map| map.get("tokenType"))
            .and_then(Value::as_str)
            .unwrap_or("Bearer")
            .to_owned(),
        label,
        site,
        desktop: true,
    })
}

fn string_value(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn json_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().map(|n| n as i64))
        .or_else(|| value.as_f64().map(|n| n as i64))
}

pub(crate) fn bearer_headers(credential: &Credential) -> Vec<(String, String)> {
    vec![
        (
            "Authorization".to_owned(),
            format!("Bearer {}", secret_text(&credential.access)),
        ),
        ("X-User-Id".to_owned(), credential.uid.clone()),
        (
            "User-Agent".to_owned(),
            credential.site.user_agent().to_owned(),
        ),
    ]
}
