//! Kiro API key (`ksk_…`) sign-in via [`SignInProof::Token`].

use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::oauth::{account_id_from, secret_text};

use crate::creds::{self, Credential};

/// Whether `key` matches the reference API-key prefix rule.
pub(crate) fn validate_format(key: &str) -> Result<(), PluginError> {
    let trimmed = key.trim();
    if trimmed.starts_with("ksk_") && trimmed.len() > "ksk_".len() {
        Ok(())
    } else {
        Err(PluginError::auth("Kiro API keys start with ksk_"))
    }
}

/// Stores a pasted API key as a Kiro account (no live validation call).
pub(crate) async fn sign_in_with_key(
    host: &HostContext,
    key: SecretString,
) -> Result<AccountId, PluginError> {
    let text = secret_text(&key).trim().to_owned();
    if text.is_empty() {
        return Err(PluginError::auth("enter your Kiro API key"));
    }
    validate_format(&text)?;
    let key = SecretString::from(text.clone());
    let credential = Credential {
        access: key,
        refresh: SecretString::from(String::new()),
        expires_at: None,
        region: "us-east-1".to_owned(),
        profile_arn: String::new(),
        label: "Kiro API key".to_owned(),
        method: "apikey".to_owned(),
        provider: String::new(),
        client_id: String::new(),
        client_secret: SecretString::from(String::new()),
        client_id_hash: String::new(),
        token_url: String::new(),
        ide_token_path: String::new(),
        db_key: String::new(),
        cli_db_path: String::new(),
    };
    let id = api_key_account_id(&text);
    let account = host
        .accounts()
        .account(id.clone(), credential.label.clone())?;
    host.accounts()
        .upsert(account, creds::secret_blob(&credential).to_secret())
        .await?;
    Ok(id)
}

fn api_key_account_id(key: &str) -> AccountId {
    let digest = hofund_plugin_sdk::digest::sha256(key.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    account_id_from(&["kiro", "api", &digest])
}
