//! Writing Codex CLI's sign-in from a stored account.

use std::path::Path;

use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::oauth::{SecretObject, same_secret, secret_text};

use crate::codex_lock;
use crate::store::{AuthPatch, StoreError, StoreReader, is_valid_auth_json, merge_auth_tokens};

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
    let base = match reader.read_login(home, true).await {
        Ok((bytes, _)) => bytes,
        Err(StoreError::SignedOut) => Vec::new(),
        Err(StoreError::InvalidJson) => {
            return Err(PluginError::decode(
                "Codex sign-in on disk is not JSON; nothing was changed",
            ));
        }
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
    if !base.is_empty() && !is_valid_auth_json(&base) {
        return Err(PluginError::decode(
            "Codex sign-in on disk is not JSON; nothing was changed",
        ));
    }
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

/// Writes rotated tokens back to Codex's store for `home` when the on-disk access token still matches `stale`.
pub(crate) async fn persist_refresh(
    home: &Path,
    host: &HostContext,
    skip_keychain: bool,
    blob: &SecretObject,
    stale: &SecretString,
) {
    let _lock = match codex_lock::acquire(home).await {
        Ok(lock) => lock,
        Err(_) => return,
    };
    let reader = StoreReader::new(Some(host), skip_keychain, None);
    let Ok((base, _)) = reader.read_login(home, true).await else {
        return;
    };
    let Some(current_access) = access_token_from_auth(&base) else {
        return;
    };
    if !same_secret(&current_access, stale) {
        return;
    }
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
    let Ok(merged) = merge_auth_tokens(&base, &patch) else {
        return;
    };
    let _ = reader.save_login(home, &merged).await;
}

fn access_token_from_auth(bytes: &[u8]) -> Option<SecretString> {
    let value =
        hofund_plugin_sdk::json::from_slice::<hofund_plugin_sdk::json::Value>(bytes).ok()?;
    let token = value.get("tokens")?.get("access_token")?.as_str()?;
    Some(SecretString::from(token.to_owned()))
}
