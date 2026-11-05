//! Writing Codex CLI's sign-in from a stored account.

use std::path::Path;

use hofund_plugin_api::{AccountId, HostContext, PluginError};
use hofund_plugin_sdk::oauth::{SecretObject, secret_text};

use crate::store::{AuthPatch, StoreError, StoreReader, merge_auth_tokens};

/// Writes Codex's credential store from the stored account.
pub(crate) async fn write(
    account: &AccountId,
    home: &Path,
    host: &HostContext,
    skip_keychain: bool,
) -> Result<(), PluginError> {
    let Some(secret) = host.accounts().secret(account).await? else {
        return Err(PluginError::auth(
            "no ChatGPT sign-in is stored for this account",
        ));
    };
    let blob = SecretObject::parse(&secret)?;
    if blob.flag(crate::tokens::LAPSED) {
        return Err(PluginError::auth(crate::tokens::LAPSED_REASON));
    }
    let access = blob
        .secret("access_token")
        .ok_or_else(|| PluginError::auth("the stored ChatGPT sign-in has no access token"))?;
    let refresh = blob.secret("refresh_token");
    let id_token = blob.secret("id_token");
    let reader = StoreReader::new(Some(host), skip_keychain, None);
    let base = match reader.read_login(home, false).await {
        Ok((bytes, _)) => bytes,
        Err(StoreError::SignedOut) => Vec::new(),
        Err(StoreError::InvalidJson) => Vec::new(),
        Err(StoreError::Unreadable { .. }) => {
            return Err(PluginError::auth(
                "Codex keeps its sign-in where it cannot be written from here",
            ));
        }
        Err(StoreError::ConfigParse(message)) => return Err(PluginError::config(message)),
        Err(StoreError::FileWrite | StoreError::KeychainWrite) => {
            return Err(PluginError::io("cannot read Codex sign-in before writing"));
        }
    };
    let patch = AuthPatch {
        access_token: Some(secret_text(&access).to_owned()),
        refresh_token: refresh.map(|token| secret_text(&token).to_owned()),
        id_token: id_token.map(|token| secret_text(&token).to_owned()),
        chatgpt_mode: true,
    };
    let merged = if base.is_empty() {
        merge_auth_tokens(br#"{"auth_mode":"chatgpt","tokens":{}}"#, &patch)?
    } else {
        merge_auth_tokens(&base, &patch)?
    };
    reader.save_login(home, &merged).await.map_err(map_store)?;
    let updated = blob.with_text("codex_home", &home.to_string_lossy());
    if let Ok(rows) = host.accounts().list().await
        && let Some(row) = rows.iter().find(|row| row.id() == account)
    {
        let account_row = host.accounts().account(account.clone(), row.label())?;
        let _ = host
            .accounts()
            .upsert(account_row, updated.to_secret())
            .await;
    }
    Ok(())
}

fn map_store(error: StoreError) -> PluginError {
    match error {
        StoreError::SignedOut => PluginError::auth("Codex is signed out in that directory"),
        StoreError::Unreadable { .. } => {
            PluginError::auth("Codex keeps its sign-in where it cannot be written from here")
        }
        StoreError::ConfigParse(message) => PluginError::config(message),
        StoreError::FileWrite | StoreError::KeychainWrite => {
            PluginError::io("cannot write Codex sign-in")
        }
        StoreError::InvalidJson => PluginError::decode("Codex sign-in is not JSON"),
    }
}

/// Writes rotated tokens back to Codex's store for `home`.
pub(crate) async fn persist_refresh(
    home: &Path,
    host: &HostContext,
    skip_keychain: bool,
    blob: &SecretObject,
) {
    let patch = AuthPatch {
        access_token: blob
            .secret("access_token")
            .map(|token| secret_text(&token).to_owned()),
        refresh_token: blob
            .secret("refresh_token")
            .map(|token| secret_text(&token).to_owned()),
        id_token: blob
            .secret("id_token")
            .map(|token| secret_text(&token).to_owned()),
        chatgpt_mode: true,
    };
    let reader = StoreReader::new(Some(host), skip_keychain, None);
    let Ok((base, _)) = reader.read_login(home, true).await else {
        return;
    };
    let Ok(merged) = merge_auth_tokens(&base, &patch) else {
        return;
    };
    let _ = reader.save_login(home, &merged).await;
}
