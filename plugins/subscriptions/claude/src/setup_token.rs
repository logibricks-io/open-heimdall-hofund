//! Inference-only accounts from a pasted `claude setup-token` value.

use hofund_plugin_api::{AccountId, HostContext, PluginError};
use hofund_plugin_sdk::digest::sha256;
use hofund_plugin_sdk::oauth::{SecretObject, account_id_from, constant_time_eq};

use crate::account;
use crate::creds;

/// Secret field holding the setup token (never logged).
pub(crate) const SETUP_TOKEN: &str = "setupToken";

/// Why agent sign-in cannot switch to a setup-token account.
pub(crate) const AGENT_SWITCH: &str = "this account was added by a setup-token: hofund serves requests with it, but Claude Code itself can't be signed in to it";

/// Validates and normalizes a pasted setup token.
pub(crate) fn normalize(token: &str) -> Result<String, PluginError> {
    let mut token = token.trim();
    token = token
        .strip_prefix("CLAUDE_CODE_OAUTH_TOKEN=")
        .unwrap_or(token);
    token = token.trim().trim_matches('"').trim_matches('\'');
    if token.is_empty() {
        return Err(PluginError::config(
            "paste the token `claude setup-token` printed",
        ));
    }
    if token.starts_with("sk-ant-api") {
        return Err(PluginError::config(
            "this is an Anthropic API key, not a setup-token: add it as an API key provider instead",
        ));
    }
    if !token.starts_with("sk-ant-oat") {
        return Err(PluginError::config(
            "this isn't a setup-token: `claude setup-token` prints one that starts with sk-ant-oat",
        ));
    }
    if token.len() > 512 || token.bytes().any(|b| b.is_ascii_whitespace() || b > 127) {
        return Err(PluginError::config(
            "this isn't a setup-token: it has spaces or other characters a token doesn't",
        ));
    }
    Ok(token.to_owned())
}

pub(crate) fn is_setup_token(blob: &SecretObject) -> bool {
    blob.text(SETUP_TOKEN).is_some()
}

pub(crate) fn token_from_blob(blob: &SecretObject) -> Option<String> {
    blob.text(SETUP_TOKEN).map(str::to_owned)
}

pub(crate) fn account_id(token: &str) -> AccountId {
    account_id_from(&["claude-setup", &version(token)])
}

pub(crate) fn label(token: &str) -> String {
    format!("Claude setup-token {}", short_hash(token))
}

pub(crate) fn version(token: &str) -> String {
    format!("setup-token:{}", &hex_hash(token)[..16])
}

pub(crate) fn blob(token: &str) -> SecretObject {
    SecretObject::new()
        .with_number(creds::VERSION, 1)
        .with_text(SETUP_TOKEN, token)
}

/// Stores a new setup-token account, or returns the existing id when the token matches.
pub(crate) async fn store(host: &HostContext, token: &str) -> Result<AccountId, PluginError> {
    let token = normalize(token)?;
    for account in host.accounts().list().await? {
        let Some(secret) = host.accounts().secret(account.id()).await? else {
            continue;
        };
        let Ok(blob) = SecretObject::parse(&secret) else {
            continue;
        };
        if let Some(stored) = token_from_blob(&blob)
            && constant_time_eq(stored.as_bytes(), token.as_bytes())
        {
            return Ok(account.id().clone());
        }
    }
    let id = account_id(&token);
    let dir = account::account_dir(host, &id)?;
    creds::private_dir(&dir)?;
    let creds_path = dir.join(creds::CREDENTIALS_FILE);
    if creds_path.exists() {
        std::fs::remove_file(&creds_path).map_err(|_| {
            PluginError::io("cannot clear a sign-in from the setup-token directory")
        })?;
    }
    creds::write_onboarded(&dir)?;
    let account = host.accounts().account(id.clone(), label(&token))?;
    host.accounts()
        .upsert(account, blob(&token).to_secret())
        .await?;
    Ok(id)
}

fn hex_hash(token: &str) -> String {
    sha256(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn short_hash(token: &str) -> String {
    hex_hash(token)[..6].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r5_5_setup_token_prefix_and_shape() {
        assert!(normalize("sk-ant-oat01-abc").is_ok());
        assert!(normalize("sk-ant-api03-key").is_err());
        assert!(normalize("not-a-token").is_err());
        assert!(normalize("sk-ant-oat01 has-space").is_err());
    }

    #[test]
    fn r8_6_api_key_named_and_length_capped() {
        let error = normalize("sk-ant-api03-key").expect_err("api key refused");
        assert!(error.message().contains("API key"), "{error}");
        let at_cap = format!("sk-ant-oat01-{}", "a".repeat(512 - 13));
        assert_eq!(at_cap.len(), 512);
        assert!(normalize(&at_cap).is_ok());
        let over = format!("{at_cap}a");
        assert!(normalize(&over).is_err());
    }

    #[test]
    fn r5_6_setup_token_strips_env_prefix() {
        let token = normalize("CLAUDE_CODE_OAUTH_TOKEN=sk-ant-oat01-abc").expect("token");
        assert_eq!(token, "sk-ant-oat01-abc");
    }

    #[test]
    fn r5_38_setup_token_fingerprint_matches_sha256() {
        let token = "sk-ant-oat01-abc";
        assert_eq!(
            hex_hash(token),
            "e4eed51bd6b8bc08df488bbccc92d40abff7ed1d248836a9f0321a1b702583b0"
        );
        assert_eq!(short_hash(token), "e4eed5");
        assert_eq!(version(token), "setup-token:e4eed51bd6b8bc08");
    }
}
