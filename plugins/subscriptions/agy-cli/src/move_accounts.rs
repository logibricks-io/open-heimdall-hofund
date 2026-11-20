//! Moving saved Antigravity accounts into per-account agy homes.
//!
//! The source accounts arrive as the import text. This plugin cannot read
//! another subscription's secret store. Old sign-ins are not deleted.

use std::collections::HashSet;
use std::fmt;
use std::path::Path;

use hofund_plugin_api::{PluginError, SecretString};
use hofund_plugin_sdk::json::{self, Value};
use hofund_plugin_sdk::oauth::{SecretObject, secret_text};

pub(crate) struct MoveRow {
    pub(crate) key: String,
    pub(crate) email: String,
    pub(crate) refresh: SecretString,
    pub(crate) proxy: String,
}

impl fmt::Debug for MoveRow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MoveRow")
            .field("key", &self.key)
            .field("email", &self.email)
            .field("refresh", &"<redacted>")
            .field("proxy", &self.proxy)
            .finish()
    }
}

pub(crate) struct MovePlan {
    pub(crate) rows: Vec<MoveRow>,
    pub(crate) source_proxy: String,
    pub(crate) agy_proxy: String,
}

/// Bare keys (no `#`) come first, then lexicographic order.
/// Duplicate refresh-token prefixes and emails are dropped. Emails already in
/// `have` (compared case-insensitively) are skipped.
#[must_use]
pub(crate) fn select_moves(mut rows: Vec<MoveRow>, have: &HashSet<String>) -> Vec<MoveRow> {
    rows.sort_by(|left, right| {
        left.key
            .contains('#')
            .cmp(&right.key.contains('#'))
            .then_with(|| left.key.cmp(&right.key))
    });
    let mut seen_refresh = HashSet::new();
    let mut seen_email = HashSet::new();
    let mut out = Vec::new();
    for row in rows {
        let email = row.email.to_ascii_lowercase();
        if email.is_empty() || have.contains(&email) || seen_email.contains(&email) {
            continue;
        }
        let prefix = refresh_prefix(&row.refresh);
        if prefix.is_empty() || seen_refresh.contains(&prefix) {
            continue;
        }
        seen_email.insert(email);
        seen_refresh.insert(prefix);
        out.push(row);
    }
    out
}

fn refresh_prefix(refresh: &SecretString) -> String {
    secret_text(refresh)
        .split('|')
        .next()
        .unwrap_or("")
        .trim()
        .to_owned()
}

/// Copies the source provider proxy onto agy when agy has no accounts and no
/// proxy of its own. A proxy the user already set, including `direct`, stays.
#[must_use]
pub(crate) fn provider_after_carry(
    account_count: usize,
    agy_proxy: &str,
    source_proxy: &str,
) -> String {
    if account_count == 0 && agy_proxy.is_empty() {
        source_proxy.to_owned()
    } else {
        agy_proxy.to_owned()
    }
}

/// Stores an account proxy when it differs from the provider proxy agy will use.
#[must_use]
pub(crate) fn account_proxy_to_store(
    provider_proxy: &str,
    source_proxy: &str,
    account_proxy: &str,
) -> String {
    let chosen = if account_proxy.is_empty() {
        source_proxy
    } else {
        account_proxy
    };
    if chosen.is_empty() || chosen == provider_proxy {
        String::new()
    } else {
        chosen.to_owned()
    }
}

pub(crate) fn parse_plan(text: &str) -> Result<MovePlan, PluginError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text).trim();
    if text.is_empty() {
        return Err(PluginError::config("nothing to import"));
    }
    let value = json::from_str::<Value>(text)
        .map_err(|_| PluginError::config("agy-cli import is not an account list"))?;
    let (accounts, source_proxy, agy_proxy, account_proxies) = match value {
        Value::Array(accounts) => (accounts, String::new(), String::new(), Value::Null),
        Value::Object(object) => {
            let accounts = match object.get("accounts") {
                Some(Value::Array(accounts)) => accounts.clone(),
                _ => {
                    return Err(PluginError::config("agy-cli import is not an account list"));
                }
            };
            let from = object.get("from_proxy");
            let source_proxy = from
                .and_then(|from| from.get("provider"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_owned();
            let agy_proxy = object
                .get("agy_proxy")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_owned();
            let account_proxies = from
                .and_then(|from| from.get("accounts"))
                .cloned()
                .unwrap_or(Value::Null);
            (accounts, source_proxy, agy_proxy, account_proxies)
        }
        _ => return Err(PluginError::config("agy-cli import is not an account list")),
    };
    let mut rows = Vec::new();
    for account in accounts {
        if let Some(row) = row_from(&account, &account_proxies) {
            rows.push(row);
        }
    }
    Ok(MovePlan {
        rows,
        source_proxy,
        agy_proxy,
    })
}

fn row_from(account: &Value, account_proxies: &Value) -> Option<MoveRow> {
    let email = email_of(account)?;
    let refresh = account
        .get("refresh")
        .and_then(Value::as_str)
        .filter(|refresh| !refresh.is_empty())
        .or_else(|| {
            account
                .get("refresh_token")
                .and_then(Value::as_str)
                .filter(|refresh| !refresh.is_empty())
        })?;
    let key = account
        .get("key")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let own = account
        .get("proxy")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    let mapped = proxy_for(account_proxies, &email);
    let proxy = if own.is_empty() { mapped } else { own };
    Some(MoveRow {
        key,
        email,
        refresh: SecretString::from(refresh.to_owned()),
        proxy,
    })
}

fn email_of(account: &Value) -> Option<String> {
    let text = account
        .get("accountId")
        .and_then(Value::as_str)
        .filter(|email| !email.is_empty())
        .or_else(|| {
            account
                .get("account_id")
                .and_then(Value::as_str)
                .filter(|email| !email.is_empty())
        })
        .or_else(|| {
            account
                .pointer("/metadata/email")
                .and_then(Value::as_str)
                .filter(|email| !email.is_empty())
        })
        .or_else(|| {
            account
                .get("email")
                .and_then(Value::as_str)
                .filter(|email| !email.is_empty())
        })?;
    Some(text.trim().to_ascii_lowercase())
}

fn proxy_for(accounts: &Value, email: &str) -> String {
    let Some(object) = accounts.as_object() else {
        return String::new();
    };
    let Some((_, value)) = object
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(email))
    else {
        return String::new();
    };
    match value {
        Value::String(proxy) => proxy.trim().to_owned(),
        Value::Object(object) => object
            .get("proxy")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned(),
        _ => String::new(),
    }
}

pub(crate) fn write_sign_in(
    home: &Path,
    email: &str,
    refresh: &SecretString,
) -> Result<(), PluginError> {
    std::fs::create_dir_all(home)
        .map_err(|error| PluginError::io(format!("cannot create the agy home ({error})")))?;
    let body = SecretObject::new()
        .with_text("email", email)
        .with_secret("refresh_token", refresh)
        .to_secret();
    write_private(&home.join("sign-in.json"), secret_text(&body))
}

fn write_private(path: &Path, text: &str) -> Result<(), PluginError> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .map_err(|error| PluginError::io(format!("cannot write the agy sign-in ({error})")))?;
        file.write_all(text.as_bytes())
            .map_err(|error| PluginError::io(format!("cannot write the agy sign-in ({error})")))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, text)
            .map_err(|error| PluginError::io(format!("cannot write the agy sign-in ({error})")))
    }
}
